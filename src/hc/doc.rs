use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::client::{error_kind, Core};

static ID_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn fresh_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let hi = nanos ^ (pid << 16);
    let lo = seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ nanos.rotate_left(17);
    format!("{prefix}{hi:016x}{lo:016x}")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Document {
    #[serde(default)]
    pub spaces: Vec<SharedSpace>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedSpace {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub tabs: Vec<SharedTab>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedTab {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "empty_root")]
    pub root: HcNode,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

fn empty_root() -> HcNode {
    HcNode::container("", HcAxis::Row, vec![])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HcAxis {
    Row,
    Column,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HcContent {
    Terminal { terminal: String },
    Other(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HcSize {
    Px { px: f32 },
    Weight { weight: f32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HcKind {
    Block {
        content: HcContent,
    },
    Row {
        children: Vec<HcNode>,
    },
    Column {
        children: Vec<HcNode>,
    },
    Stack {
        children: Vec<HcNode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HcNode {
    #[serde(default)]
    pub id: String,
    #[serde(flatten)]
    pub kind: HcKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<HcSize>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl HcNode {
    pub fn terminal_block(id: &str, terminal: &str) -> Self {
        Self {
            id: id.to_owned(),
            kind: HcKind::Block {
                content: HcContent::Terminal {
                    terminal: terminal.to_owned(),
                },
            },
            size: None,
            rest: Map::new(),
        }
    }

    pub fn container(id: &str, axis: HcAxis, children: Vec<HcNode>) -> Self {
        let kind = match axis {
            HcAxis::Row => HcKind::Row { children },
            HcAxis::Column => HcKind::Column { children },
        };
        Self {
            id: id.to_owned(),
            kind,
            size: None,
            rest: Map::new(),
        }
    }

    pub fn terminal_addresses(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_terminals(&mut out);
        out
    }

    fn collect_terminals(&self, out: &mut Vec<String>) {
        match &self.kind {
            HcKind::Block {
                content: HcContent::Terminal { terminal },
            } => out.push(terminal.clone()),
            HcKind::Block { .. } => {}
            HcKind::Row { children }
            | HcKind::Column { children }
            | HcKind::Stack { children, .. } => {
                for child in children {
                    child.collect_terminals(out);
                }
            }
        }
    }
}

pub fn read_doc(core: &Core, machine: Option<&str>) -> io::Result<(Document, u64)> {
    let result = core.call_timeout(
        "doc.get",
        json!({ "name": "herdr-app", "on": machine }),
        Duration::from_secs(10),
    )?;
    let document = if result["value"].is_null() {
        Document::default()
    } else {
        serde_json::from_value(result["value"].clone())
            .map_err(|e| io::Error::other(format!("cannot decode herdr-app doc: {e}")))?
    };
    let version = result["version"]
        .as_u64()
        .ok_or_else(|| io::Error::other("herdr-app doc missing version"))?;
    Ok((document, version))
}

pub fn write_doc(
    core: &Core,
    machine: Option<&str>,
    version: u64,
    doc: &Document,
) -> io::Result<u64> {
    let value = serde_json::to_value(doc)
        .map_err(|e| io::Error::other(format!("cannot encode herdr-app doc: {e}")))?;
    let mut current_version = version;
    for attempt in 0..3 {
        match core.call_timeout(
            "doc.set",
            json!({
                "name": "herdr-app",
                "on": machine,
                "version": current_version,
                "value": value
            }),
            Duration::from_secs(10),
        ) {
            Ok(v) => {
                return v["version"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("doc.set returned no version"));
            }
            Err(e) if error_kind(&e) == Some("conflict") && attempt < 2 => {
                let (_, fresh_ver) = read_doc(core, machine)?;
                current_version = fresh_ver;
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("doc.set failed after conflict retries"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_roundtrip_preserves_herdr_app_shape_and_extra_fields() {
        let raw = json!({
            "spaces": [{
                "id": "sp123",
                "name": "api",
                "folder": "/srv/api",
                "extra_space_field": 42,
                "tabs": [{
                    "id": "tb456",
                    "name": "dev",
                    "root": {
                        "id": "tb456",
                        "type": "row",
                        "children": [
                            { "id": "pn1", "type": "block", "content": { "terminal": "t1" }, "size": { "weight": 0.4 } },
                            { "id": "pn2", "type": "block", "content": { "mod": "notes", "props": { "file": "README.md" } }, "size": { "weight": 0.6 } }
                        ]
                    }
                }]
            }]
        });
        let doc: Document = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(doc.spaces.len(), 1);
        assert_eq!(
            doc.spaces[0].tabs[0].root.terminal_addresses(),
            vec!["t1".to_string()]
        );
        let serialized = serde_json::to_value(&doc).unwrap();
        assert_eq!(serialized["spaces"][0]["extra_space_field"], 42);
        assert_eq!(
            serialized["spaces"][0]["tabs"][0]["root"]["children"][1]["content"]["mod"],
            "notes"
        );
    }
}
