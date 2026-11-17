use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::layout::PaneId;

const HEADER: usize = 9;
const MAX_PAYLOAD: usize = 16 << 20;

pub const DATA: u8 = 0;
pub const CREDIT: u8 = 1;
pub const CLOSE: u8 = 2;
pub const RESIZE: u8 = 3;
#[allow(dead_code)] // Wire frame constant retained for herdr-core protocol parity.
pub const FOCUS: u8 = 4;
#[allow(dead_code)] // Wire frame constant retained for herdr-core protocol parity.
pub const FACTS: u8 = 5;
pub const AUTOMATIC_INPUT: u8 = 6;

pub const GENERATION: u64 = 1;
pub const USES: &[&str] = &[
    "terminal.run",
    "terminal.list",
    "terminal.kill",
    "terminal.label",
    "terminal.attach",
    "terminal.keep",
    "doc.get",
    "doc.set",
    "events.subscribe",
];

#[derive(Debug, Clone)]
pub enum Stream {
    Data(Vec<u8>),
    Closed(#[allow(dead_code)] Value),
    Lost,
}

#[derive(Default)]
pub struct EventLines {
    pending: Vec<u8>,
    seq: u64,
    terminals: Vec<Value>,
    seen: HashMap<String, u64>,
    resynced: bool,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pushed {
    pub docs: bool,
    pub terminals: bool,
}

impl EventLines {
    pub fn new(snapshot: &Value) -> Self {
        let terminals = snapshot["terminals"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        EventLines {
            seq: snapshot["seq"].as_u64().unwrap_or(0),
            terminals,
            resynced: true,
            ..Default::default()
        }
    }

    #[allow(dead_code)] // Retained for herdr-core event tracking parity.
    pub fn take_seen(&mut self) -> (HashMap<String, u64>, bool) {
        (std::mem::take(&mut self.seen), std::mem::take(&mut self.resynced))
    }

    fn note_seen(&mut self, address: &Value, labels: &Value) {
        let (Some(address), Some(seen)) = (
            address.as_str(),
            labels["herdr.seen"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok()),
        ) else {
            return;
        };
        let high = self.seen.entry(address.to_owned()).or_default();
        *high = (*high).max(seen);
    }

    pub fn all_terminals(&self) -> &[Value] {
        &self.terminals
    }

    #[allow(dead_code)] // Retained for herdr-core event snapshot parity.
    pub fn live(&self) -> Value {
        let live: Vec<&Value> = self
            .terminals
            .iter()
            .filter(|t| t["state"] == "live")
            .collect();
        json!({ "terminals": live })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Pushed {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut pushed = Pushed::default();
        let lines: Vec<Value> = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| {
                let line = serde_json::from_slice::<Value>(&self.pending[consumed..i])
                    .unwrap_or_default();
                consumed = i + 1;
                line
            })
            .collect();
        self.pending.drain(..consumed);
        for line in lines {
            let p = &line["params"];
            match line["method"].as_str() {
                Some("missed") => {
                    self.terminals = p["terminals"].as_array().cloned().unwrap_or_default();
                    self.seq = p["seq"].as_u64().unwrap_or(self.seq);
                    self.resynced = true;
                    pushed.docs = true;
                    pushed.terminals = true;
                }
                Some("event") if p["seq"].as_u64().is_none_or(|s| s > self.seq) => {
                    self.seq = p["seq"].as_u64().unwrap_or(self.seq);
                    if p["type"] == "doc" {
                        pushed.docs = true;
                    } else {
                        pushed.terminals |= self.apply(p);
                    }
                }
                _ => {}
            }
        }
        pushed
    }

    fn apply(&mut self, p: &Value) -> bool {
        let (kind, address, data) = (
            p["type"].as_str().unwrap_or_default(),
            &p["terminal"],
            &p["data"],
        );
        if matches!(kind, "created" | "restarted" | "labels") {
            self.note_seen(address, &data["labels"]);
        }
        let at = self
            .terminals
            .iter()
            .position(|t| &t["address"] == address);
        let whole = match kind {
            "created" | "restarted" => Some(data),
            "labels" => data.get("record"),
            _ => None,
        };
        if let Some(record) = whole {
            match at {
                Some(i) => self.terminals[i] = record.clone(),
                None => self.terminals.push(record.clone()),
            }
            return true;
        }
        let Some(i) = at else {
            return false;
        };
        if kind == "removed" {
            self.terminals.remove(i);
            return true;
        }
        let t = &mut self.terminals[i];
        let mut set = |keys: &[&str]| keys.iter().for_each(|k| t[*k] = data[*k].clone());
        match kind {
            "finished" | "lost" => {
                set(&["reason", "exit_code", "signal", "finished_ms"]);
                t["state"] = if kind == "lost" { "live" } else { "finished" }.into();
                if kind == "lost" {
                    t["pid"] = Value::Null;
                }
                if let Some(obj) = t.as_object_mut() {
                    obj.remove("foreground");
                }
            }
            "status" => set(&["status", "records", "reported"]),
            "turn" => set(&["turn"]),
            "title" => set(&["title"]),
            "labels" => set(&["labels"]),
            "keep" => set(&["keep"]),
            "size" => set(&["cols", "rows"]),
            "foreground" => t["foreground"] = data.clone(),
            _ => return false,
        }
        true
    }
}

#[derive(Default)]
struct Routes {
    closed: bool,
    automatic_input: HashSet<u32>,
    replies: HashMap<u64, mpsc::Sender<Result<Value, Value>>>,
    streams: HashMap<u32, UnboundedSender<Stream>>,
    early: HashMap<u32, Vec<Stream>>,
}

#[derive(Debug)]
pub struct RpcError(pub Value);

impl RpcError {
    pub fn kind(&self) -> Option<&str> {
        self.0.pointer("/data/kind").and_then(Value::as_str)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            self.0
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("core call failed")
        )?;
        if let Some(hint) = self.0.pointer("/data/hint").and_then(Value::as_str) {
            if !hint.is_empty() {
                write!(f, " ({hint})")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

pub fn error_kind(error: &io::Error) -> Option<&str> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<RpcError>())
        .and_then(RpcError::kind)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mismatch {
    Newer { generation: u64 },
    OlderGeneration { generation: u64 },
    Missing(Vec<String>),
}

pub fn check_hello(hello: &Value) -> Result<(), Mismatch> {
    let generation = hello
        .get("generation")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generation > GENERATION {
        return Err(Mismatch::Newer { generation });
    }
    if generation < GENERATION {
        return Err(Mismatch::OlderGeneration { generation });
    }
    let methods: Vec<&str> = hello
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let missing: Vec<String> = USES
        .iter()
        .filter(|m| !methods.contains(m))
        .map(|m| (*m).to_string())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Mismatch::Missing(missing))
    }
}

pub struct Core {
    pub hello: Value,
    writer: mpsc::Sender<Vec<u8>>,
    routes: Arc<Mutex<Routes>>,
    next_id: AtomicU64,
    child: Mutex<Child>,
    #[allow(dead_code)] // Broadcast channel kept alive for lost-core listeners.
    lost: Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
}

pub struct Attached {
    pub channel: u32,
    pub stream: UnboundedReceiver<Stream>,
    pub terminal: Value,
    #[allow(dead_code)] // Retained for attached channel capability inspection.
    pub automatic_input: bool,
}

pub fn encode(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn read_frame(r: &mut impl Read) -> Option<(u32, u8, Vec<u8>)> {
    let mut h = [0u8; HEADER];
    r.read_exact(&mut h).ok()?;
    let channel = u32::from_le_bytes(h[0..4].try_into().ok()?);
    let len = u32::from_le_bytes(h[5..9].try_into().ok()?) as usize;
    if len > MAX_PAYLOAD {
        return None;
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).ok()?;
    Some((channel, h[4], payload))
}

fn connect_error(stderr: &str) -> Option<String> {
    let reply: Value = serde_json::from_str(stderr.trim()).ok()?;
    let error = reply.get("error")?;
    let message = error.get("message")?.as_str()?;
    Some(
        match error
            .get("hint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(hint) => format!("{message}\n\n{hint}"),
            None => message.to_owned(),
        },
    )
}

impl Core {
    pub fn connect(session: Option<&str>) -> io::Result<Arc<Core>> {
        let mut cmd = Command::new(super::core_bin::path());
        cmd.arg("connect");
        if let Some(session) = session.filter(|s| !s.is_empty()) {
            cmd.args(["--session", session]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("cannot run `hc connect`: is herdr-core installed? ({err})"),
                )
            })?;

        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing hc stderr pipe"))?;
        let (error_tx, errors) = mpsc::channel();
        std::thread::Builder::new()
            .name("hc-stderr".into())
            .spawn(move || {
                let mut captured = Vec::new();
                let mut buffer = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    let remaining = 65536usize.saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..n.min(remaining)]);
                }
                let _ = error_tx.send(String::from_utf8_lossy(&captured).into_owned());
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing hc stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing hc stdout pipe"))?;
        let mut reader = BufReader::new(stdout);
        let hello_result = (|| -> io::Result<Value> {
            stdin.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\",\"params\":{\"framed\":true}}\n",
            )?;
            stdin.flush()?;
            let mut line = String::new();
            reader.read_line(&mut line)?;
            serde_json::from_str(&line)
                .map_err(|err| io::Error::other(format!("bad hello reply from core: {err}")))
        })();
        let hello = match hello_result {
            Ok(hello) => hello,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(msg) =
                    connect_error(&errors.recv_timeout(Duration::from_secs(1)).unwrap_or_default())
                {
                    return Err(io::Error::other(msg));
                }
                return Err(error);
            }
        };
        if let Some(err) = hello.get("error") {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("core refused hello: {err}")));
        }
        let hello_value = hello.get("result").cloned().unwrap_or(Value::Null);
        if let Err(mismatch) = check_hello(&hello_value) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!(
                "incompatible herdr-core API: {mismatch:?}"
            )));
        }

        let (writer, frames) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("hc-writer".into())
            .spawn(move || {
                while let Ok(frame) = frames.recv() {
                    if stdin.write_all(&frame).is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
            })?;

        let routes = Arc::new(Mutex::new(Routes::default()));
        let (lost_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let core = Arc::new(Core {
            hello: hello_value,
            writer,
            routes: routes.clone(),
            next_id: AtomicU64::new(1),
            child: Mutex::new(child),
            lost: Mutex::new(Some(lost_tx.clone())),
        });
        std::thread::Builder::new()
            .name("hc-reader".into())
            .spawn(move || {
                while let Some((channel, kind, payload)) = read_frame(&mut reader) {
                    let mut r = routes.lock().unwrap_or_else(|p| p.into_inner());
                    if channel == 0 {
                        let Ok(msg) = serde_json::from_slice::<Value>(&payload) else {
                            continue;
                        };
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        if let Some(tx) = r.replies.remove(&id) {
                            let _ = tx.send(match msg.get("error") {
                                Some(e) => Err(e.clone()),
                                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                            });
                        }
                        continue;
                    }
                    let item = match kind {
                        DATA => Stream::Data(payload),
                        CLOSE => Stream::Closed(
                            serde_json::from_slice(&payload).unwrap_or(Value::Null),
                        ),
                        _ => continue,
                    };
                    let closing = matches!(item, Stream::Closed(_));
                    match r.streams.get(&channel) {
                        Some(tx) => {
                            let _ = tx.send(item);
                        }
                        None => r.early.entry(channel).or_default().push(item),
                    }
                    if closing {
                        r.automatic_input.remove(&channel);
                        r.streams.remove(&channel);
                    }
                }
                let mut r = routes.lock().unwrap_or_else(|p| p.into_inner());
                r.closed = true;
                r.automatic_input.clear();
                for (_, tx) in r.streams.drain() {
                    let _ = tx.send(Stream::Lost);
                }
                for (_, tx) in r.replies.drain() {
                    let _ = tx.send(Err(json!({"message": "lost the core"})));
                }
                let _ = lost_tx.send(());
            })?;
        Ok(core)
    }

    #[allow(dead_code)] // Retained for herdr-core lifecycle notifications.
    pub fn subscribe_lost(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        self.lost
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(tokio::sync::broadcast::Sender::subscribe)
    }

    pub fn send(&self, channel: u32, kind: u8, payload: &[u8]) {
        let _ = self.writer.send(encode(channel, kind, payload));
    }

    pub fn call(&self, method: &str, params: Value) -> io::Result<Value> {
        self.call_timeout(method, params, Duration::from_secs(10))
    }

    pub fn call_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> io::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
            if routes.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("{method}: lost the core"),
                ));
            }
            routes.replies.insert(id, tx);
        }
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send(0, DATA, req.to_string().as_bytes());
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(io::Error::other(RpcError(e))),
            Err(_) => {
                self.routes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .replies
                    .remove(&id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{method}: no reply from core"),
                ))
            }
        }
    }

    pub fn list_terminals(&self, all: bool) -> io::Result<Value> {
        self.call("terminal.list", json!({ "all": all }))
    }

    pub fn list_machines(&self) -> io::Result<Value> {
        self.call("machine.list", json!({}))
    }

    pub fn run(
        &self,
        argv: Vec<String>,
        cwd: Option<String>,
        labels: Value,
        on: Option<&str>,
        size: Option<(u16, u16)>,
        display: Value,
    ) -> io::Result<String> {
        let env = json!({
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "TERM_PROGRAM": "herdr"
        });
        let mut params = json!({
            "argv": argv,
            "cwd": cwd,
            "env": env,
            "labels": labels,
            "on": on,
            "keep": true,
            "display": display
        });
        if let Some((cols, rows)) = size {
            params["size"] = json!({ "cols": cols, "rows": rows });
        }
        let r = self.call("terminal.run", params)?;
        r.get("address")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other(format!("terminal.run returned no address: {r}")))
    }

    pub fn kill_terminal(&self, terminal: &str) -> io::Result<Value> {
        self.call("terminal.kill", json!({ "terminal": terminal }))
    }

    pub fn rm_terminal(&self, terminal: &str) -> io::Result<Value> {
        self.call("terminal.rm", json!({ "terminal": terminal }))
    }

    pub fn label_terminal(&self, terminal: &str, labels: Value) -> io::Result<Value> {
        self.call(
            "terminal.label",
            json!({ "terminal": terminal, "labels": labels }),
        )
    }

    #[allow(dead_code)] // Retained for herdr-core RPC parity.
    pub fn keep_terminal(&self, terminal: &str, off: bool) -> io::Result<Value> {
        self.call(
            "terminal.keep",
            json!({ "terminal": terminal, "off": off }),
        )
    }

    pub fn subscribe(&self) -> io::Result<(u32, UnboundedReceiver<Stream>, Value)> {
        let result = self.call("events.subscribe", json!({}))?;
        let channel = result["channel"]
            .as_u64()
            .ok_or_else(|| io::Error::other("subscription returned no channel"))?
            as u32;
        let (tx, rx) = unbounded_channel();
        let mut r = self.routes.lock().unwrap_or_else(|p| p.into_inner());
        for item in r.early.remove(&channel).unwrap_or_default() {
            let _ = tx.send(item);
        }
        r.streams.insert(channel, tx);
        drop(r);
        self.credit(channel, 1024 * 1024);
        Ok((channel, rx, result))
    }

    pub fn attach(
        &self,
        terminal: &str,
        cols: u16,
        rows: u16,
        display: Value,
    ) -> io::Result<Attached> {
        let result = self.call(
            "terminal.attach",
            json!({
                "terminal": terminal,
                "cols": cols,
                "rows": rows,
                "history": 2000,
                "display": display
            }),
        )?;
        let channel = result["channel"]
            .as_u64()
            .ok_or_else(|| io::Error::other("attach returned no channel"))?
            as u32;
        let (tx, rx) = unbounded_channel();
        let mut r = self.routes.lock().unwrap_or_else(|p| p.into_inner());
        for item in r.early.remove(&channel).unwrap_or_default() {
            let _ = tx.send(item);
        }
        r.streams.insert(channel, tx);
        let automatic_input = self.hello["features"]
            .as_array()
            .is_some_and(|features| features.iter().any(|f| f == "automatic_input"))
            && result["automatic_input"] == true;
        if automatic_input {
            r.automatic_input.insert(channel);
        }
        Ok(Attached {
            channel,
            stream: rx,
            terminal: result["terminal"].clone(),
            automatic_input,
        })
    }

    pub fn input(&self, channel: u32, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.send(channel, DATA, bytes);
        }
    }

    pub fn automatic_input(&self, channel: u32, bytes: &[u8]) {
        if !bytes.is_empty() {
            let kind = if self
                .routes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automatic_input
                .contains(&channel)
            {
                AUTOMATIC_INPUT
            } else {
                DATA
            };
            self.send(channel, kind, bytes);
        }
    }

    pub fn credit(&self, channel: u32, n: usize) {
        self.send(channel, CREDIT, &(n as u32).to_le_bytes());
    }

    pub fn resize(&self, channel: u32, cols: u16, rows: u16) {
        let mut p = cols.to_le_bytes().to_vec();
        p.extend(rows.to_le_bytes());
        self.send(channel, RESIZE, &p);
    }

    #[allow(dead_code)] // Retained for herdr-core display hint wire parity.
    pub fn facts(&self, channel: u32, display: &Value) {
        self.send(channel, FACTS, display.to_string().as_bytes());
    }

    #[allow(dead_code)] // Retained for herdr-core focus hint wire parity.
    pub fn focus(&self, channel: u32, focused: bool) {
        self.send(channel, FOCUS, &[focused as u8]);
    }

    pub fn detach(&self, channel: u32) {
        let mut routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
        routes.streams.remove(&channel);
        routes.automatic_input.remove(&channel);
        drop(routes);
        self.send(channel, CLOSE, &[]);
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

static GLOBAL_CORE: OnceLock<Mutex<Option<Arc<Core>>>> = OnceLock::new();

pub fn set_global_core(core: Option<Arc<Core>>) {
    let lock = GLOBAL_CORE.get_or_init(|| Mutex::new(None));
    *lock.lock().unwrap_or_else(|p| p.into_inner()) = core;
}

pub fn global_core() -> Option<Arc<Core>> {
    GLOBAL_CORE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

#[derive(Default)]
struct HcRegistry {
    pane_to_addr: HashMap<PaneId, String>,
    addr_to_pane: HashMap<String, PaneId>,
    pending_attach: HashMap<PaneId, String>,
    pending_labels: HashMap<PaneId, Value>,
    pending_machine: HashMap<PaneId, String>,
    default_spawn_machine: Option<String>,
    ws_to_space_id: HashMap<String, String>,
    ws_to_machine: HashMap<String, String>,
    ws_to_raw_name: HashMap<String, String>,
    tab_to_shared_id: HashMap<(String, usize), String>,
    pane_to_shared_id: HashMap<PaneId, String>,
    doc_versions_by_machine: HashMap<Option<String>, u64>,
}

static REGISTRY: OnceLock<Mutex<HcRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<HcRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(HcRegistry::default()))
}

pub fn reset_registry() {
    let mut r = registry().lock().unwrap_or_else(|p| p.into_inner());
    *r = HcRegistry::default();
}

pub fn register_pane_address(pane_id: PaneId, address: String) {
    let mut r = registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(old) = r.pane_to_addr.insert(pane_id, address.clone()) {
        if old != address {
            r.addr_to_pane.remove(&old);
        }
    }
    r.addr_to_pane.insert(address, pane_id);
}

pub fn unregister_pane(pane_id: PaneId) {
    let mut r = registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(addr) = r.pane_to_addr.remove(&pane_id) {
        r.addr_to_pane.remove(&addr);
    }
    r.pending_attach.remove(&pane_id);
    r.pending_labels.remove(&pane_id);
    r.pane_to_shared_id.remove(&pane_id);
}

pub fn pane_address(pane_id: PaneId) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pane_to_addr
        .get(&pane_id)
        .cloned()
}

pub fn pane_for_address(address: &str) -> Option<PaneId> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .addr_to_pane
        .get(address)
        .copied()
}

pub fn is_address_known(address: &str) -> bool {
    let guard = registry().lock().unwrap_or_else(|p| p.into_inner());
    guard.addr_to_pane.contains_key(address) || guard.pending_attach.values().any(|a| a == address)
}

pub fn queue_pending_attach_for_pane(pane_id: PaneId, address: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_attach
        .insert(pane_id, address);
}

pub fn take_pending_attach_for_pane(pane_id: PaneId) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_attach
        .remove(&pane_id)
}

#[allow(dead_code)] // Registry helper retained for pane launch label overrides.
pub fn queue_pending_labels_for_pane(pane_id: PaneId, labels: Value) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_labels
        .insert(pane_id, labels);
}

pub fn take_pending_labels_for_pane(pane_id: PaneId) -> Option<Value> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_labels
        .remove(&pane_id)
}

pub fn set_space_id(ws_id: &str, space_id: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ws_to_space_id
        .insert(ws_id.to_owned(), space_id);
}

pub fn get_space_id(ws_id: &str) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ws_to_space_id
        .get(ws_id)
        .cloned()
}

pub fn set_tab_shared_id(ws_id: &str, tab_number: usize, shared_id: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .tab_to_shared_id
        .insert((ws_id.to_owned(), tab_number), shared_id);
}

pub fn get_tab_shared_id(ws_id: &str, tab_number: usize) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .tab_to_shared_id
        .get(&(ws_id.to_owned(), tab_number))
        .cloned()
}

pub fn set_pane_shared_id(pane_id: PaneId, shared_id: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pane_to_shared_id
        .insert(pane_id, shared_id);
}

pub fn get_pane_shared_id(pane_id: PaneId) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pane_to_shared_id
        .get(&pane_id)
        .cloned()
}

pub fn set_local_doc_version(version: u64) {
    set_machine_doc_version(None, version);
}

pub fn local_doc_version() -> u64 {
    machine_doc_version(None)
}

pub fn set_machine_doc_version(machine: Option<&str>, version: u64) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .doc_versions_by_machine
        .insert(machine.map(str::to_owned), version);
}

pub fn machine_doc_version(machine: Option<&str>) -> u64 {
    let key = machine.map(str::to_owned);
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .doc_versions_by_machine
        .get(&key)
        .copied()
        .unwrap_or(0)
}

pub fn set_workspace_machine(ws_id: &str, machine: Option<String>) {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    match machine {
        Some(m) => {
            reg.ws_to_machine.insert(ws_id.to_owned(), m);
        }
        None => {
            reg.ws_to_machine.remove(ws_id);
        }
    }
}

pub fn get_workspace_machine(ws_id: &str) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ws_to_machine
        .get(ws_id)
        .cloned()
}

pub fn set_workspace_raw_name(ws_id: &str, name: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ws_to_raw_name
        .insert(ws_id.to_owned(), name);
}

pub fn get_workspace_raw_name(ws_id: &str) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ws_to_raw_name
        .get(ws_id)
        .cloned()
}

pub fn queue_pending_machine_for_pane(pane_id: PaneId, machine: String) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_machine
        .insert(pane_id, machine);
}

pub fn take_pending_machine_for_pane(pane_id: PaneId) -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending_machine
        .remove(&pane_id)
}

pub fn set_default_spawn_machine(machine: Option<String>) {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .default_spawn_machine = machine;
}

pub fn default_spawn_machine() -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .default_spawn_machine
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_stream_tracks_terminal_updates_across_frame_boundaries() {
        let event = |seq: u64, kind: &str, t: &str, data: Value| {
            format!(
                "{}\n",
                json!({"method": "event", "params": {"seq": seq, "type": kind, "terminal": t, "data": data}})
            )
        };
        let rec = |a: &str| json!({"address": a, "state": "live", "title": "", "labels": {}, "status": null});
        let mut lines = EventLines::new(&json!({"seq": 5, "terminals": [rec("t1")]}));
        assert_eq!(lines.live()["terminals"].as_array().unwrap().len(), 1);
        let chunk = format!(
            "{}{}",
            event(6, "created", "t2", rec("t2")),
            event(7, "title", "t1", json!({"title": "hi"}))
        );
        let mid = chunk.len() / 2;
        let p1 = lines.push(&chunk.as_bytes()[..mid]);
        let p2 = lines.push(&chunk.as_bytes()[mid..]);
        assert!(p1.terminals || p2.terminals);
        assert_eq!(lines.live()["terminals"][0]["title"], "hi");
        assert_eq!(lines.live()["terminals"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn frame_encode_and_read_roundtrip() {
        let payload = b"hello world";
        let bytes = encode(42, DATA, payload);
        let mut cursor = std::io::Cursor::new(bytes);
        let (ch, kind, decoded) = read_frame(&mut cursor).unwrap();
        assert_eq!(ch, 42);
        assert_eq!(kind, DATA);
        assert_eq!(decoded, payload);
    }
}
