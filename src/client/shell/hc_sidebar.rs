use super::sidebar::{
    displayed_workspace_status, entries_for_workspace_indices, render_parent_group_toggle,
    render_workspace_footer, render_workspace_rows, workspace_rows,
};
use super::*;

struct HcMachineGroup {
    machine: Option<String>,
    label: String,
    connected: bool,
    indices: Vec<usize>,
}

enum GroupedRow {
    MachineHeader {
        machine: Option<String>,
        label: String,
        connected: bool,
        collapsed: bool,
    },
    Workspace(WorkspaceEntry),
}

pub(in crate::client::shell) fn is_grouped_mode(snapshot: &ClientShellSnapshot) -> bool {
    !snapshot.hc_machines.is_empty()
        || snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.machine.is_some())
}

pub(in crate::client::shell) fn grouped_workspace_entries(
    snapshot: &ClientShellSnapshot,
    collapsed_groups: &HashSet<String>,
    collapsed_hc_machines: &HashSet<Option<String>>,
) -> Vec<WorkspaceEntry> {
    let mut entries = Vec::new();
    for group in machine_groups(snapshot) {
        if collapsed_hc_machines.contains(&group.machine) {
            continue;
        }
        entries.extend(entries_for_workspace_indices(
            snapshot,
            &group.indices,
            collapsed_groups,
        ));
    }
    entries
}

pub(in crate::client::shell) fn active_target_machine(
    snapshot: &ClientShellSnapshot,
    selected_hc_machine: Option<&Option<String>>,
) -> Option<String> {
    if let Some(selected) = selected_hc_machine {
        return selected.clone();
    }
    let focused_id = snapshot.focused_workspace_id.as_deref()?;
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == focused_id)
        .and_then(|workspace| workspace.machine.clone())
}

pub(in crate::client::shell) fn render_grouped_sidebar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    render_sidebar_background(buffer, area, palette);
    hits.sidebar_divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    let (workspace_area, detail_area) =
        crate::ui::expanded_sidebar_sections(area, state.sidebar_section_split);
    hits.sidebar_section_divider =
        crate::ui::sidebar_section_divider_rect(area, state.sidebar_section_split);
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        " spaces",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );

    let (rows, row_heights, gaps) = build_grouped_rows(snapshot, config, state);
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    hits.workspace_body = body;
    let mut metrics = super::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *state.workspace_scroll,
    );
    if !body.is_empty() && std::mem::take(state.reveal_focused_workspace) {
        if let Some(target) = rows.iter().position(|row| match row {
            GroupedRow::Workspace(entry) => snapshot
                .workspaces
                .get(entry.index)
                .is_some_and(|workspace| workspace.focused),
            GroupedRow::MachineHeader { .. } => false,
        }) {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
            metrics = super::scroll::list_scroll_metrics(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
            );
        }
    }
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let active_machine = active_target_machine(snapshot, state.selected_hc_machine);
    let mut y = body.y;

    for (row_pos, row) in rows.iter().enumerate().skip(*state.workspace_scroll) {
        let row_height = row_heights.get(row_pos).copied().unwrap_or(1);
        if y.saturating_add(row_height) > body.bottom() {
            break;
        }
        match row {
            GroupedRow::MachineHeader {
                machine,
                label,
                connected,
                collapsed,
            } => {
                let rect = Rect::new(body.x, y, content_width, 1);
                render_machine_header(
                    buffer,
                    rect,
                    machine,
                    label,
                    *connected,
                    *collapsed,
                    &active_machine == machine,
                    config.mouse_capture,
                    palette,
                    hits,
                );
            }
            GroupedRow::Workspace(entry) => {
                if let Some(workspace) = snapshot.workspaces.get(entry.index) {
                    render_grouped_workspace_row(
                        buffer,
                        body,
                        y,
                        content_width,
                        row_height,
                        snapshot,
                        workspace,
                        entry,
                        config,
                        state,
                        hits,
                    );
                }
            }
        }
        let gap = gaps.get(row_pos).copied().unwrap_or(0);
        y = y.saturating_add(row_height + gap);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }

    let footer_label = match active_machine.as_deref() {
        Some(machine) => format!(" new · {machine}"),
        None => " new".to_owned(),
    };
    render_workspace_footer(
        buffer,
        workspace_area,
        snapshot,
        config,
        &footer_label,
        hits,
    );

    super::render_agent_panel(
        buffer,
        detail_area,
        snapshot,
        config,
        state.agent_scroll,
        hits,
    );

    hits.sidebar_toggle = Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        u16::from(area.height > 0),
    );
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "«",
        Style::default().fg(palette.overlay0),
    );
}

fn machine_groups(snapshot: &ClientShellSnapshot) -> Vec<HcMachineGroup> {
    let mut local_indices = Vec::new();
    let mut by_machine: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut orphan_order: Vec<&str> = Vec::new();
    let known: HashSet<&str> = snapshot
        .hc_machines
        .iter()
        .map(|machine| machine.name.as_str())
        .collect();

    for (index, workspace) in snapshot.workspaces.iter().enumerate() {
        match workspace.machine.as_deref() {
            None => local_indices.push(index),
            Some(name) => {
                if !known.contains(name) && !by_machine.contains_key(name) {
                    orphan_order.push(name);
                }
                by_machine.entry(name).or_default().push(index);
            }
        }
    }

    let mut groups = vec![HcMachineGroup {
        machine: None,
        label: "This computer".to_owned(),
        connected: true,
        indices: local_indices,
    }];
    let mut seen = HashSet::new();
    for machine in &snapshot.hc_machines {
        if !seen.insert(machine.name.as_str()) {
            continue;
        }
        groups.push(HcMachineGroup {
            machine: Some(machine.name.clone()),
            label: machine.name.clone(),
            connected: machine.connected,
            indices: by_machine.remove(machine.name.as_str()).unwrap_or_default(),
        });
    }
    for name in orphan_order {
        groups.push(HcMachineGroup {
            machine: Some(name.to_owned()),
            label: name.to_owned(),
            connected: false,
            indices: by_machine.remove(name).unwrap_or_default(),
        });
    }
    groups
}

fn build_grouped_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
) -> (Vec<GroupedRow>, Vec<u16>, Vec<u16>) {
    let mut rows = Vec::new();
    let mut row_heights = Vec::new();
    for group in machine_groups(snapshot) {
        let collapsed = state.collapsed_hc_machines.contains(&group.machine);
        rows.push(GroupedRow::MachineHeader {
            machine: group.machine,
            label: group.label,
            connected: group.connected,
            collapsed,
        });
        row_heights.push(1);
        if collapsed {
            continue;
        }
        for entry in entries_for_workspace_indices(snapshot, &group.indices, state.collapsed_groups)
        {
            let height = snapshot
                .workspaces
                .get(entry.index)
                .map(|workspace| {
                    workspace_rows(
                        workspace,
                        displayed_workspace_status(snapshot, workspace, state.collapsed_groups),
                        entry.indented,
                        &config.spaces,
                    )
                    .len()
                    .max(1)
                    .min(u16::MAX as usize) as u16
                })
                .unwrap_or(1);
            rows.push(GroupedRow::Workspace(entry));
            row_heights.push(height);
        }
    }

    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, row)| match (row, rows.get(index + 1)) {
            (GroupedRow::Workspace(_), Some(GroupedRow::Workspace(next))) => {
                u16::from(!next.indented) * config.spaces.row_gap
            }
            _ => 0,
        })
        .collect();
    (rows, row_heights, gaps)
}

#[allow(clippy::too_many_arguments)]
fn render_machine_header(
    buffer: &mut Buffer,
    rect: Rect,
    machine: &Option<String>,
    label: &str,
    connected: bool,
    collapsed: bool,
    is_target: bool,
    mouse_capture: bool,
    palette: &Palette,
    hits: &mut ShellHitMap,
) {
    let marker = if collapsed { "▸" } else { "▾" };
    let show_add = mouse_capture && rect.width >= 10;
    let right_reserved: u16 = if show_add { 5 } else { 3 };
    let label_width = rect.width.saturating_sub(right_reserved);
    let label_color = if !connected {
        palette.overlay0
    } else if is_target {
        palette.text
    } else {
        palette.subtext0
    };
    put_text(
        buffer,
        rect.x,
        rect.y,
        label_width,
        &format!(" {marker} {label}"),
        Style::default()
            .fg(label_color)
            .add_modifier(Modifier::BOLD),
    );

    let add_button = if show_add {
        let add_x = rect.right().saturating_sub(4);
        let add_style = Style::default().fg(if is_target {
            palette.accent
        } else {
            palette.overlay0
        });
        put_text(buffer, add_x, rect.y, 1, "+", add_style);
        Rect::new(add_x, rect.y, 1, 1)
    } else {
        Rect::default()
    };

    if rect.width >= 4 {
        let (dot, dot_color) = if connected {
            ("●", palette.green)
        } else {
            ("○", palette.overlay0)
        };
        put_text(
            buffer,
            rect.right().saturating_sub(2),
            rect.y,
            1,
            dot,
            Style::default().fg(dot_color),
        );
    }

    hits.hc_machines.push(HcMachineHit {
        rect,
        collapse_toggle: Rect::new(rect.x, rect.y, 2.min(rect.width), 1),
        add_button,
        machine: machine.clone(),
    });
}

#[allow(clippy::too_many_arguments)]
fn render_grouped_workspace_row(
    buffer: &mut Buffer,
    body: Rect,
    y: u16,
    content_width: u16,
    row_height: u16,
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    entry: &WorkspaceEntry,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let status = displayed_workspace_status(snapshot, workspace, state.collapsed_groups);
    let rows = workspace_rows(workspace, status, entry.indented, &config.spaces);
    let full_rect = Rect::new(body.x, y, content_width, row_height);
    let indent = u16::from(content_width > 6);
    let inner_rect = Rect::new(
        body.x.saturating_add(indent),
        y,
        content_width.saturating_sub(indent),
        row_height,
    );
    let selected = state
        .selected_workspace_id
        .is_some_and(|target| target.matches(state.active_endpoint_id, &workspace.workspace_id));
    let dragged = state.dragged_workspace_id == Some(workspace.workspace_id.as_str());
    let bg = if selected {
        Some(if palette.selection_bg == ratatui::style::Color::Reset {
            palette.active_row_bg
        } else {
            palette.selection_bg
        })
    } else if dragged {
        Some(palette.surface1)
    } else if workspace.focused {
        Some(super::sidebar::workspace_active_background(
            palette,
            state.selected_workspace_id.is_some(),
        ))
    } else {
        None
    };
    if let Some(bg) = bg {
        buffer.set_style(full_rect, Style::default().bg(bg));
    }
    render_workspace_rows(
        buffer,
        inner_rect,
        status,
        config.status_indicators,
        entry,
        rows,
        workspace.focused,
        selected,
        state.selected_workspace_id.is_some(),
        dragged,
        palette,
    );
    let group_toggle = render_parent_group_toggle(
        buffer,
        inner_rect,
        snapshot,
        entry.index,
        state.collapsed_groups,
        palette,
    );
    hits.workspaces.push(WorkspaceHit {
        rect: full_rect,
        endpoint_id: ClientEndpointId::Local,
        workspace_id: workspace.workspace_id.clone(),
        indented: entry.indented,
        group_toggle,
    });
}
