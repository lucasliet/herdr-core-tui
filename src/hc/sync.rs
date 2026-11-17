use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::layout::Direction;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::app::{App, AppState};
use crate::detect::AgentState;
use crate::events::AppEvent;
use crate::layout::{Node, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneShellConfig, PaneState};
use crate::render_signal::RenderSignal;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState, TerminalStateMutation};
use crate::terminal_theme::{HostAppearance, TerminalTheme};
use crate::workspace::{
    discover_workspace_git_identity, fallback_label_from_cwd, git_branch, MovedPane, Tab, Workspace,
};

use super::client::{
    get_pane_shared_id, get_space_id, get_tab_shared_id, get_workspace_machine,
    get_workspace_raw_name, is_address_known, local_doc_version, pane_address, pane_for_address,
    queue_pending_attach_for_pane, queue_pending_machine_for_pane, set_default_spawn_machine,
    set_local_doc_version, set_machine_doc_version, set_pane_shared_id, set_space_id,
    set_tab_shared_id, set_workspace_machine, set_workspace_raw_name, Core, EventLines, Stream,
};
use super::doc::{
    fresh_id, read_doc, write_doc, Document, HcAxis, HcContent, HcKind, HcNode, HcSize,
    SharedSpace, SharedTab,
};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

static LABELED_TERMINALS: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();

fn labeled_cache() -> &'static Mutex<HashMap<String, (String, String)>> {
    LABELED_TERMINALS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_terminal_label_cache(address: &str, space_id: &str, tab_id: &str) {
    let mut cache = labeled_cache().lock().unwrap_or_else(|p| p.into_inner());
    cache.insert(address.to_owned(), (space_id.to_owned(), tab_id.to_owned()));
}

fn needs_terminal_label_update(address: &str, space_id: &str, tab_id: &str) -> bool {
    let cache = labeled_cache().lock().unwrap_or_else(|p| p.into_inner());
    cache.get(address) != Some(&(space_id.to_owned(), tab_id.to_owned()))
}

pub struct RestoredHcSession {
    pub workspaces: Vec<Workspace>,
    pub terminals: HashMap<TerminalId, TerminalState>,
    pub terminal_runtimes: HashMap<TerminalId, TerminalRuntime>,
    pub active: Option<usize>,
    pub selected: usize,
}

pub struct HcDocSnapshot {
    spaces: Vec<SharedSpace>,
    labels: Vec<(String, String, String)>,
}

struct AttachContext<'a> {
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    host_terminal_theme: TerminalTheme,
    host_terminal_appearance: Option<HostAppearance>,
    shell_config: PaneShellConfig<'a>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
}

fn fallback_cwd() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn terminal_address(info: &Value) -> Option<&str> {
    info.get("address")
        .and_then(Value::as_str)
        .filter(|addr| !addr.is_empty())
}

fn terminal_is_closed(info: &Value) -> bool {
    info.get("closed").and_then(Value::as_bool) == Some(true)
        || info.get("state").and_then(Value::as_str) == Some("finished")
}

pub(crate) fn machine_of_address(addr: &str) -> Option<&str> {
    addr.split_once('/').map(|(machine, _)| machine)
}

pub(crate) fn qualify_address(machine: Option<&str>, addr: &str) -> String {
    if addr.contains('/') {
        return addr.to_owned();
    }
    match machine {
        Some(m) if !m.is_empty() => format!("{m}/{addr}"),
        _ => addr.to_owned(),
    }
}

pub(crate) fn format_workspace_display_name(machine: Option<&str>, raw_name: &str) -> String {
    match machine {
        Some(m) if !m.is_empty() => format!("{m} · {raw_name}"),
        _ => raw_name.to_owned(),
    }
}

fn strip_machine_prefix<'a>(machine: Option<&str>, name: &'a str) -> &'a str {
    let Some(m) = machine else {
        return name;
    };
    name.strip_prefix(m)
        .and_then(|rest| rest.strip_prefix(" · "))
        .unwrap_or(name)
}

fn infer_space_machine(space: &SharedSpace, doc_machine: Option<&str>) -> Option<String> {
    if let Some(m) = doc_machine.filter(|s| !s.is_empty()) {
        return Some(m.to_owned());
    }
    if let Some(m) = space
        .rest
        .get("machine")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(m.to_owned());
    }
    for tab in &space.tabs {
        for addr in tab.root.terminal_addresses() {
            if let Some(m) = machine_of_address(&addr) {
                return Some(m.to_owned());
            }
        }
    }
    None
}

fn terminal_cwd(info: &Value) -> PathBuf {
    info.pointer("/foreground/cwd")
        .and_then(Value::as_str)
        .or_else(|| info.get("cwd").and_then(Value::as_str))
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(fallback_cwd)
}

fn terminal_title(info: &Value) -> Option<String> {
    info.get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

fn terminal_label_str<'a>(info: &'a Value, key: &str) -> Option<&'a str> {
    info.get("labels")
        .and_then(|labels| labels.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|val| !val.is_empty())
}

pub(crate) fn map_hc_status(info: &Value) -> Option<(AgentState, String, Option<String>)> {
    let state_str = info.pointer("/status/state").and_then(Value::as_str)?;
    let state = match state_str {
        "working" => AgentState::Working,
        "blocked" => AgentState::Blocked,
        "done" | "idle" => AgentState::Idle,
        _ => return None,
    };
    let agent_label = info
        .pointer("/status/label")
        .and_then(Value::as_str)
        .or_else(|| info.pointer("/foreground/program").and_then(Value::as_str))
        .or_else(|| terminal_label_str(info, "herdr.agent"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("agent")
        .to_owned();
    let message = info
        .pointer("/status/message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some((state, agent_label, message))
}

fn apply_hc_metadata_mutation(
    terminal: &mut TerminalState,
    info: &Value,
) -> (Option<TerminalStateMutation>, bool) {
    let mut non_agent_changed = false;
    let cwd = terminal_cwd(info);
    if cwd.is_absolute() && terminal.cwd != cwd {
        terminal.cwd = cwd;
        non_agent_changed = true;
    }
    if let Some(title) = terminal_title(info) {
        if terminal.terminal_title.as_deref() != Some(title.as_str()) {
            terminal.set_terminal_title(Some(title));
            non_agent_changed = true;
        }
    }
    let mutation = if let Some((state, agent_label, message)) = map_hc_status(info) {
        let already_same = terminal.hook_authority.as_ref().is_some_and(|auth| {
            auth.source == "hc"
                && auth.agent_label == agent_label
                && auth.state == state
                && auth.message == message
        });
        if already_same {
            None
        } else {
            terminal.set_hook_authority_with_session_ref(
                "hc".to_owned(),
                agent_label,
                state,
                message,
                None,
                None,
            )
        }
    } else {
        terminal.clear_hook_authority_with_mutation(Some("hc"), None)
    };
    (mutation, non_agent_changed)
}

fn child_weight(node: &HcNode) -> f32 {
    match node.size {
        Some(HcSize::Weight { weight }) => weight.max(0.05),
        Some(HcSize::Px { px }) => px.max(1.0),
        None => 1.0,
    }
}

fn fold_weighted_children(direction: Direction, mut items: Vec<(Node, f32)>) -> Option<Node> {
    if items.is_empty() {
        return None;
    }
    if items.len() == 1 {
        return items.pop().map(|(node, _)| node);
    }
    let rest = items.split_off(1);
    let (first_node, first_weight) = items.pop()?;
    let rest_weight: f32 = rest
        .iter()
        .map(|(_, weight)| *weight)
        .sum::<f32>()
        .max(0.01);
    let total_weight = (first_weight + rest_weight).max(0.01);
    let ratio = (first_weight / total_weight).clamp(0.1, 0.9);
    let second_node = fold_weighted_children(direction, rest)?;
    Some(Node::Split {
        direction,
        ratio,
        first: Box::new(first_node),
        second: Box::new(second_node),
    })
}

pub(crate) fn convert_hc_node_to_layout<F>(
    node: &HcNode,
    resolve_pane: &mut F,
) -> Option<(Node, f32)>
where
    F: FnMut(&str, &str) -> Option<PaneId>,
{
    let weight = child_weight(node);
    match &node.kind {
        HcKind::Block {
            content: HcContent::Terminal { terminal },
        } => {
            let pane_id = resolve_pane(terminal, &node.id)?;
            Some((Node::Pane(pane_id), weight))
        }
        HcKind::Block { .. } => None,
        HcKind::Row { children, .. } => {
            let mapped: Vec<(Node, f32)> = children
                .iter()
                .filter_map(|child| convert_hc_node_to_layout(child, resolve_pane))
                .collect();
            fold_weighted_children(Direction::Horizontal, mapped).map(|root| (root, weight))
        }
        HcKind::Column { children, .. } | HcKind::Stack { children, .. } => {
            let mapped: Vec<(Node, f32)> = children
                .iter()
                .filter_map(|child| convert_hc_node_to_layout(child, resolve_pane))
                .collect();
            fold_weighted_children(Direction::Vertical, mapped).map(|root| (root, weight))
        }
    }
}

pub(crate) fn layout_node_to_hc_node<F>(node: &Node, resolve_addr: &mut F) -> Option<HcNode>
where
    F: FnMut(PaneId) -> Option<(String, String)>,
{
    match node {
        Node::Pane(pane_id) => {
            let (shared_id, address) = resolve_addr(*pane_id)?;
            Some(HcNode::terminal_block(&shared_id, &address))
        }
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let mut left = layout_node_to_hc_node(first, resolve_addr);
            let mut right = layout_node_to_hc_node(second, resolve_addr);
            match (left.take(), right.take()) {
                (Some(mut l), Some(mut r)) => {
                    let clamped = ratio.clamp(0.1, 0.9);
                    l.size = Some(HcSize::Weight { weight: clamped });
                    r.size = Some(HcSize::Weight {
                        weight: 1.0 - clamped,
                    });
                    let axis = match direction {
                        Direction::Horizontal => HcAxis::Row,
                        Direction::Vertical => HcAxis::Column,
                    };
                    Some(HcNode::container(&fresh_id("sp"), axis, vec![l, r]))
                }
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

fn spawn_hc_pane_runtime(
    ctx: &AttachContext<'_>,
    pane_id: PaneId,
    cwd: PathBuf,
) -> io::Result<TerminalRuntime> {
    TerminalRuntime::spawn(
        pane_id,
        ctx.rows,
        ctx.cols,
        cwd,
        ctx.scrollback_limit_bytes,
        ctx.host_terminal_theme,
        ctx.host_terminal_appearance,
        ctx.shell_config,
        &PaneLaunchEnv::default(),
        ctx.events.clone(),
        ctx.render_notify.clone(),
        ctx.render_dirty.clone(),
    )
}

fn spawn_hc_attached_pane(
    ctx: &AttachContext<'_>,
    info: &Value,
    shared_pane_id: Option<&str>,
) -> io::Result<(PaneId, TerminalState, TerminalRuntime)> {
    let address = terminal_address(info)
        .ok_or_else(|| io::Error::other("hc terminal entry missing address"))?
        .to_owned();
    let pane_id = PaneId::alloc();
    let cwd = terminal_cwd(info);
    queue_pending_attach_for_pane(pane_id, address);
    if let Some(shared_id) = shared_pane_id {
        set_pane_shared_id(pane_id, shared_id.to_owned());
    }
    let runtime = spawn_hc_pane_runtime(ctx, pane_id, cwd.clone())?;
    let terminal_id = TerminalId::alloc();
    let mut terminal = TerminalState::new(terminal_id, cwd);
    let _ = apply_hc_metadata_mutation(&mut terminal, info);
    Ok((pane_id, terminal, runtime))
}

fn respawn_hc_pane(
    ctx: &AttachContext<'_>,
    cwd: PathBuf,
    machine: Option<&str>,
    shared_pane_id: Option<&str>,
) -> io::Result<(PaneId, TerminalState, TerminalRuntime)> {
    let pane_id = PaneId::alloc();
    if let Some(m) = machine {
        queue_pending_machine_for_pane(pane_id, m.to_owned());
    }
    if let Some(shared_id) = shared_pane_id {
        set_pane_shared_id(pane_id, shared_id.to_owned());
    }
    let runtime = spawn_hc_pane_runtime(ctx, pane_id, cwd.clone())?;
    let terminal_id = TerminalId::alloc();
    let terminal = TerminalState::new(terminal_id, cwd);
    Ok((pane_id, terminal, runtime))
}

fn build_workspace_shell(
    custom_name: Option<String>,
    identity_cwd: PathBuf,
    tabs: Vec<Tab>,
    active_tab: usize,
) -> Workspace {
    let id = crate::workspace::generate_workspace_id();
    let mut public_pane_numbers = HashMap::new();
    let mut next_public_pane_number = 1;
    for tab in &tabs {
        for pane_id in tab.layout.pane_ids() {
            if !public_pane_numbers.contains_key(&pane_id) {
                public_pane_numbers.insert(pane_id, next_public_pane_number);
                next_public_pane_number += 1;
            }
        }
    }
    let next_public_tab_number = tabs.len() + 1;
    let active_tab = active_tab.min(tabs.len().saturating_sub(1));
    let (cached_git_space, cached_auto_label, cached_git_status_key) =
        discover_workspace_git_identity(&identity_cwd);
    Workspace {
        id,
        custom_name,
        identity_cwd: identity_cwd.clone(),
        cached_identity_cwd: identity_cwd.clone(),
        cached_auto_label,
        cached_git_status_key,
        cached_git_branch: git_branch(&identity_cwd),
        cached_git_ahead_behind: None,
        cached_git_space,
        worktree_space: None,
        metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
        metadata_token_sequences: HashMap::new(),
        public_pane_numbers,
        next_public_pane_number,
        next_public_tab_number,
        tabs,
        active_tab,
        #[cfg(test)]
        test_runtimes: HashMap::new(),
    }
}

fn build_tab_shell(
    number: usize,
    custom_name: Option<String>,
    node: Node,
    pane_terminal_ids: Vec<(PaneId, TerminalId)>,
    ctx: &AttachContext<'_>,
) -> Option<Tab> {
    let pane_ids = {
        let mut ids = Vec::new();
        let mut stack = vec![&node];
        while let Some(cur) = stack.pop() {
            match cur {
                Node::Pane(id) => ids.push(*id),
                Node::Split { first, second, .. } => {
                    stack.push(second);
                    stack.push(first);
                }
            }
        }
        ids
    };
    let root_pane = *pane_ids.first()?;
    let mut panes = HashMap::new();
    for (pane_id, terminal_id) in pane_terminal_ids {
        panes.insert(pane_id, PaneState::new(terminal_id));
    }
    Some(Tab {
        custom_name,
        number,
        root_pane,
        layout: TileLayout::from_saved(node, root_pane),
        panes,
        #[cfg(test)]
        runtimes: HashMap::new(),
        zoomed: false,
        events: ctx.events.clone(),
        render_notify: ctx.render_notify.clone(),
        render_dirty: ctx.render_dirty.clone(),
    })
}

fn spawn_hc_subscription_task(
    core: Arc<Core>,
    sub_channel: u32,
    mut rx: mpsc::UnboundedReceiver<Stream>,
    snapshot: Value,
    events: mpsc::Sender<AppEvent>,
) {
    let mut tracker = EventLines::new(&snapshot);
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            match item {
                Stream::Data(bytes) => {
                    core.credit(sub_channel, bytes.len());
                    let pushed = tracker.push(&bytes);
                    if pushed.docs || pushed.terminals {
                        let terminals = tracker.all_terminals().to_vec();
                        if events
                            .send(AppEvent::HcSync {
                                docs_changed: pushed.docs,
                                terminals,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Stream::Closed(_) | Stream::Lost => break,
            }
        }
    });
}

static UNRESTORED_OFFLINE_SPACES: OnceLock<Mutex<Vec<SharedSpace>>> = OnceLock::new();

fn unrestored_offline_spaces() -> &'static Mutex<Vec<SharedSpace>> {
    UNRESTORED_OFFLINE_SPACES.get_or_init(|| Mutex::new(Vec::new()))
}

fn collect_configured_machines(core: &Core) -> Vec<String> {
    match core.list_machines() {
        Ok(val) => val
            .get("machines")
            .and_then(Value::as_array)
            .or_else(|| val.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str))
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        Err(err) => {
            tracing::warn!(err = %err, "failed listing configured hc machines");
            Vec::new()
        }
    }
}

fn collect_initial_terminals(core: &Core, snapshot_terminals: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    let mut index_by_addr: HashMap<String, usize> = HashMap::new();
    let mut push_entry = |entry: Value| {
        if let Some(addr) = terminal_address(&entry).map(str::to_owned) {
            if let Some(&idx) = index_by_addr.get(&addr) {
                merged[idx] = entry;
            } else {
                index_by_addr.insert(addr, merged.len());
                merged.push(entry);
            }
        }
    };
    for entry in snapshot_terminals {
        push_entry(entry);
    }
    if let Ok(list) = core.list_terminals(false) {
        if let Some(items) = list.get("terminals").and_then(Value::as_array) {
            for entry in items {
                push_entry(entry.clone());
            }
        }
    }
    merged
}

#[allow(clippy::too_many_arguments)]
pub fn restore_from_hc(
    core: &Arc<Core>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    shell_config: PaneShellConfig<'_>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
) -> io::Result<RestoredHcSession> {
    let (sub_channel, rx, snapshot) = core.subscribe()?;
    let snapshot_terminals = snapshot
        .get("terminals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let initial_terminals = collect_initial_terminals(core, snapshot_terminals);

    let configured_machines = collect_configured_machines(core);

    let (local_doc, doc_version) = match read_doc(core, None) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(err = %err, "failed reading herdr-app doc on startup; starting with empty doc");
            (Document::default(), 0)
        }
    };
    set_local_doc_version(doc_version);

    let mut docs_to_restore: Vec<(Option<String>, Document)> = vec![(None, local_doc)];
    let mut seen_space_ids: HashSet<String> = docs_to_restore[0]
        .1
        .spaces
        .iter()
        .map(|space| space.id.clone())
        .collect();
    for machine in &configured_machines {
        if let Ok((mut remote_doc, remote_ver)) = read_doc(core, Some(machine)) {
            set_machine_doc_version(Some(machine), remote_ver);
            remote_doc
                .spaces
                .retain(|space| seen_space_ids.insert(space.id.clone()));
            if !remote_doc.spaces.is_empty() {
                docs_to_restore.push((Some(machine.clone()), remote_doc));
            }
        }
    }

    let ctx = AttachContext {
        rows: rows.max(DEFAULT_ROWS),
        cols: cols.max(DEFAULT_COLS),
        scrollback_limit_bytes,
        host_terminal_theme: TerminalTheme::default(),
        host_terminal_appearance: None,
        shell_config,
        events: events.clone(),
        render_notify,
        render_dirty,
    };

    let mut live_by_addr: HashMap<&str, &Value> = HashMap::new();
    let mut finished_addrs: HashSet<&str> = HashSet::new();
    for term in &initial_terminals {
        let Some(addr) = terminal_address(term) else {
            continue;
        };
        if terminal_is_closed(term) {
            finished_addrs.insert(addr);
        } else {
            live_by_addr.insert(addr, term);
        }
    }

    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut terminal_runtimes = HashMap::new();
    let mut mapped_addresses = HashSet::new();
    let mut preserved_offline_spaces = Vec::new();

    for (doc_machine, doc) in &docs_to_restore {
        for space in &doc.spaces {
            let space_machine = infer_space_machine(space, doc_machine.as_deref());
            let mut tabs = Vec::new();
            let mut tab_shared_ids = Vec::new();
            let trimmed_folder = space.folder.trim();
            let mut identity_cwd =
                (!trimmed_folder.is_empty()).then(|| PathBuf::from(trimmed_folder));

            for (tab_idx, shared_tab) in space.tabs.iter().enumerate() {
                let mut pane_pairs = Vec::new();
                let Some((layout_node, _)) =
                    convert_hc_node_to_layout(&shared_tab.root, &mut |raw_addr, shared_pane_id| {
                        let qualified_addr = qualify_address(space_machine.as_deref(), raw_addr);
                        if mapped_addresses.contains(&qualified_addr) {
                            return None;
                        }
                        let attach_attempt = if finished_addrs.contains(qualified_addr.as_str()) {
                            None
                        } else if let Some(info) = live_by_addr.get(qualified_addr.as_str()) {
                            Some(spawn_hc_attached_pane(&ctx, info, Some(shared_pane_id)))
                        } else {
                            let synthetic = json!({
                                "address": qualified_addr,
                                "cwd": space.folder,
                            });
                            Some(spawn_hc_attached_pane(
                                &ctx,
                                &synthetic,
                                Some(shared_pane_id),
                            ))
                        };

                        let pane_result = match attach_attempt {
                            Some(Ok(res)) => Ok((res, Some(qualified_addr.clone()), true)),
                            Some(Err(_)) | None => {
                                let target_cwd =
                                    identity_cwd.clone().unwrap_or_else(|| PathBuf::from("~"));
                                respawn_hc_pane(
                                    &ctx,
                                    target_cwd,
                                    space_machine.as_deref(),
                                    Some(shared_pane_id),
                                )
                                .map(|res| (res, None, false))
                            }
                        };

                        match pane_result {
                            Ok(((pane_id, term_state, runtime), attached_addr, was_attached)) => {
                                if identity_cwd.is_none() {
                                    identity_cwd = Some(term_state.cwd.clone());
                                }
                                let term_id = term_state.id.clone();
                                if let Some(addr) = attached_addr.as_deref() {
                                    mapped_addresses.insert(addr.to_owned());
                                    if was_attached {
                                        if let Some(info) = live_by_addr.get(addr) {
                                            if terminal_label_str(info, "herdr.space")
                                                == Some(space.id.as_str())
                                                && terminal_label_str(info, "herdr.tab")
                                                    == Some(shared_tab.id.as_str())
                                            {
                                                record_terminal_label_cache(
                                                    addr,
                                                    &space.id,
                                                    &shared_tab.id,
                                                );
                                            }
                                        }
                                    }
                                }
                                terminals.insert(term_id.clone(), term_state);
                                terminal_runtimes.insert(term_id.clone(), runtime);
                                pane_pairs.push((pane_id, term_id));
                                Some(pane_id)
                            }
                            Err(err) => {
                                tracing::warn!(
                                    address = %qualified_addr,
                                    machine = ?space_machine,
                                    err = %err,
                                    "failed restoring hc pane from layout"
                                );
                                None
                            }
                        }
                    })
                else {
                    continue;
                };

                let number = tab_idx + 1;
                let tab_custom_name =
                    (!shared_tab.name.trim().is_empty()).then(|| shared_tab.name.trim().to_owned());
                if let Some(tab) =
                    build_tab_shell(number, tab_custom_name, layout_node, pane_pairs, &ctx)
                {
                    tab_shared_ids.push((number, shared_tab.id.clone()));
                    tabs.push(tab);
                }
            }

            if tabs.is_empty() {
                if space_machine.is_some() {
                    preserved_offline_spaces.push(space.clone());
                }
                continue;
            }
            let ws_cwd = identity_cwd.unwrap_or_else(fallback_cwd);
            let fallback_name = fallback_label_from_cwd(&ws_cwd);
            let raw_name = if !space.name.trim().is_empty() {
                space.name.trim().to_owned()
            } else {
                fallback_name.clone()
            };
            let custom_name = if space_machine.is_some() {
                Some(format_workspace_display_name(
                    space_machine.as_deref(),
                    &raw_name,
                ))
            } else {
                (raw_name != fallback_name).then(|| raw_name.clone())
            };
            let ws = build_workspace_shell(custom_name, ws_cwd, tabs, 0);
            set_space_id(&ws.id, space.id.clone());
            set_workspace_machine(&ws.id, space_machine);
            set_workspace_raw_name(&ws.id, raw_name);
            for (number, tab_shared_id) in tab_shared_ids {
                set_tab_shared_id(&ws.id, number, tab_shared_id);
            }
            workspaces.push(ws);
        }
    }

    *unrestored_offline_spaces()
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = preserved_offline_spaces;

    for info in &initial_terminals {
        if terminal_is_closed(info) {
            continue;
        }
        let Some(addr) = terminal_address(info) else {
            continue;
        };
        if mapped_addresses.contains(addr) {
            continue;
        }
        let Ok((pane_id, term_state, runtime)) = spawn_hc_attached_pane(&ctx, info, None) else {
            continue;
        };
        mapped_addresses.insert(addr.to_owned());
        let term_id = term_state.id.clone();
        let pane_cwd = term_state.cwd.clone();
        terminals.insert(term_id.clone(), term_state);
        terminal_runtimes.insert(term_id.clone(), runtime);

        let shared_pane_id = fresh_id("p");
        set_pane_shared_id(pane_id, shared_pane_id);

        let labeled_space = terminal_label_str(info, "herdr.space").map(str::to_owned);
        let labeled_tab = terminal_label_str(info, "herdr.tab").map(str::to_owned);
        let machine = machine_of_address(addr).map(str::to_owned);
        let moved = MovedPane {
            pane_id,
            pane_state: PaneState::new(term_id),
        };

        let existing_ws = labeled_space.as_deref().and_then(|space_id| {
            workspaces
                .iter_mut()
                .find(|ws| get_space_id(&ws.id).as_deref() == Some(space_id))
        });

        if let Some(ws) = existing_ws {
            let tab_idx = ws.create_tab_from_existing_pane(
                moved,
                None,
                ctx.events.clone(),
                ctx.render_notify.clone(),
                ctx.render_dirty.clone(),
            );
            let tab_number = ws.tabs[tab_idx].number;
            let shared_tab_id = labeled_tab.unwrap_or_else(|| fresh_id("tab"));
            set_tab_shared_id(&ws.id, tab_number, shared_tab_id.clone());
            if let Some(space_id) = get_space_id(&ws.id) {
                record_terminal_label_cache(addr, &space_id, &shared_tab_id);
            }
        } else {
            let raw_name = fallback_label_from_cwd(&pane_cwd);
            let label = machine
                .as_deref()
                .map(|m| format_workspace_display_name(Some(m), &raw_name));
            let ws = Workspace::from_existing_pane(
                label,
                None,
                pane_cwd,
                moved,
                ctx.events.clone(),
                ctx.render_notify.clone(),
                ctx.render_dirty.clone(),
            );
            let shared_space_id = labeled_space.unwrap_or_else(|| fresh_id("s"));
            let shared_tab_id = labeled_tab.unwrap_or_else(|| fresh_id("tab"));
            set_space_id(&ws.id, shared_space_id.clone());
            set_workspace_machine(&ws.id, machine);
            set_workspace_raw_name(&ws.id, raw_name);
            set_tab_shared_id(&ws.id, ws.tabs[0].number, shared_tab_id.clone());
            record_terminal_label_cache(addr, &shared_space_id, &shared_tab_id);
            workspaces.push(ws);
        }
    }

    for machine in configured_machines {
        let already_present = workspaces
            .iter()
            .any(|ws| get_workspace_machine(&ws.id).as_deref() == Some(machine.as_str()));
        if already_present {
            continue;
        }
        set_default_spawn_machine(Some(machine.clone()));
        let created = Workspace::new(
            PathBuf::from("~"),
            ctx.rows,
            ctx.cols,
            ctx.scrollback_limit_bytes,
            ctx.host_terminal_theme,
            ctx.host_terminal_appearance,
            ctx.shell_config,
            ctx.events.clone(),
            ctx.render_notify.clone(),
            ctx.render_dirty.clone(),
        );
        set_default_spawn_machine(None);
        match created {
            Ok((mut ws, term_state, runtime)) => {
                let space_id = fresh_id("s");
                let tab_id = fresh_id("tab");
                let pane_id = ws.tabs[0].root_pane;
                set_space_id(&ws.id, space_id);
                set_workspace_machine(&ws.id, Some(machine.clone()));
                set_workspace_raw_name(&ws.id, "~".to_owned());
                ws.custom_name = Some(format_workspace_display_name(Some(&machine), "~"));
                set_tab_shared_id(&ws.id, ws.tabs[0].number, tab_id);
                set_pane_shared_id(pane_id, fresh_id("p"));
                let term_id = term_state.id.clone();
                terminals.insert(term_id.clone(), term_state);
                terminal_runtimes.insert(term_id, runtime);
                workspaces.push(ws);
            }
            Err(err) => {
                tracing::warn!(
                    machine = %machine,
                    err = %err,
                    "failed spawning initial workspace on configured hc remote machine"
                );
            }
        }
    }

    spawn_hc_subscription_task(core.clone(), sub_channel, rx, snapshot, events);

    let active = (!workspaces.is_empty()).then_some(0);
    Ok(RestoredHcSession {
        workspaces,
        terminals,
        terminal_runtimes,
        active,
        selected: 0,
    })
}

pub fn capture_hc_doc_snapshot(workspaces: &[Workspace]) -> HcDocSnapshot {
    let mut spaces = Vec::with_capacity(workspaces.len());
    let mut labels = Vec::new();
    let mut active_space_ids = HashSet::new();

    for ws in workspaces {
        let machine = get_workspace_machine(&ws.id);
        let space_id = get_space_id(&ws.id).unwrap_or_else(|| {
            let id = fresh_id("s");
            set_space_id(&ws.id, id.clone());
            id
        });
        let name = ws
            .custom_name
            .as_deref()
            .map(str::trim)
            .map(|n| strip_machine_prefix(machine.as_deref(), n))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| get_workspace_raw_name(&ws.id))
            .unwrap_or_else(|| {
                if ws.cached_auto_label.is_empty() {
                    fallback_label_from_cwd(&ws.identity_cwd)
                } else {
                    ws.cached_auto_label.clone()
                }
            });

        let mut shared_tabs = Vec::with_capacity(ws.tabs.len());
        for tab in &ws.tabs {
            let tab_id = get_tab_shared_id(&ws.id, tab.number).unwrap_or_else(|| {
                let id = fresh_id("tab");
                set_tab_shared_id(&ws.id, tab.number, id.clone());
                id
            });
            let Some(root) = layout_node_to_hc_node(tab.layout.root(), &mut |pane_id| {
                let raw_address = pane_address(pane_id)?;
                let address = qualify_address(machine.as_deref(), &raw_address);
                let shared_id = get_pane_shared_id(pane_id).unwrap_or_else(|| {
                    let id = fresh_id("p");
                    set_pane_shared_id(pane_id, id.clone());
                    id
                });
                labels.push((address.clone(), space_id.clone(), tab_id.clone()));
                Some((shared_id, address))
            }) else {
                continue;
            };
            let tab_name = tab
                .custom_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Tab {}", tab.number));
            shared_tabs.push(SharedTab {
                id: tab_id,
                name: tab_name,
                root,
                rest: Default::default(),
            });
        }
        if shared_tabs.is_empty() {
            continue;
        }
        active_space_ids.insert(space_id.clone());
        spaces.push(SharedSpace {
            id: space_id,
            name,
            folder: ws.identity_cwd.display().to_string(),
            tabs: shared_tabs,
            rest: Default::default(),
        });
    }

    let preserved = unrestored_offline_spaces()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for space in preserved.iter() {
        if active_space_ids.insert(space.id.clone()) {
            spaces.push(space.clone());
        }
    }

    HcDocSnapshot { spaces, labels }
}

pub fn write_hc_doc_snapshot(core: &Core, snapshot: HcDocSnapshot) {
    for (address, space_id, tab_id) in snapshot.labels {
        if needs_terminal_label_update(&address, &space_id, &tab_id) {
            if let Err(err) = core.label_terminal(
                &address,
                json!({
                    "herdr.space": space_id,
                    "herdr.tab": tab_id,
                }),
            ) {
                tracing::warn!(address = %address, err = %err, "failed to label hc terminal");
            } else {
                record_terminal_label_cache(&address, &space_id, &tab_id);
            }
        }
    }

    let (mut doc, current_ver) = match read_doc(core, None) {
        Ok(pair) => pair,
        Err(_) => (Document::default(), local_doc_version()),
    };

    let mut prev_spaces: HashMap<String, SharedSpace> = doc
        .spaces
        .into_iter()
        .map(|space| (space.id.clone(), space))
        .collect();

    doc.spaces = snapshot
        .spaces
        .into_iter()
        .map(|mut space| {
            if let Some(prev) = prev_spaces.remove(&space.id) {
                space.rest = prev.rest;
                let mut prev_tabs: HashMap<String, SharedTab> = prev
                    .tabs
                    .into_iter()
                    .map(|tab| (tab.id.clone(), tab))
                    .collect();
                for tab in &mut space.tabs {
                    if let Some(prev_tab) = prev_tabs.remove(&tab.id) {
                        tab.rest = prev_tab.rest;
                    }
                }
            }
            space
        })
        .collect();

    match write_doc(core, None, current_ver, &doc) {
        Ok(new_ver) => set_local_doc_version(new_ver),
        Err(err) => tracing::warn!(err = %err, "failed to sync workspaces to herdr-app doc"),
    }
}

pub fn sync_workspaces_to_hc(core: &Core, workspaces: &[Workspace]) {
    let snapshot = capture_hc_doc_snapshot(workspaces);
    write_hc_doc_snapshot(core, snapshot);
}

pub fn apply_hc_metadata_to_state(
    state: &mut AppState,
    terminals: &[Value],
) -> (Vec<crate::app::actions::PaneStateUpdate>, bool) {
    let mut updates = Vec::new();
    let mut dirty = false;

    for info in terminals {
        let Some(addr) = terminal_address(info) else {
            continue;
        };
        let Some(pane_id) = pane_for_address(addr) else {
            continue;
        };
        let mut non_agent_changed = false;
        let maybe_update = state.update_terminal_state(pane_id, |terminal| {
            let (mutation, meta_changed) = apply_hc_metadata_mutation(terminal, info);
            non_agent_changed = meta_changed;
            mutation
        });
        if let Some(update) = maybe_update {
            updates.push(update);
            dirty = true;
        } else if non_agent_changed {
            dirty = true;
        }
    }

    (updates, dirty)
}

fn adopt_unmapped_hc_terminal(app: &mut App, info: &Value) -> bool {
    let Some(addr) = terminal_address(info) else {
        return false;
    };
    let pane_id = PaneId::alloc();
    let cwd = terminal_cwd(info);
    queue_pending_attach_for_pane(pane_id, addr.to_owned());
    set_pane_shared_id(pane_id, fresh_id("p"));

    let runtime = match TerminalRuntime::spawn(
        pane_id,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        cwd.clone(),
        app.state.pane_scrollback_limit_bytes,
        app.state.host_terminal_theme,
        app.pane_host_appearance,
        PaneShellConfig::new(&app.state.default_shell, app.state.shell_mode),
        &PaneLaunchEnv::default(),
        app.event_tx.clone(),
        app.render_notify.clone(),
        app.render_dirty.clone(),
    ) {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::warn!(address = addr, err = %err, "failed to attach external hc terminal");
            return false;
        }
    };

    let terminal_id = TerminalId::alloc();
    let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone());
    let _ = apply_hc_metadata_mutation(&mut terminal, info);
    app.state.terminals.insert(terminal_id.clone(), terminal);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);

    let labeled_space = terminal_label_str(info, "herdr.space").map(str::to_owned);
    let labeled_tab = terminal_label_str(info, "herdr.tab").map(str::to_owned);
    let moved = MovedPane {
        pane_id,
        pane_state: PaneState::new(terminal_id),
    };

    let matching_ws = labeled_space.as_deref().and_then(|space_id| {
        app.state
            .workspaces
            .iter_mut()
            .find(|ws| get_space_id(&ws.id).as_deref() == Some(space_id))
    });

    if let Some(ws) = matching_ws {
        let tab_idx = ws.create_tab_from_existing_pane(
            moved,
            None,
            app.event_tx.clone(),
            app.render_notify.clone(),
            app.render_dirty.clone(),
        );
        let tab_number = ws.tabs[tab_idx].number;
        let shared_tab_id = labeled_tab.unwrap_or_else(|| fresh_id("tab"));
        set_tab_shared_id(&ws.id, tab_number, shared_tab_id.clone());
        if let Some(space_id) = get_space_id(&ws.id) {
            record_terminal_label_cache(addr, &space_id, &shared_tab_id);
        }
    } else {
        let raw_name = fallback_label_from_cwd(&cwd);
        let machine = machine_of_address(addr).map(str::to_owned);
        let label = machine
            .as_deref()
            .map(|m| format_workspace_display_name(Some(m), &raw_name));
        let ws = Workspace::from_existing_pane(
            label,
            None,
            cwd,
            moved,
            app.event_tx.clone(),
            app.render_notify.clone(),
            app.render_dirty.clone(),
        );
        let shared_space_id = labeled_space.unwrap_or_else(|| fresh_id("s"));
        let shared_tab_id = labeled_tab.unwrap_or_else(|| fresh_id("tab"));
        set_space_id(&ws.id, shared_space_id.clone());
        set_workspace_machine(&ws.id, machine);
        set_workspace_raw_name(&ws.id, raw_name);
        set_tab_shared_id(&ws.id, ws.tabs[0].number, shared_tab_id.clone());
        record_terminal_label_cache(addr, &shared_space_id, &shared_tab_id);
        app.state.workspaces.push(ws);
        if app.state.active.is_none() {
            app.state.selected = 0;
            app.state.active = Some(0);
        }
    }

    true
}

pub fn handle_hc_sync(
    app: &mut App,
    docs_changed: bool,
    terminals: Vec<Value>,
) -> Vec<crate::app::actions::PaneStateUpdate> {
    let (updates, mut dirty) = apply_hc_metadata_to_state(&mut app.state, &terminals);
    for update in &updates {
        app.emit_pane_state_update(update);
    }

    let mut adopted_any = false;
    for info in &terminals {
        if terminal_is_closed(info) {
            continue;
        }
        let Some(addr) = terminal_address(info) else {
            continue;
        };
        if !is_address_known(addr) && adopt_unmapped_hc_terminal(app, info) {
            adopted_any = true;
            dirty = true;
        }
    }

    if adopted_any {
        app.state.mark_session_dirty();
    }

    if docs_changed {
        if let Some(core) = super::global_core() {
            if let Ok((_, version)) = read_doc(&core, None) {
                set_local_doc_version(version);
            }
        }
    }

    if dirty {
        app.render_dirty.request_generic();
        app.render_notify.notify_one();
    }

    updates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hc_node_and_layout_node_roundtrip_preserves_splits_and_panes() {
        let p1 = PaneId::from_raw(101);
        let p2 = PaneId::from_raw(102);
        let p3 = PaneId::from_raw(103);

        let doc_tree = HcNode::container(
            "root",
            HcAxis::Row,
            vec![
                HcNode::terminal_block("n1", "t1"),
                HcNode::container(
                    "right",
                    HcAxis::Column,
                    vec![
                        HcNode::terminal_block("n2", "t2"),
                        HcNode::terminal_block("n3", "t3"),
                    ],
                ),
            ],
        );

        let mut addr_to_pane = HashMap::from([("t1", p1), ("t2", p2), ("t3", p3)]);
        let (layout_root, _) =
            convert_hc_node_to_layout(&doc_tree, &mut |addr, _| addr_to_pane.remove(addr)).unwrap();

        let pane_meta = HashMap::from([
            (p1, ("n1".to_owned(), "t1".to_owned())),
            (p2, ("n2".to_owned(), "t2".to_owned())),
            (p3, ("n3".to_owned(), "t3".to_owned())),
        ]);
        let exported =
            layout_node_to_hc_node(&layout_root, &mut |id| pane_meta.get(&id).cloned()).unwrap();

        assert_eq!(exported.terminal_addresses(), vec!["t1", "t2", "t3"]);
        match exported.kind {
            HcKind::Row { children, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[1].kind, HcKind::Column { .. }));
            }
            other => panic!("expected row split, got {other:?}"),
        }
    }

    #[test]
    fn hc_status_maps_to_hook_authority_and_clears() {
        let mut terminal = TerminalState::new(TerminalId::alloc(), PathBuf::from("/tmp"));

        let working = json!({
            "address": "t1",
            "title": "Claude Code",
            "status": { "state": "working", "label": "claude" }
        });
        let (mutation, changed) = apply_hc_metadata_mutation(&mut terminal, &working);
        assert!(changed);
        assert!(mutation.is_some());
        assert_eq!(terminal.state, AgentState::Working);

        let blocked = json!({
            "address": "t1",
            "status": { "state": "blocked", "label": "claude", "message": "Approve edit?" }
        });
        let (mutation, _) = apply_hc_metadata_mutation(&mut terminal, &blocked);
        assert!(mutation.is_some());
        assert_eq!(terminal.state, AgentState::Blocked);

        let done = json!({
            "address": "t1",
            "status": { "state": "done", "label": "claude" }
        });
        let _ = apply_hc_metadata_mutation(&mut terminal, &done);
        assert_eq!(terminal.state, AgentState::Idle);

        let plain = json!({ "address": "t1" });
        let _ = apply_hc_metadata_mutation(&mut terminal, &plain);
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn remote_machine_address_and_display_name_helpers_work() {
        assert_eq!(machine_of_address("desk-local/t6"), Some("desk-local"));
        assert_eq!(machine_of_address("t1"), None);

        assert_eq!(qualify_address(Some("desk-local"), "t6"), "desk-local/t6");
        assert_eq!(
            qualify_address(Some("desk-local"), "desk-local/t6"),
            "desk-local/t6"
        );
        assert_eq!(qualify_address(None, "t1"), "t1");

        let display = format_workspace_display_name(Some("oracle"), "BattleShip");
        assert_eq!(display, "oracle · BattleShip");
        assert_eq!(strip_machine_prefix(Some("oracle"), &display), "BattleShip");
        assert_eq!(strip_machine_prefix(None, "BattleShip"), "BattleShip");

        assert!(terminal_is_closed(
            &json!({ "address": "t1", "state": "finished" })
        ));
        assert!(terminal_is_closed(
            &json!({ "address": "t1", "closed": true })
        ));
        assert!(!terminal_is_closed(
            &json!({ "address": "t1", "state": "live" })
        ));
    }
}
