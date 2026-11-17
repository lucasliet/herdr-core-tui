use std::path::PathBuf;

use crate::app::{App, Mode};
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::popup_size::{resolve_popup_geometry, PopupSize};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PopupGeometry {
    pub width: Option<PopupSize>,
    pub height: Option<PopupSize>,
}

impl App {
    pub(crate) fn active_public_tab_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        let ws = self.state.workspaces.get(ws_idx)?;
        self.public_tab_id(ws_idx, ws.active_tab_index())
    }

    fn shutdown_popup_state(&mut self, popup: crate::app::state::PopupPaneState) {
        self.state
            .direct_attach_resize_locks
            .remove(&popup.terminal_id);
        self.state.terminals.remove(&popup.terminal_id);
        self.shutdown_terminal_runtime(popup.terminal_id);
    }

    pub(crate) fn close_popup_pane(&mut self) -> bool {
        let Some(popup) = self.state.popup_pane.take() else {
            return false;
        };
        self.shutdown_popup_state(popup);
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn park_active_popup(&mut self, restore_on_return: bool) -> bool {
        let Some(popup) = self.state.popup_pane.take() else {
            return false;
        };
        if !popup.persistent {
            self.shutdown_popup_state(popup);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return true;
        }
        let Some(owner_tab_id) = popup.owner_tab_id.clone() else {
            self.shutdown_popup_state(popup);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
            return true;
        };
        let command_label = popup
            .command_label
            .clone()
            .unwrap_or_else(|| "popup".to_owned());
        if restore_on_return {
            for ((owner, _), entry) in &mut self.state.parked_floats {
                if owner == &owner_tab_id {
                    entry.restore_on_return = false;
                }
            }
        }
        let key = (owner_tab_id.clone(), command_label);
        if let Some(previous) = self.state.parked_floats.insert(
            key,
            crate::app::state::ParkedFloatState {
                owner_tab_id,
                pane: popup,
                restore_on_return,
            },
        ) {
            self.shutdown_popup_state(previous.pane);
        }
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn dismiss_active_popup(&mut self) -> bool {
        match self.state.popup_pane.as_ref() {
            Some(popup) if popup.persistent => self.park_active_popup(true),
            Some(_) => self.close_popup_pane(),
            None => false,
        }
    }

    pub(crate) fn promote_parked_float(&mut self, key: &(String, String)) -> bool {
        let Some(parked) = self.state.parked_floats.remove(key) else {
            return false;
        };
        for ((owner, _), entry) in &mut self.state.parked_floats {
            if owner == &key.0 {
                entry.restore_on_return = false;
            }
        }
        self.state.popup_pane = Some(parked.pane);
        self.state.mode = Mode::Terminal;
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn close_parked_float_for_pane(&mut self, pane_id: PaneId) -> bool {
        let key = self
            .state
            .parked_floats
            .iter()
            .find_map(|(key, entry)| (entry.pane.pane_id == pane_id).then(|| key.clone()));
        let Some(key) = key else {
            return false;
        };
        if let Some(removed) = self.state.parked_floats.remove(&key) {
            self.shutdown_popup_state(removed.pane);
            return true;
        }
        false
    }

    pub(crate) fn prune_dead_tab_floats(&mut self) -> bool {
        let dead_keys: Vec<(String, String)> = self
            .state
            .parked_floats
            .iter()
            .filter(|(_, entry)| self.parse_tab_id(&entry.owner_tab_id).is_none())
            .map(|(key, _)| key.clone())
            .collect();
        let mut changed = !dead_keys.is_empty();
        for key in dead_keys {
            if let Some(removed) = self.state.parked_floats.remove(&key) {
                self.shutdown_popup_state(removed.pane);
            }
        }
        if let Some(active_owner) = self
            .state
            .popup_pane
            .as_ref()
            .filter(|popup| popup.persistent)
            .and_then(|popup| popup.owner_tab_id.as_deref())
        {
            if self.parse_tab_id(active_owner).is_none() && self.close_popup_pane() {
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn sync_persistent_floats(&mut self) -> bool {
        let mut changed = self.prune_dead_tab_floats();
        let current_tab = self.active_public_tab_id();
        if let Some(popup) = self.state.popup_pane.as_ref() {
            if popup.persistent && popup.owner_tab_id != current_tab && self.park_active_popup(true)
            {
                changed = true;
            }
        }
        if self.state.popup_pane.is_none() {
            if let Some(owner_tab_id) = current_tab {
                let restore_key = self.state.parked_floats.iter().find_map(|(key, entry)| {
                    (entry.owner_tab_id == owner_tab_id && entry.restore_on_return)
                        .then(|| key.clone())
                });
                if let Some(key) = restore_key {
                    if self.promote_parked_float(&key) {
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn resize_parked_floats(
        &self,
        area: ratatui::layout::Rect,
        cell_size: crate::kitty_graphics::HostCellSize,
    ) {
        for entry in self.state.parked_floats.values() {
            let Some(geometry) = resolve_popup_geometry(entry.pane.width, entry.pane.height, area)
            else {
                continue;
            };
            if self
                .state
                .direct_attach_resize_locks
                .contains(&entry.pane.terminal_id)
            {
                continue;
            }
            if let Some(runtime) = self.terminal_runtimes.get(&entry.pane.terminal_id) {
                runtime.resize(
                    geometry.inner.height,
                    geometry.inner.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
    }

    pub(crate) fn toggle_persistent_popup_command(
        &mut self,
        binding: &crate::config::CustomCommandKeybind,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<()> {
        let Some(owner_tab_id) = self.active_public_tab_id() else {
            return Err(std::io::Error::other("no active tab"));
        };
        let key = (owner_tab_id.clone(), binding.label.clone());
        if self.state.popup_pane.as_ref().is_some_and(|popup| {
            popup.persistent
                && popup.owner_tab_id.as_deref() == Some(owner_tab_id.as_str())
                && popup.command_label.as_deref() == Some(binding.label.as_str())
        }) {
            self.park_active_popup(false);
            return Ok(());
        }
        if self.state.parked_floats.contains_key(&key) {
            self.dismiss_active_popup();
            self.promote_parked_float(&key);
            return Ok(());
        }
        self.dismiss_active_popup();
        for ((owner, _), entry) in &mut self.state.parked_floats {
            if owner == &owner_tab_id {
                entry.restore_on_return = false;
            }
        }
        let cmd = binding.command.clone();
        self.spawn_popup_command_with_meta(
            None,
            extra_env,
            PopupGeometry {
                width: binding.width,
                height: binding.height,
            },
            true,
            Some(owner_tab_id),
            Some(binding.label.clone()),
            |pane_id, rows, cols, cwd, launch_env, app| {
                TerminalRuntime::spawn_shell_command(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    &cmd,
                    launch_env,
                    crate::pane::AgentDetection::Disabled,
                    app.state.pane_scrollback_limit_bytes,
                    app.state.host_terminal_theme,
                    app.pane_host_terminal_appearance(),
                    app.event_tx.clone(),
                    app.render_notify.clone(),
                    app.render_dirty.clone(),
                )
                .map(|runtime| (runtime, None))
            },
        )
    }

    pub(crate) fn spawn_popup_shell_command(
        &mut self,
        command: &str,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
    ) -> std::io::Result<()> {
        if self
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.persistent)
        {
            self.park_active_popup(true);
        }
        self.spawn_popup_command(
            cwd,
            extra_env,
            geometry,
            |pane_id, rows, cols, cwd, launch_env, app| {
                TerminalRuntime::spawn_shell_command(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    command,
                    launch_env,
                    crate::pane::AgentDetection::Disabled,
                    app.state.pane_scrollback_limit_bytes,
                    app.state.host_terminal_theme,
                    app.pane_host_terminal_appearance(),
                    app.event_tx.clone(),
                    app.render_notify.clone(),
                    app.render_dirty.clone(),
                )
                .map(|runtime| (runtime, None))
            },
        )
    }

    pub(crate) fn spawn_popup_argv_command(
        &mut self,
        argv: &[String],
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
    ) -> std::io::Result<()> {
        self.spawn_popup_command(
            cwd,
            extra_env,
            geometry,
            |pane_id, rows, cols, cwd, launch_env, app| {
                TerminalRuntime::spawn_argv_command(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    argv,
                    launch_env,
                    crate::pane::AgentDetection::Disabled,
                    app.state.pane_scrollback_limit_bytes,
                    app.state.host_terminal_theme,
                    app.pane_host_terminal_appearance(),
                    app.event_tx.clone(),
                    app.render_notify.clone(),
                    app.render_dirty.clone(),
                )
                .map(|runtime| (runtime, Some(argv.to_vec())))
            },
        )
    }

    fn spawn_popup_command<F>(
        &mut self,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
        spawn: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(
            PaneId,
            u16,
            u16,
            PathBuf,
            &PaneLaunchEnv,
            &mut App,
        ) -> std::io::Result<(TerminalRuntime, Option<Vec<String>>)>,
    {
        self.spawn_popup_command_with_meta(cwd, extra_env, geometry, false, None, None, spawn)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_popup_command_with_meta<F>(
        &mut self,
        cwd: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
        geometry: PopupGeometry,
        persistent: bool,
        owner_tab_id: Option<String>,
        command_label: Option<String>,
        spawn: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(
            PaneId,
            u16,
            u16,
            PathBuf,
            &PaneLaunchEnv,
            &mut App,
        ) -> std::io::Result<(TerminalRuntime, Option<Vec<String>>)>,
    {
        if self.state.popup_pane.is_some() {
            return Err(std::io::Error::other("popup already open"));
        }
        let Some(ws_idx) = self.state.active else {
            return Err(std::io::Error::other("no active workspace"));
        };
        let ws = self
            .state
            .workspaces
            .get(ws_idx)
            .ok_or_else(|| std::io::Error::other("active workspace disappeared"))?;
        let active_tab = ws
            .active_tab()
            .ok_or_else(|| std::io::Error::other("active tab disappeared"))?;
        let focused_pane = ws
            .focused_pane_id()
            .ok_or_else(|| std::io::Error::other("active tab has no focused pane"))?;
        let cwd = cwd.or_else(|| {
            active_tab.cwd_for_pane(focused_pane, &self.state.terminals, &self.terminal_runtimes)
        });
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let launch_env = PaneLaunchEnv::from_extra(extra_env).without_pane_identity();
        let terminal_area = if self.state.view.terminal_area.width >= 4
            && self.state.view.terminal_area.height >= 4
        {
            self.state.view.terminal_area
        } else {
            let (cols, rows) = self.state.headless_size;
            ratatui::layout::Rect::new(0, 0, cols, rows)
        };
        let Some(resolved_geometry) =
            resolve_popup_geometry(geometry.width, geometry.height, terminal_area)
        else {
            return Err(std::io::Error::other("terminal area too small for popup"));
        };
        let rows = resolved_geometry.inner.height;
        let cols = resolved_geometry.inner.width;
        let (runtime, launch_argv) = spawn(pane_id, rows, cols, cwd.clone(), &launch_env, self)?;
        let terminal = match launch_argv {
            Some(argv) => TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(argv),
            None => TerminalState::new(terminal_id.clone(), cwd),
        };
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id,
            terminal_id,
            width: geometry.width,
            height: geometry.height,
            persistent,
            owner_tab_id,
            command_label,
        });
        self.state.mode = Mode::Terminal;
        Ok(())
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn install_test_popup_runtime(
        &mut self,
        runtime: TerminalRuntime,
    ) -> (PaneId, TerminalId) {
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/popup")),
        );
        self.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id,
            terminal_id: terminal_id.clone(),
            width: None,
            height: None,
            persistent: false,
            owner_tab_id: None,
            command_label: None,
        });
        (pane_id, terminal_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_popup() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("popup")];
        app.state.active = Some(0);
        app.state.selected = 0;
        let terminal_id = TerminalId::alloc();
        app.state.terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id.clone(), PathBuf::from("/popup")),
        );
        app.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id: PaneId::alloc(),
            terminal_id,
            width: None,
            height: None,
            persistent: false,
            owner_tab_id: None,
            command_label: None,
        });
        app
    }

    #[test]
    fn close_popup_uses_terminal_mode_with_active_workspace() {
        let mut app = app_with_popup();
        app.state.mode = Mode::Navigate;

        assert!(app.close_popup_pane());

        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[test]
    fn close_popup_uses_navigate_mode_without_active_workspace() {
        let mut app = app_with_popup();
        app.state.workspaces.clear();
        app.state.active = None;
        app.state.mode = Mode::Navigate;

        assert!(app.close_popup_pane());

        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn close_popup_clears_direct_attach_resize_lock() {
        let mut app = app_with_popup();
        let terminal_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();
        app.state
            .direct_attach_resize_locks
            .insert(terminal_id.clone());

        assert!(app.close_popup_pane());

        assert!(!app.state.direct_attach_resize_locks.contains(&terminal_id));
    }

    #[test]
    fn popup_survives_background_workspace_removal() {
        let mut app = app_with_popup();
        app.state.workspaces.clear();
        app.state.active = None;

        app.state.assert_invariants_for_test();

        assert!(app.state.popup_pane.is_some());
    }

    #[test]
    fn popup_close_api_closes_only_active_popup() {
        let mut app = app_with_popup();
        let close = || crate::api::schema::Request {
            id: "close-popup".into(),
            method: crate::api::schema::Method::PopupClose(
                crate::api::schema::EmptyParams::default(),
            ),
        };

        let response = app.handle_api_request(close());
        let response: crate::api::schema::SuccessResponse =
            serde_json::from_str(&response).unwrap();
        assert_eq!(response.result, crate::api::schema::ResponseResult::Ok {});

        let response = app.handle_api_request(close());
        let response: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(response.error.code, "popup_not_open");
    }

    #[test]
    fn persistent_float_parks_restores_across_tabs_and_prunes_on_tab_close() {
        let mut app = app_with_popup();
        let ws_id = app.state.workspaces[0].id.clone();
        let tab1_id = app.active_public_tab_id().unwrap();
        let mut tab2 = crate::workspace::Workspace::test_new("tab2").tabs.remove(0);
        tab2.number = 2;
        app.state.workspaces[0].tabs.push(tab2);
        let tab2_id = crate::workspace::public_tab_id_for_number(&ws_id, 2);

        let float_term_id = app.state.popup_pane.as_ref().unwrap().terminal_id.clone();
        let float_pane_id = app.state.popup_pane.as_ref().unwrap().pane_id;
        {
            let popup = app.state.popup_pane.as_mut().unwrap();
            popup.persistent = true;
            popup.owner_tab_id = Some(tab1_id.clone());
            popup.command_label = Some("alt+f".to_owned());
        }

        // Switching away to tab 2 auto-parks tab 1's float with restore_on_return = true.
        app.state.switch_workspace_tab(0, 1);
        assert!(app.sync_persistent_floats());
        assert!(app.state.popup_pane.is_none());
        let key = (tab1_id.clone(), "alt+f".to_owned());
        assert!(app
            .state
            .parked_floats
            .get(&key)
            .is_some_and(|entry| entry.restore_on_return));

        // Returning to tab 1 automatically restores the same float PTY without respawn.
        app.state.switch_workspace_tab(0, 0);
        assert!(app.sync_persistent_floats());
        assert_eq!(
            app.state
                .popup_pane
                .as_ref()
                .map(|p| (&p.terminal_id, p.pane_id)),
            Some((&float_term_id, float_pane_id))
        );

        // Explicit toggle park (restore_on_return = false) stays hidden across a round-trip.
        assert!(app.park_active_popup(false));
        app.state.switch_workspace_tab(0, 1);
        assert!(!app.sync_persistent_floats());
        app.state.switch_workspace_tab(0, 0);
        assert!(!app.sync_persistent_floats());
        assert!(app.state.popup_pane.is_none());
        assert!(app.state.parked_floats.contains_key(&key));

        // Closing the owning tab prunes the parked float and its TerminalState.
        app.state.workspaces[0].tabs.retain(|t| t.number != 1);
        app.state.workspaces[0].active_tab = 0;
        let _ = tab2_id;
        assert!(app.sync_persistent_floats());
        assert!(app.state.parked_floats.is_empty());
        assert!(!app.state.terminals.contains_key(&float_term_id));
    }
}
