use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::render::{display_width, put_right_text, put_segment, put_text};
use super::state::ShellHitMap;
use crate::app::state::Palette;
use crate::protocol::ClientShellSnapshot;

const DEFAULT_RIGHT_PANEL_WIDTH: u16 = 36;
pub(crate) const MIN_RIGHT_PANEL_WIDTH: u16 = 24;
pub(crate) const MAX_RIGHT_PANEL_WIDTH: u16 = 72;
const MAX_UNTRACKED_PREVIEW_FILES: usize = 8;
const MAX_UNTRACKED_PREVIEW_LINES: usize = 200;
const REMOTE_DIFF_MARKER: &str = "___HERDR_DIFF_SENTINEL___";

/// Active sub-view of the collapsible right-hand workspace panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RightPanelTab {
    Files,
    Changes,
}

/// Target workspace location inspected by the right panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RightPanelTarget {
    Local(PathBuf),
    Hc {
        machine: Option<String>,
        cwd: String,
    },
}

/// Single entry inside a directory listing in the Files view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileTreeEntry {
    pub rel_path: String,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
}

/// Git status category for a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

/// Changed file entry and its parsed unified diff lines for the Changes view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFileEntry {
    pub path: String,
    pub status: ChangeKind,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<String>,
}

/// Interactive hit action for a visible row in the right panel body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RightPanelRowHit {
    ToggleDir(String),
    ToggleDiffFile(String),
    SelectFile(String),
}

enum RightPanelFetchPayload {
    DirResult {
        rel_dir: String,
        result: Result<Vec<FileTreeEntry>, String>,
    },
    Changes {
        branch: Option<String>,
        files: Vec<DiffFileEntry>,
        error: Option<String>,
    },
}

struct RightPanelFetchResult {
    generation: u64,
    payload: RightPanelFetchPayload,
}

/// Client-side state for the collapsible Files / Changes right panel.
pub(crate) struct RightPanelState {
    pub active_tab: Option<RightPanelTab>,
    pub width: u16,
    pub scroll: usize,
    pub target: Option<RightPanelTarget>,
    pub expanded_dirs: HashSet<String>,
    pub dir_children: HashMap<String, Vec<FileTreeEntry>>,
    pub collapsed_diff_files: HashSet<String>,
    pub selected_path: Option<String>,
    pub diff_files: Vec<DiffFileEntry>,
    pub branch: Option<String>,
    pub loading_files: bool,
    pub loading_changes: bool,
    pub files_error: Option<String>,
    pub changes_error: Option<String>,
    files_loaded: bool,
    changes_loaded: bool,
    tx: Option<mpsc::Sender<RightPanelFetchResult>>,
    rx: Option<mpsc::Receiver<RightPanelFetchResult>>,
    generation: u64,
}

impl Default for RightPanelState {
    fn default() -> Self {
        Self {
            active_tab: None,
            width: DEFAULT_RIGHT_PANEL_WIDTH,
            scroll: 0,
            target: None,
            expanded_dirs: HashSet::new(),
            dir_children: HashMap::new(),
            collapsed_diff_files: HashSet::new(),
            selected_path: None,
            diff_files: Vec::new(),
            branch: None,
            loading_files: false,
            loading_changes: false,
            files_error: None,
            changes_error: None,
            files_loaded: false,
            changes_loaded: false,
            tx: None,
            rx: None,
            generation: 0,
        }
    }
}

impl RightPanelState {
    /// Return whether the currently visible tab has a fetch in progress.
    pub fn is_loading(&self) -> bool {
        match self.active_tab {
            Some(RightPanelTab::Files) => self.loading_files,
            Some(RightPanelTab::Changes) => self.loading_changes,
            None => false,
        }
    }

    /// Return the error message belonging to the currently active tab, if any.
    pub fn active_error(&self) -> Option<&str> {
        match self.active_tab {
            Some(RightPanelTab::Files) => self.files_error.as_deref(),
            Some(RightPanelTab::Changes) => self.changes_error.as_deref(),
            None => None,
        }
    }

    /// Toggle a tab open, switch between Files/Changes, or collapse when clicking the active tab.
    pub fn toggle_tab(&mut self, tab: RightPanelTab) {
        if self.active_tab == Some(tab) {
            self.active_tab = None;
            return;
        }
        self.active_tab = Some(tab);
        self.scroll = 0;
        self.start_fetch_if_needed(tab);
    }

    /// Collapse the right panel.
    pub fn close(&mut self) {
        self.active_tab = None;
    }

    /// Poll completed background fetches and ensure the active view is up to date for `snapshot`.
    pub fn sync_target(&mut self, snapshot: &ClientShellSnapshot) {
        self.poll();
        if self.active_tab.is_none() {
            return;
        }
        let next_target = resolve_target(snapshot);
        if self.target != next_target {
            let preserve_preloaded = self.target.is_none();
            self.target = next_target;
            self.scroll = 0;
            self.reset_channel();
            if !preserve_preloaded {
                self.expanded_dirs.clear();
                self.dir_children.clear();
                self.collapsed_diff_files.clear();
                self.selected_path = None;
                self.diff_files.clear();
                self.branch = None;
                self.files_error = None;
                self.changes_error = None;
                self.files_loaded = false;
                self.changes_loaded = false;
            }
        }
        if let Some(tab) = self.active_tab {
            self.start_fetch_if_needed(tab);
        }
    }

    /// Drain pending background fetch results and return whether rendered state changed.
    pub fn poll(&mut self) -> bool {
        let mut drained = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(result) = rx.try_recv() {
                drained.push(result);
            }
        }
        let mut changed = false;
        for result in drained {
            if result.generation != self.generation {
                continue;
            }
            changed = true;
            match result.payload {
                RightPanelFetchPayload::DirResult { rel_dir, result } => {
                    self.loading_files = false;
                    self.files_loaded = true;
                    match result {
                        Ok(entries) => {
                            self.files_error = None;
                            self.dir_children.insert(rel_dir, entries);
                        }
                        Err(err) => {
                            self.files_error = Some(err);
                        }
                    }
                }
                RightPanelFetchPayload::Changes {
                    branch,
                    files,
                    error,
                } => {
                    self.loading_changes = false;
                    self.changes_loaded = true;
                    self.branch = branch;
                    self.diff_files = files;
                    self.changes_error = error;
                }
            }
        }
        changed
    }

    /// Expand or collapse a directory entry, spawning a child fetch on first expansion.
    pub fn toggle_dir(&mut self, rel_path: &str) {
        if self.expanded_dirs.remove(rel_path) {
            return;
        }
        self.expanded_dirs.insert(rel_path.to_owned());
        if !self.dir_children.contains_key(rel_path) {
            let depth = rel_path.split('/').filter(|part| !part.is_empty()).count();
            self.spawn_dir_fetch(rel_path.to_owned(), depth);
        }
    }

    /// Toggle whether a file's diff hunks are collapsed inside the Changes view.
    pub fn toggle_diff_file(&mut self, path: &str) {
        if !self.collapsed_diff_files.remove(path) {
            self.collapsed_diff_files.insert(path.to_owned());
        }
    }

    /// Clear cached listings/diffs for the current target and re-fetch from disk or `hc`.
    pub fn refresh(&mut self) {
        self.files_error = None;
        self.changes_error = None;
        self.dir_children.clear();
        self.diff_files.clear();
        self.branch = None;
        self.files_loaded = false;
        self.changes_loaded = false;
        self.reset_channel();
        if let Some(tab) = self.active_tab {
            self.start_fetch_if_needed(tab);
        }
    }

    fn reset_channel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.tx = None;
        self.rx = None;
        self.loading_files = false;
        self.loading_changes = false;
    }

    fn ensure_channel(&mut self) -> (u64, mpsc::Sender<RightPanelFetchResult>) {
        if self.tx.is_none() || self.rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        (
            self.generation,
            self.tx.as_ref().expect("channel initialized").clone(),
        )
    }

    fn start_fetch_if_needed(&mut self, tab: RightPanelTab) {
        if self.target.is_none() {
            return;
        }
        match tab {
            RightPanelTab::Files => {
                if self.loading_files
                    || self.files_loaded
                    || !self.dir_children.is_empty()
                    || self.files_error.is_some()
                {
                    return;
                }
                self.spawn_dir_fetch(String::new(), 0);
                let expanded: Vec<String> = self.expanded_dirs.iter().cloned().collect();
                for dir in expanded {
                    let depth = dir.split('/').filter(|part| !part.is_empty()).count();
                    self.spawn_dir_fetch(dir, depth);
                }
            }
            RightPanelTab::Changes => {
                if self.loading_changes
                    || self.changes_loaded
                    || !self.diff_files.is_empty()
                    || self.branch.is_some()
                    || self.changes_error.is_some()
                {
                    return;
                }
                self.spawn_changes_fetch();
            }
        }
    }

    fn spawn_dir_fetch(&mut self, rel_dir: String, depth: usize) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let (generation, tx) = self.ensure_channel();
        self.loading_files = true;
        std::thread::spawn(move || {
            let result = fetch_dir_entries(&target, &rel_dir, depth);
            let _ = tx.send(RightPanelFetchResult {
                generation,
                payload: RightPanelFetchPayload::DirResult { rel_dir, result },
            });
        });
    }

    fn spawn_changes_fetch(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let (generation, tx) = self.ensure_channel();
        self.loading_changes = true;
        std::thread::spawn(move || {
            let payload = fetch_changes(&target);
            let _ = tx.send(RightPanelFetchResult {
                generation,
                payload,
            });
        });
    }
}

/// Resolve the focused pane's workspace target for Files/Changes inspection.
pub(crate) fn resolve_target(snapshot: &ClientShellSnapshot) -> Option<RightPanelTarget> {
    let focused_ws = snapshot
        .focused_workspace_id
        .as_deref()
        .and_then(|id| snapshot.workspaces.iter().find(|ws| ws.workspace_id == id))
        .or_else(|| snapshot.workspaces.iter().find(|ws| ws.focused));

    let focused_pane = snapshot
        .focused_pane_id
        .as_deref()
        .and_then(|id| snapshot.panes.iter().find(|pane| pane.pane_id == id))
        .or_else(|| snapshot.panes.iter().find(|pane| pane.focused))
        .or_else(|| {
            focused_ws.and_then(|ws| {
                snapshot.panes.iter().find(|pane| {
                    pane.workspace_id == ws.workspace_id && pane.tab_id == ws.active_tab_id
                })
            })
        });

    let cwd = focused_pane
        .and_then(|pane| {
            pane.foreground_cwd
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| pane.cwd.as_deref().filter(|s| !s.is_empty()))
        })
        .or_else(|| {
            focused_ws
                .map(|ws| ws.new_workspace_cwd.as_str())
                .filter(|s| !s.is_empty())
        })?;

    let ws_for_pane = focused_pane
        .and_then(|pane| {
            snapshot
                .workspaces
                .iter()
                .find(|ws| ws.workspace_id == pane.workspace_id)
        })
        .or(focused_ws);

    let machine = ws_for_pane
        .and_then(|ws| ws.machine.clone())
        .or_else(|| {
            ws_for_pane.and_then(|ws| crate::hc::client::get_workspace_machine(&ws.workspace_id))
        })
        .filter(|machine| !machine.is_empty());

    if let Some(machine) = machine {
        return Some(RightPanelTarget::Hc {
            machine: Some(machine),
            cwd: cwd.to_owned(),
        });
    }

    Some(RightPanelTarget::Local(PathBuf::from(cwd)))
}

fn join_rel_path(rel_dir: &str, name: &str) -> String {
    if rel_dir.is_empty() {
        name.to_owned()
    } else {
        format!("{rel_dir}/{name}")
    }
}

fn sort_tree_entries(entries: &mut [FileTreeEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Parse `ls -1Ap` output lines into sorted file-tree entries relative to `rel_dir`.
pub(crate) fn parse_ls_entries(rel_dir: &str, depth: usize, output: &str) -> Vec<FileTreeEntry> {
    let mut entries = Vec::new();
    for raw_line in output.lines() {
        let trimmed = raw_line.trim_end_matches('\r');
        if trimmed.is_empty() || trimmed == "./" || trimmed == "../" {
            continue;
        }
        let is_dir = trimmed.ends_with('/');
        let name = if is_dir {
            trimmed.trim_end_matches('/')
        } else {
            trimmed
        };
        if name.is_empty() || name == "." || name == ".." || name == ".git" {
            continue;
        }
        entries.push(FileTreeEntry {
            rel_path: join_rel_path(rel_dir, name),
            name: name.to_owned(),
            is_dir,
            depth,
        });
    }
    sort_tree_entries(&mut entries);
    entries
}

fn fetch_dir_entries(
    target: &RightPanelTarget,
    rel_dir: &str,
    depth: usize,
) -> Result<Vec<FileTreeEntry>, String> {
    match target {
        RightPanelTarget::Local(root) => {
            let dir_path = if rel_dir.is_empty() {
                root.clone()
            } else {
                root.join(rel_dir)
            };
            let read = std::fs::read_dir(&dir_path)
                .map_err(|err| format!("failed to read {}: {err}", dir_path.display()))?;
            let mut entries = Vec::new();
            for item in read {
                let Ok(entry) = item else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                entries.push(FileTreeEntry {
                    rel_path: join_rel_path(rel_dir, &name),
                    name,
                    is_dir,
                    depth,
                });
            }
            sort_tree_entries(&mut entries);
            Ok(entries)
        }
        RightPanelTarget::Hc { machine, cwd } => {
            let core = active_hc_core()?;
            let target_dir = if rel_dir.is_empty() {
                cwd.clone()
            } else {
                format!("{}/{}", cwd.trim_end_matches('/'), rel_dir)
            };
            let stdout = core
                .exec_machine(
                    machine.as_deref(),
                    &[
                        "sh",
                        "-c",
                        "cd -- \"$1\" && LC_ALL=C ls -1Ap",
                        "sh",
                        &target_dir,
                    ],
                    5_000,
                )
                .map_err(|err| format!("remote ls failed: {err}"))?;
            Ok(parse_ls_entries(rel_dir, depth, &stdout))
        }
    }
}

fn active_hc_core() -> Result<std::sync::Arc<crate::hc::Core>, String> {
    if let Some(core) = crate::hc::global_core() {
        return Ok(core);
    }
    let session = crate::session::active_name();
    crate::hc::Core::connect(session.as_deref()).map_err(|err| format!("hc connect failed: {err}"))
}

fn fetch_changes(target: &RightPanelTarget) -> RightPanelFetchPayload {
    match target {
        RightPanelTarget::Local(cwd) => collect_local_git_changes(cwd),
        RightPanelTarget::Hc { machine, cwd } => collect_hc_git_changes(machine.as_deref(), cwd),
    }
}

fn collect_local_git_changes(cwd: &Path) -> RightPanelFetchPayload {
    let status_out = match Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v1", "-b"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let message = if stderr.is_empty() {
                "Not a git repository".to_owned()
            } else {
                stderr
            };
            return RightPanelFetchPayload::Changes {
                branch: None,
                files: Vec::new(),
                error: Some(message),
            };
        }
        Err(err) => {
            return RightPanelFetchPayload::Changes {
                branch: None,
                files: Vec::new(),
                error: Some(format!("git status failed: {err}")),
            };
        }
    };

    let diff_out = match Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--no-color", "HEAD"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => {
            let unstaged = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["diff", "--no-color"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            let cached = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["diff", "--no-color", "--cached"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            format!("{unstaged}\n{cached}")
        }
    };

    let (branch, mut files) = parse_git_status_and_diff(&status_out, &diff_out);
    populate_local_untracked_previews(cwd, &mut files);
    RightPanelFetchPayload::Changes {
        branch,
        files,
        error: None,
    }
}

fn collect_hc_git_changes(machine: Option<&str>, cwd: &str) -> RightPanelFetchPayload {
    let core = match active_hc_core() {
        Ok(core) => core,
        Err(err) => {
            return RightPanelFetchPayload::Changes {
                branch: None,
                files: Vec::new(),
                error: Some(err),
            };
        }
    };
    let script = concat!(
        "cd -- \"$1\" && ",
        "git status --porcelain=v1 -b && ",
        "printf '___HERDR_DIFF_SENTINEL___\\n' && ",
        "(git diff --no-color HEAD 2>/dev/null || (git diff --no-color; git diff --no-color --cached))"
    );
    let stdout = match core.exec_machine(machine, &["sh", "-c", script, "sh", cwd], 8_000) {
        Ok(out) => out,
        Err(err) => {
            return RightPanelFetchPayload::Changes {
                branch: None,
                files: Vec::new(),
                error: Some(format!("Not a git repository ({err})")),
            };
        }
    };
    let Some((status_out, diff_rest)) = stdout.split_once(REMOTE_DIFF_MARKER) else {
        return RightPanelFetchPayload::Changes {
            branch: None,
            files: Vec::new(),
            error: Some("Not a git repository".to_owned()),
        };
    };
    let diff_out = diff_rest.trim_start_matches(['\r', '\n']);
    let (branch, mut files) = parse_git_status_and_diff(status_out, diff_out);
    if core.supports_method("file.read") {
        let mut previewed = 0usize;
        for file in &mut files {
            if file.status != ChangeKind::Untracked || previewed >= MAX_UNTRACKED_PREVIEW_FILES {
                continue;
            }
            previewed += 1;
            if let Ok(text) = core.read_remote_file(machine, &file.path, Some(cwd)) {
                apply_untracked_text_preview(file, &text);
            }
        }
    }
    RightPanelFetchPayload::Changes {
        branch,
        files,
        error: None,
    }
}

fn populate_local_untracked_previews(cwd: &Path, files: &mut [DiffFileEntry]) {
    let mut previewed = 0usize;
    for file in files {
        if file.status != ChangeKind::Untracked || previewed >= MAX_UNTRACKED_PREVIEW_FILES {
            continue;
        }
        previewed += 1;
        let full_path = cwd.join(&file.path);
        let Ok(bytes) = std::fs::read(&full_path) else {
            continue;
        };
        if bytes.contains(&0) {
            file.lines = vec!["(binary file)".to_owned()];
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        apply_untracked_text_preview(file, &text);
    }
}

fn apply_untracked_text_preview(file: &mut DiffFileEntry, text: &str) {
    let mut lines = Vec::new();
    for line in text.lines().take(MAX_UNTRACKED_PREVIEW_LINES) {
        lines.push(format!("+{line}"));
    }
    file.additions = lines.len();
    file.deletions = 0;
    file.lines = lines;
}

/// Parse `git status --porcelain=v1 -b` and unified `git diff --no-color` into structured entries.
pub(crate) fn parse_git_status_and_diff(
    status_output: &str,
    diff_output: &str,
) -> (Option<String>, Vec<DiffFileEntry>) {
    let mut branch = None;
    let mut entries = Vec::<DiffFileEntry>::new();

    for line in status_output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("## ") {
            branch = parse_status_branch(rest);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let _space = chars.next();
        let raw_path = &line[3..];
        let path = parse_porcelain_path(raw_path, x == 'R' || y == 'R');
        if path.is_empty() {
            continue;
        }
        let status = match (x, y) {
            ('?', '?') => ChangeKind::Untracked,
            ('A', _) | (_, 'A') => ChangeKind::Added,
            ('D', _) | (_, 'D') => ChangeKind::Deleted,
            ('R', _) | (_, 'R') => ChangeKind::Renamed,
            _ => ChangeKind::Modified,
        };
        if let Some(existing) = entries.iter_mut().find(|item| item.path == path) {
            existing.status = status;
        } else {
            entries.push(DiffFileEntry {
                path,
                status,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
        }
    }

    for section in parse_diff_sections(diff_output) {
        if let Some(existing) = entries.iter_mut().find(|item| item.path == section.path) {
            existing.additions = existing.additions.saturating_add(section.additions);
            existing.deletions = existing.deletions.saturating_add(section.deletions);
            existing.lines.extend(section.lines);
        } else {
            entries.push(section);
        }
    }

    (branch, entries)
}

fn parse_status_branch(rest: &str) -> Option<String> {
    let rest = rest
        .strip_prefix("No commits yet on ")
        .or_else(|| rest.strip_prefix("Initial commit on "))
        .unwrap_or(rest);
    if rest.starts_with("HEAD (") {
        return Some("HEAD".to_owned());
    }
    let name = rest
        .split("...")
        .next()
        .unwrap_or(rest)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn strip_git_quotes(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].replace("\\\"", "\"")
    } else {
        trimmed.to_owned()
    }
}

fn parse_porcelain_path(raw: &str, renamed: bool) -> String {
    if renamed {
        if let Some((_, rhs)) = raw.split_once(" -> ") {
            return strip_git_quotes(rhs);
        }
    }
    strip_git_quotes(raw)
}

fn parse_diff_sections(diff_output: &str) -> Vec<DiffFileEntry> {
    let mut sections = Vec::<DiffFileEntry>::new();
    let mut current: Option<DiffFileEntry> = None;
    let mut in_hunk = false;

    for raw_line in diff_output.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(header) = line.strip_prefix("diff --git ") {
            if let Some(prev) = current.take() {
                sections.push(prev);
            }
            in_hunk = false;
            let path = parse_diff_header_path(header);
            current = Some(DiffFileEntry {
                path,
                status: ChangeKind::Modified,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(renamed_to) = line.strip_prefix("rename to ") {
            entry.path = strip_git_quotes(renamed_to);
            entry.status = ChangeKind::Renamed;
            continue;
        }
        if line.starts_with("new file mode ") {
            entry.status = ChangeKind::Added;
            continue;
        }
        if line.starts_with("deleted file mode ") {
            entry.status = ChangeKind::Deleted;
            continue;
        }
        if let Some(plus_path) = line.strip_prefix("+++ ") {
            if plus_path != "/dev/null" {
                let clean = plus_path.strip_prefix("b/").unwrap_or(plus_path);
                entry.path = strip_git_quotes(clean);
            }
            continue;
        }
        if let Some(minus_path) = line.strip_prefix("--- ") {
            if minus_path != "/dev/null" && entry.path.is_empty() {
                let clean = minus_path.strip_prefix("a/").unwrap_or(minus_path);
                entry.path = strip_git_quotes(clean);
            }
            continue;
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
            entry.lines.push(line.to_owned());
            continue;
        }
        if in_hunk {
            if line.starts_with('+') {
                entry.additions += 1;
                entry.lines.push(line.to_owned());
            } else if line.starts_with('-') {
                entry.deletions += 1;
                entry.lines.push(line.to_owned());
            } else if line.starts_with(' ') || line.starts_with('\\') {
                entry.lines.push(line.to_owned());
            }
        }
    }
    if let Some(prev) = current.take() {
        sections.push(prev);
    }
    sections
}

fn parse_diff_header_path(header: &str) -> String {
    if let Some((_, rhs)) = header.split_once(" b/") {
        return strip_git_quotes(rhs);
    }
    if let Some(last) = header.split_whitespace().last() {
        return strip_git_quotes(last.strip_prefix("b/").unwrap_or(last));
    }
    String::new()
}

struct RenderedRow {
    text: String,
    right_text: Option<(String, Style)>,
    style: Style,
    hit: Option<RightPanelRowHit>,
}

/// Render the collapsible Files / Changes right panel and populate its mouse hit rectangles.
pub(super) fn render_right_panel(
    buffer: &mut Buffer,
    area: Rect,
    state: &RightPanelState,
    palette: &Palette,
    mouse_capture: bool,
    hits: &mut ShellHitMap,
) {
    let Some(active_tab) = state.active_tab else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }

    buffer.set_style(area, Style::default().fg(palette.text).bg(palette.panel_bg));
    for y in area.y..area.bottom() {
        put_text(
            buffer,
            area.x,
            y,
            1,
            "│",
            Style::default().fg(palette.surface1).bg(palette.panel_bg),
        );
    }
    if mouse_capture {
        hits.right_panel_divider = Rect::new(area.x, area.y, 1, area.height);
    }

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    render_header(
        buffer,
        inner,
        state,
        active_tab,
        palette,
        mouse_capture,
        hits,
    );

    if inner.height < 2 {
        return;
    }
    render_subheader(buffer, inner, state, active_tab, palette);

    if inner.height <= 2 {
        return;
    }
    let body = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);
    hits.right_panel_body = body;

    let rows = match active_tab {
        RightPanelTab::Files => build_file_tree_rows(state, palette),
        RightPanelTab::Changes => build_changes_rows(state, palette),
    };
    hits.right_panel_max_scroll = rows.len().saturating_sub(usize::from(body.height));
    let scroll = state.scroll.min(hits.right_panel_max_scroll);

    for (idx, row) in rows
        .into_iter()
        .skip(scroll)
        .take(usize::from(body.height))
        .enumerate()
    {
        let y = body.y + idx as u16;
        let row_rect = Rect::new(body.x, y, body.width, 1);
        buffer.set_style(row_rect, row.style);
        let right_width = row
            .right_text
            .as_ref()
            .map(|(text, _)| display_width(text).saturating_add(1))
            .unwrap_or(0);
        let left_width = body.width.saturating_sub(right_width);
        put_text(buffer, body.x, y, left_width, &row.text, row.style);
        if let Some((right_str, right_style)) = row.right_text {
            put_right_text(buffer, row_rect, y, &right_str, right_style);
        }
        if mouse_capture {
            if let Some(hit) = row.hit {
                hits.right_panel_rows.push((row_rect, hit));
            }
        }
    }
}

fn render_header(
    buffer: &mut Buffer,
    inner: Rect,
    state: &RightPanelState,
    active_tab: RightPanelTab,
    palette: &Palette,
    mouse_capture: bool,
    hits: &mut ShellHitMap,
) {
    let close_width = 3_u16.min(inner.width);
    let close_rect = Rect::new(
        inner.right().saturating_sub(close_width),
        inner.y,
        close_width,
        1,
    );
    let refresh_width = 3_u16.min(inner.width.saturating_sub(close_width));
    let refresh_rect = Rect::new(
        close_rect.x.saturating_sub(refresh_width),
        inner.y,
        refresh_width,
        1,
    );

    let title_right = refresh_rect.x;
    let title = match active_tab {
        RightPanelTab::Files => " Files",
        RightPanelTab::Changes => " Changes",
    };
    let mut x = put_segment(
        buffer,
        inner.x,
        inner.y,
        title_right,
        title,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    if active_tab == RightPanelTab::Changes {
        if let Some(branch) = &state.branch {
            let label = format!("  {branch}");
            put_segment(
                buffer,
                x,
                inner.y,
                title_right,
                &label,
                Style::default().fg(palette.mauve).bg(palette.panel_bg),
            );
        }
    } else if let Some(target_label) = target_basename(state.target.as_ref()) {
        let label = format!("  {target_label}");
        x = put_segment(
            buffer,
            x,
            inner.y,
            title_right,
            &label,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        let _ = x;
    }

    let btn_style = Style::default().fg(palette.overlay1).bg(palette.surface0);
    if refresh_rect.width >= 3 {
        put_text(
            buffer,
            refresh_rect.x,
            inner.y,
            refresh_rect.width,
            " ↻ ",
            btn_style,
        );
        if mouse_capture {
            hits.right_panel_refresh = refresh_rect;
        }
    }
    if close_rect.width >= 3 {
        put_text(
            buffer,
            close_rect.x,
            inner.y,
            close_rect.width,
            " » ",
            btn_style,
        );
        if mouse_capture {
            hits.right_panel_close = close_rect;
        }
    }
}

fn render_subheader(
    buffer: &mut Buffer,
    inner: Rect,
    state: &RightPanelState,
    active_tab: RightPanelTab,
    palette: &Palette,
) {
    let y = inner.y + 1;
    let line = "─".repeat(usize::from(inner.width));
    put_text(
        buffer,
        inner.x,
        y,
        inner.width,
        &line,
        Style::default().fg(palette.surface0).bg(palette.panel_bg),
    );
    if state.is_loading() {
        put_text(
            buffer,
            inner.x,
            y,
            inner.width,
            " loading… ",
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        return;
    }
    if let Some(err) = state.active_error() {
        put_text(
            buffer,
            inner.x,
            y,
            inner.width,
            &format!(" {err} "),
            Style::default().fg(palette.red).bg(palette.panel_bg),
        );
        return;
    }
    if active_tab == RightPanelTab::Changes {
        let additions: usize = state.diff_files.iter().map(|f| f.additions).sum();
        let deletions: usize = state.diff_files.iter().map(|f| f.deletions).sum();
        let file_label = if state.diff_files.len() == 1 {
            " 1 file ".to_owned()
        } else {
            format!(" {} files ", state.diff_files.len())
        };
        let add_text = format!("+{additions}");
        let del_text = format!("-{deletions} ");
        let stats_width = display_width(&add_text)
            .saturating_add(1)
            .saturating_add(display_width(&del_text));
        put_text(
            buffer,
            inner.x,
            y,
            inner.width.saturating_sub(stats_width),
            &file_label,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        let row_rect = Rect::new(inner.x, y, inner.width, 1);
        let del_width = display_width(&del_text);
        put_right_text(
            buffer,
            row_rect,
            y,
            &del_text,
            Style::default().fg(palette.red).bg(palette.panel_bg),
        );
        let add_area = Rect::new(inner.x, y, inner.width.saturating_sub(del_width + 1), 1);
        put_right_text(
            buffer,
            add_area,
            y,
            &add_text,
            Style::default().fg(palette.green).bg(palette.panel_bg),
        );
        return;
    }
    if let Some(path) = target_display_path(state.target.as_ref()) {
        put_text(
            buffer,
            inner.x,
            y,
            inner.width,
            &format!(" {path} "),
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
}

fn target_basename(target: Option<&RightPanelTarget>) -> Option<&str> {
    match target? {
        RightPanelTarget::Local(path) => path
            .file_name()
            .and_then(|s| s.to_str())
            .or_else(|| path.to_str()),
        RightPanelTarget::Hc { cwd, .. } => cwd
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .or(Some(cwd.as_str())),
    }
}

fn target_display_path(target: Option<&RightPanelTarget>) -> Option<String> {
    match target? {
        RightPanelTarget::Local(path) => Some(path.display().to_string()),
        RightPanelTarget::Hc { machine, cwd } => Some(match machine {
            Some(m) => format!("{m}:{cwd}"),
            None => cwd.clone(),
        }),
    }
}

fn build_file_tree_rows(state: &RightPanelState, palette: &Palette) -> Vec<RenderedRow> {
    let mut rows = Vec::new();
    append_visible_tree_rows(state, "", palette, &mut rows);
    if rows.is_empty() && !state.is_loading() && state.active_error().is_none() {
        rows.push(RenderedRow {
            text: "  (empty directory)".to_owned(),
            right_text: None,
            style: Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            hit: None,
        });
    }
    rows
}

fn append_visible_tree_rows(
    state: &RightPanelState,
    rel_dir: &str,
    palette: &Palette,
    rows: &mut Vec<RenderedRow>,
) {
    let Some(entries) = state.dir_children.get(rel_dir) else {
        return;
    };
    for entry in entries {
        let indent = "  ".repeat(entry.depth);
        let selected = state.selected_path.as_deref() == Some(entry.rel_path.as_str());
        let bg = if selected {
            palette.active_row_bg
        } else {
            palette.panel_bg
        };
        if entry.is_dir {
            let expanded = state.expanded_dirs.contains(&entry.rel_path);
            let marker = if expanded { "▾ " } else { "▸ " };
            rows.push(RenderedRow {
                text: format!(" {indent}{marker}{}/", entry.name),
                right_text: None,
                style: Style::default().fg(palette.accent).bg(bg),
                hit: Some(RightPanelRowHit::ToggleDir(entry.rel_path.clone())),
            });
            if expanded {
                append_visible_tree_rows(state, &entry.rel_path, palette, rows);
            }
        } else {
            rows.push(RenderedRow {
                text: format!(" {indent}  {}", entry.name),
                right_text: None,
                style: Style::default().fg(palette.text).bg(bg),
                hit: Some(RightPanelRowHit::SelectFile(entry.rel_path.clone())),
            });
        }
    }
}

fn build_changes_rows(state: &RightPanelState, palette: &Palette) -> Vec<RenderedRow> {
    let mut rows = Vec::new();
    for file in &state.diff_files {
        let collapsed = state.collapsed_diff_files.contains(&file.path);
        let marker = if collapsed { "▸" } else { "▾" };
        let (code, color) = match file.status {
            ChangeKind::Modified => ("M", palette.yellow),
            ChangeKind::Added => ("A", palette.green),
            ChangeKind::Deleted => ("D", palette.red),
            ChangeKind::Renamed => ("R", palette.accent),
            ChangeKind::Untracked => ("?", palette.green),
        };
        let stats = if file.additions > 0 || file.deletions > 0 {
            Some((
                format!("+{} -{} ", file.additions, file.deletions),
                Style::default().fg(palette.overlay1).bg(palette.surface0),
            ))
        } else {
            None
        };
        rows.push(RenderedRow {
            text: format!(" {marker} {code} {}", file.path),
            right_text: stats,
            style: Style::default()
                .fg(color)
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD),
            hit: Some(RightPanelRowHit::ToggleDiffFile(file.path.clone())),
        });
        if collapsed {
            continue;
        }
        if file.lines.is_empty() {
            rows.push(RenderedRow {
                text: "   (no diff preview)".to_owned(),
                right_text: None,
                style: Style::default().fg(palette.overlay0).bg(palette.panel_bg),
                hit: None,
            });
            continue;
        }
        for line in &file.lines {
            let fg = if line.starts_with('+') {
                palette.green
            } else if line.starts_with('-') {
                palette.red
            } else if line.starts_with("@@") {
                palette.accent
            } else {
                palette.subtext0
            };
            rows.push(RenderedRow {
                text: format!("  {line}"),
                right_text: None,
                style: Style::default().fg(fg).bg(palette.panel_bg),
                hit: None,
            });
        }
    }
    if rows.is_empty() && !state.is_loading() && state.active_error().is_none() {
        rows.push(RenderedRow {
            text: "  No uncommitted changes".to_owned(),
            right_text: None,
            style: Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            hit: None,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ls_entries_sorts_dirs_first_and_skips_git() {
        let output = ".git/\nsrc/\nCargo.toml\nREADME.md\nassets/\n";
        let entries = parse_ls_entries("sub", 1, output);
        let summary: Vec<(&str, &str, bool, usize)> = entries
            .iter()
            .map(|e| (e.rel_path.as_str(), e.name.as_str(), e.is_dir, e.depth))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("sub/assets", "assets", true, 1),
                ("sub/src", "src", true, 1),
                ("sub/Cargo.toml", "Cargo.toml", false, 1),
                ("sub/README.md", "README.md", false, 1),
            ]
        );
    }

    #[test]
    fn parse_git_status_and_diff_extracts_branch_counts_and_hunks() {
        let status = "## master...origin/master [ahead 1]\n M src/lib.rs\n?? new.txt\n";
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 fn a() {}
-fn old() {}
+fn new_one() {}
+fn extra() {}
";
        let (branch, files) = parse_git_status_and_diff(status, diff);
        assert_eq!(branch.as_deref(), Some("master"));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].status, ChangeKind::Modified);
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert!(files[0].lines.iter().any(|line| line == "+fn extra() {}"));
        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].status, ChangeKind::Untracked);
    }

    fn test_shell_state() -> super::super::ClientShellState {
        let mut config =
            super::super::ClientShellConfig::from_config(&crate::config::Config::default());
        config.hide_tab_bar_when_single_tab = false;
        let mut state = super::super::ClientShellState::new(config);
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        let surface_buffer = Buffer::with_lines(["LIVE", "PANE"]);
        let pane_surface = crate::protocol::PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: crate::protocol::FrameData::from_ratatui_buffer_with_hyperlinks(
                &surface_buffer,
                None,
                &[],
            ),
            panes: vec![crate::protocol::PaneSurfacePane {
                pane_id: "pane_1".into(),
                content_revision: 0,
                rect: crate::protocol::SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                inner_rect: crate::protocol::SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
            popup: None,
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        };
        state.set_pane_surface(pane_surface);
        state
    }

    fn mouse_event(
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> crate::raw_input::RawInputEvent {
        crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
    }

    fn frame_text(frame: &crate::protocol::FrameData) -> String {
        frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn clicking_files_tab_opens_and_collapses_right_panel() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut state = test_shell_state();
        state.compose(120, 24).expect("initial frame");
        let files_btn = state.hits.tab_bar_files_button;
        assert!(files_btn.width > 0);
        let closed_layout = state.layout(120, 24);
        assert_eq!(closed_layout.right_panel.width, 0);

        state.handle_raw_events(vec![mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            files_btn.x + 1,
            files_btn.y,
        )]);
        assert_eq!(state.right_panel.active_tab, Some(RightPanelTab::Files));
        let open_layout = state.layout(120, 24);
        assert_eq!(open_layout.right_panel.width, 36);
        assert_eq!(
            open_layout.pane_surface.width,
            closed_layout.pane_surface.width - 36
        );

        state.compose(120, 24).expect("open files frame");
        let close_btn = state.hits.right_panel_close;
        assert!(close_btn.width > 0);
        state.handle_raw_events(vec![mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            close_btn.x + 1,
            close_btn.y,
        )]);
        assert_eq!(state.right_panel.active_tab, None);
        assert_eq!(state.layout(120, 24).right_panel.width, 0);
    }

    #[test]
    fn changes_tab_renders_diff_files_and_toggles_per_file_collapse() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut state = test_shell_state();
        let status = "## main\n M src/lib.rs\n";
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1,2 @@
 fn a() {}
+fn extra() {}
";
        let (branch, diff_files) = parse_git_status_and_diff(status, diff);
        state.right_panel.active_tab = Some(RightPanelTab::Changes);
        state.right_panel.branch = branch;
        state.right_panel.diff_files = diff_files;
        state.right_panel.changes_loaded = true;

        let open_frame = state.compose(120, 24).expect("changes frame");
        let open_text = frame_text(&open_frame);
        assert!(open_text.contains("src/lib.rs"));
        assert!(open_text.contains("+fn extra() {}"));

        let file_row = state
            .hits
            .right_panel_rows
            .iter()
            .find_map(|(rect, hit)| {
                matches!(hit, RightPanelRowHit::ToggleDiffFile(p) if p == "src/lib.rs")
                    .then_some(*rect)
            })
            .expect("diff file header hit");
        state.handle_raw_events(vec![mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            file_row.x + 1,
            file_row.y,
        )]);

        let collapsed_frame = state.compose(120, 24).expect("collapsed diff frame");
        let collapsed_text = frame_text(&collapsed_frame);
        assert!(collapsed_text.contains("src/lib.rs"));
        assert!(!collapsed_text.contains("+fn extra() {}"));
    }

    #[test]
    fn dragging_right_panel_divider_resizes_panel() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let mut state = test_shell_state();
        state.right_panel.active_tab = Some(RightPanelTab::Files);
        state.compose(120, 24).expect("open files frame");
        let divider = state.hits.right_panel_divider;
        assert_eq!(divider.width, 1);

        state.handle_raw_events(vec![
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                divider.x,
                divider.y + 1,
            ),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 76, divider.y + 1),
            mouse_event(MouseEventKind::Up(MouseButton::Left), 76, divider.y + 1),
        ]);
        assert_eq!(state.right_panel.width, 44);
        assert_eq!(state.layout(120, 24).right_panel.width, 44);
    }

    #[test]
    fn files_error_does_not_bleed_into_changes_tab() {
        let mut state = test_shell_state();
        state.right_panel.active_tab = Some(RightPanelTab::Files);
        state.right_panel.files_error = Some("remote ls failed".to_owned());
        let files_frame = state.compose(120, 24).expect("files error frame");
        assert!(frame_text(&files_frame).contains("remote ls failed"));

        state.right_panel.active_tab = Some(RightPanelTab::Changes);
        state.right_panel.changes_loaded = true;
        let changes_frame = state.compose(120, 24).expect("changes frame");
        let changes_text = frame_text(&changes_frame);
        assert!(!changes_text.contains("remote ls failed"));
        assert!(changes_text.contains("0 files"));
        assert!(changes_text.contains("No uncommitted changes"));
    }
}
