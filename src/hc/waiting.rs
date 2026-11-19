//! Background probe worker for presentation-only `AgentStatus::Waiting`.
//!
//! Probes live reported `idle`/`done` `hc` terminals via two layers:
//! 1. `machine.exec` (`ps -A -o pid=,ppid=,etime=,args=`): flags descendants running
//!    a `-c` shell spawned at least 20s after the agent across two consecutive scans.
//! 2. `file.read` (`~/.claude/sessions/<pid>.json`): confirms Claude Code's `status == "shell"`.
//!
//! Verdicts never mutate `AgentState`, `pane.seen`, or completion notifications.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::events::AppEvent;

use super::client::{set_waiting_addresses, Core};

pub(crate) const PS_ARGV: &[&str] = &["ps", "-A", "-o", "pid=,ppid=,etime=,args="];
pub(crate) const STARTUP_WINDOW_SECS: u64 = 20;
const ACTIVE_CADENCE: Duration = Duration::from_secs(3);
const QUIET_CADENCE: Duration = Duration::from_secs(10);
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer2 {
    ClaudeSessions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub pid: i32,
    pub created_ms: u64,
    pub word: String,
    pub status_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbePlan {
    pub address: String,
    pub machine: Option<String>,
    pub fingerprint: Fingerprint,
    pub process_tree: bool,
    pub layer2: Option<Layer2>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub etime: u64,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone)]
struct Verdict {
    fingerprint: Fingerprint,
    waiting: bool,
    candidate: bool,
    probed_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct WaitingProbeTracker {
    verdicts: HashMap<String, Verdict>,
    seen_once: HashMap<String, (Fingerprint, HashSet<u32>)>,
}

impl WaitingProbeTracker {
    pub(crate) fn prune_and_collect_due(
        &mut self,
        terminals: &[Value],
        now: Instant,
    ) -> Vec<ProbePlan> {
        let mut eligible = HashSet::new();
        let mut candidates = Vec::new();
        let mut live_fps = HashMap::new();

        for info in terminals {
            let Some(plan) = probe_plan_from_info(info) else {
                continue;
            };
            eligible.insert(plan.address.clone());
            live_fps.insert(plan.address.clone(), plan.fingerprint.clone());
            candidates.push(plan);
        }

        self.verdicts
            .retain(|addr, v| live_fps.get(addr) == Some(&v.fingerprint));
        self.seen_once.retain(|addr, _| eligible.contains(addr));

        candidates
            .into_iter()
            .filter(|plan| {
                self.verdicts
                    .get(&plan.address)
                    .is_none_or(|v| now.duration_since(v.probed_at) >= cadence_of(v))
            })
            .collect()
    }

    pub(crate) fn apply_cycle_results(
        &mut self,
        plans: &[ProbePlan],
        listings: &HashMap<Option<String>, Vec<ProcessRow>>,
        layer2_hits: &HashSet<String>,
        now: Instant,
    ) -> HashSet<String> {
        for plan in plans {
            let mut candidate = false;
            let mut waiting = layer2_hits.contains(&plan.address);
            if plan.process_tree {
                if let (Some(rows), Ok(agent_pid)) = (
                    listings.get(&plan.machine),
                    u32::try_from(plan.fingerprint.pid),
                ) {
                    let current = tool_shell_descendant_pids(rows, agent_pid, STARTUP_WINDOW_SECS);
                    let prior = self
                        .seen_once
                        .get(&plan.address)
                        .filter(|(fp, _)| fp == &plan.fingerprint)
                        .map(|(_, pids)| pids);
                    if !waiting {
                        waiting =
                            prior.is_some_and(|pids| current.iter().any(|pid| pids.contains(pid)));
                    }
                    candidate = !current.is_empty();
                    self.seen_once
                        .insert(plan.address.clone(), (plan.fingerprint.clone(), current));
                }
            }
            self.verdicts.insert(
                plan.address.clone(),
                Verdict {
                    fingerprint: plan.fingerprint.clone(),
                    waiting,
                    candidate,
                    probed_at: now,
                },
            );
        }

        self.waiting_addresses()
    }

    pub(crate) fn waiting_addresses(&self) -> HashSet<String> {
        self.verdicts
            .iter()
            .filter_map(|(addr, verdict)| verdict.waiting.then_some(addr.clone()))
            .collect()
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.verdicts
            .values()
            .map(|verdict| verdict.probed_at + cadence_of(verdict))
            .min()
    }
}

fn cadence_of(verdict: &Verdict) -> Duration {
    if verdict.waiting || verdict.candidate {
        ACTIVE_CADENCE
    } else {
        QUIET_CADENCE
    }
}

fn is_c_form(arg: &str) -> bool {
    let Some(body) = arg.strip_prefix('-').filter(|body| !body.starts_with('-')) else {
        return false;
    };
    body.strip_suffix('c')
        .is_some_and(|flags| flags.chars().all(|c| matches!(c, 'l' | 'i' | 'x' | 's')))
}

pub(crate) fn tool_shell(argv: &[String]) -> bool {
    let Some(exe) = argv.first() else {
        return false;
    };
    let base = exe.rsplit('/').next().unwrap_or(exe);
    SHELLS.contains(&base.trim_start_matches('-')) && argv[1..].iter().any(|arg| is_c_form(arg))
}

pub(crate) fn parse_etime(word: &str) -> Option<u64> {
    let (days, rest) = match word.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, word),
    };
    let mut parts = rest.split(':');
    let (hours, minutes, seconds) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(minutes), Some(seconds), None, None) => (
            0,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        (Some(hours), Some(minutes), Some(seconds), None) => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

pub(crate) fn parse_processes(text: &str) -> Vec<ProcessRow> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some(ProcessRow {
                pid: words.next()?.parse().ok()?,
                ppid: words.next()?.parse().ok()?,
                etime: parse_etime(words.next()?)?,
                argv: words.map(str::to_owned).collect(),
            })
        })
        .collect()
}

pub(crate) fn tool_shell_descendant_pids(
    rows: &[ProcessRow],
    agent_pid: u32,
    window_secs: u64,
) -> HashSet<u32> {
    let Some(agent_etime) = rows.iter().find(|r| r.pid == agent_pid).map(|r| r.etime) else {
        return HashSet::new();
    };
    let mut children_of: HashMap<u32, Vec<&ProcessRow>> = HashMap::new();
    for row in rows {
        children_of.entry(row.ppid).or_default().push(row);
    }
    let mut pids = HashSet::new();
    let mut visited = HashSet::from([agent_pid]);
    let mut frontier = children_of.get(&agent_pid).cloned().unwrap_or_default();
    while let Some(row) = frontier.pop() {
        if !visited.insert(row.pid) {
            continue;
        }
        if agent_etime.saturating_sub(row.etime) >= window_secs && tool_shell(&row.argv) {
            pids.insert(row.pid);
        }
        if let Some(children) = children_of.get(&row.pid) {
            frontier.extend(children);
        }
    }
    pids
}

pub(crate) fn session_is_shell(text: &str, pid: i32) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let file_pid = value.get("pid").and_then(|pid_val| {
        pid_val
            .as_i64()
            .or_else(|| pid_val.as_str().and_then(|digits| digits.parse().ok()))
    });
    file_pid == Some(i64::from(pid)) && value.get("status").and_then(Value::as_str) == Some("shell")
}

pub(crate) fn layer2_for(app: &str, word: &str) -> Option<Layer2> {
    let normalized = app.trim().to_ascii_lowercase();
    match (normalized.as_str(), word) {
        ("claude-code" | "claude", "idle" | "done") => Some(Layer2::ClaudeSessions),
        _ => None,
    }
}

pub(crate) fn probe_plan_from_info(info: &Value) -> Option<ProbePlan> {
    if matches!(
        info.get("state").and_then(Value::as_str),
        Some("finished" | "closed" | "exited" | "dead")
    ) || info.get("closed").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let address = info.get("address").and_then(Value::as_str)?.to_owned();
    let app = super::sync::status_agent_label(info).or_else(|| {
        super::sync::foreground_detected_agent(info)
            .map(|agent| crate::detect::agent_label(agent).to_owned())
    });
    let word = match info.pointer("/status/state").and_then(Value::as_str) {
        Some("idle") => "idle",
        Some("done") => "done",
        Some(_) => return None,
        None => {
            super::sync::foreground_detected_agent(info)?;
            "idle"
        }
    };
    let pid_i64 = info.pointer("/foreground/pid").and_then(Value::as_i64)?;
    let pid = i32::try_from(pid_i64).ok()?;
    let created_ms = info.get("created_ms").and_then(Value::as_u64).unwrap_or(0);
    let layer2 = app.as_deref().and_then(|name| layer2_for(name, word));
    let machine = super::sync::machine_of_address(&address).map(str::to_owned);

    Some(ProbePlan {
        address,
        machine,
        fingerprint: Fingerprint {
            pid,
            created_ms,
            word: word.to_owned(),
            status_label: app,
        },
        process_tree: true,
        layer2,
    })
}

fn resolve_machine_home(
    core: &Core,
    machine: &Option<String>,
    homes: &mut HashMap<Option<String>, Option<String>>,
    can_exec: bool,
) -> Option<String> {
    if let Some(cached) = homes.get(machine) {
        return cached.clone();
    }
    let resolved = if can_exec {
        core.exec_machine(
            machine.as_deref(),
            &["sh", "-c", "printf %s \"$HOME\""],
            3_000,
        )
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    } else if machine.is_none() {
        std::env::var("HOME").ok().filter(|s| !s.is_empty())
    } else {
        None
    };
    homes.insert(machine.clone(), resolved.clone());
    resolved
}

fn run_probe_cycle(
    core: &Core,
    plans: &[ProbePlan],
    homes: &mut HashMap<Option<String>, Option<String>>,
) -> (HashMap<Option<String>, Vec<ProcessRow>>, HashSet<String>) {
    let can_exec = core.supports_method("machine.exec");
    let can_read = core.supports_method("file.read");

    let mut by_machine: BTreeMap<Option<String>, Vec<&ProbePlan>> = BTreeMap::new();
    for plan in plans {
        by_machine
            .entry(plan.machine.clone())
            .or_default()
            .push(plan);
    }

    let mut listings = HashMap::new();
    let mut layer2_hits = HashSet::new();

    for (machine, machine_plans) in by_machine {
        if can_exec && machine_plans.iter().any(|plan| plan.process_tree) {
            if let Ok(stdout) = core.exec_machine(machine.as_deref(), PS_ARGV, 4_000) {
                listings.insert(machine.clone(), parse_processes(&stdout));
            }
        }
        if can_read {
            for plan in machine_plans {
                if plan.layer2 != Some(Layer2::ClaudeSessions) {
                    continue;
                }
                let Some(home) = resolve_machine_home(core, &machine, homes, can_exec) else {
                    continue;
                };
                let path = format!(
                    "{}/.claude/sessions/{}.json",
                    home.trim_end_matches('/'),
                    plan.fingerprint.pid
                );
                if let Ok(text) = core.read_remote_file(machine.as_deref(), &path, None) {
                    if session_is_shell(&text, plan.fingerprint.pid) {
                        layer2_hits.insert(plan.address.clone());
                    }
                }
            }
        }
    }

    (listings, layer2_hits)
}

pub(crate) fn spawn_waiting_worker(
    core: Arc<Core>,
    initial_terminals: Vec<Value>,
    mut terminals_rx: watch::Receiver<Vec<Value>>,
    events: mpsc::Sender<AppEvent>,
) {
    if !core.supports_method("machine.exec") && !core.supports_method("file.read") {
        return;
    }
    tokio::spawn(async move {
        let mut tracker = WaitingProbeTracker::default();
        let mut homes: HashMap<Option<String>, Option<String>> = HashMap::new();
        let mut latest_terminals = initial_terminals;
        let mut last_waiting = HashSet::new();

        loop {
            let now = Instant::now();
            let due_plans = tracker.prune_and_collect_due(&latest_terminals, now);
            let mut next_waiting = tracker.waiting_addresses();

            if !due_plans.is_empty() {
                let core_clone = core.clone();
                let plans_clone = due_plans.clone();
                let mut homes_clone = std::mem::take(&mut homes);
                let result = tokio::task::spawn_blocking(move || {
                    let out = run_probe_cycle(&core_clone, &plans_clone, &mut homes_clone);
                    (out, homes_clone)
                })
                .await;
                if let Ok(((listings, layer2_hits), updated_homes)) = result {
                    homes = updated_homes;
                    next_waiting = tracker.apply_cycle_results(
                        &due_plans,
                        &listings,
                        &layer2_hits,
                        Instant::now(),
                    );
                }
            }

            if next_waiting != last_waiting {
                last_waiting = next_waiting.clone();
                set_waiting_addresses(next_waiting);
                if events.send(AppEvent::HcWaitingChanged).await.is_err() {
                    break;
                }
            }

            let sleep_fut = async {
                match tracker.next_due() {
                    Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                res = terminals_rx.changed() => {
                    if res.is_err() {
                        break;
                    }
                    latest_terminals = terminals_rx.borrow_and_update().clone();
                }
                _ = sleep_fut => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ps_parser_detects_tool_shells_outside_startup_window() {
        let sample = "\
  100   1 10:00 claude --model opus
  101 100 09:55 /bin/zsh -c ls
  102 100 08:30 -bash -lc sleep 120
  103 102 08:29 python3 worker.py
  104 100 08:20 node server.js
";
        let rows = parse_processes(sample);
        assert_eq!(rows.len(), 5);
        assert_eq!(parse_etime("1-02:03:04"), Some(93_784));
        let pids = tool_shell_descendant_pids(&rows, 100, 20);
        // 100 ran 600s; 101 ran 595s (delta 5s < 20s startup window), 102 ran 510s (delta 90s >= 20s).
        assert_eq!(pids, HashSet::from([102]));
    }

    #[test]
    fn claude_sessions_json_matches_exact_pid_and_shell_status() {
        assert!(session_is_shell(
            r#"{"pid": 4242, "status": "shell", "sessionId": "abc"}"#,
            4242
        ));
        assert!(!session_is_shell(
            r#"{"pid": 4242, "status": "busy"}"#,
            4242
        ));
        assert!(!session_is_shell(
            r#"{"pid": 9999, "status": "shell"}"#,
            4242
        ));
        assert!(!session_is_shell("not-json", 4242));
    }

    #[test]
    fn two_sample_confirmation_and_fingerprint_reset_work() {
        let mut tracker = WaitingProbeTracker::default();
        let t0 = Instant::now();
        let term = json!({
            "address": "oracle/t1",
            "state": "live",
            "created_ms": 111,
            "foreground": { "pid": 100, "program": "claude" },
            "status": { "state": "idle", "label": "claude" }
        });
        let rows = parse_processes(
            "\
  100   1 05:00 claude
  102 100 03:00 bash -c sleep 99
",
        );
        let listings = HashMap::from([(Some("oracle".to_owned()), rows)]);

        let plans1 = tracker.prune_and_collect_due(std::slice::from_ref(&term), t0);
        assert_eq!(plans1.len(), 1);
        let w1 = tracker.apply_cycle_results(&plans1, &listings, &HashSet::new(), t0);
        assert!(!w1.contains("oracle/t1"));

        // Candidate saw once -> fast cadence (3s) due!
        assert!(
            tracker
                .prune_and_collect_due(std::slice::from_ref(&term), t0 + Duration::from_secs(3))
                .len()
                == 1
        );
        let t1 = t0 + Duration::from_secs(3);
        let plans2 = tracker.prune_and_collect_due(std::slice::from_ref(&term), t1);
        let w2 = tracker.apply_cycle_results(&plans2, &listings, &HashSet::new(), t1);
        assert!(w2.contains("oracle/t1"));

        // Fingerprint changes (e.g. different created_ms) -> verdict cleared immediately.
        let restarted = json!({
            "address": "oracle/t1",
            "state": "live",
            "created_ms": 222,
            "foreground": { "pid": 100, "program": "claude" },
            "status": { "state": "idle", "label": "claude" }
        });
        let plans3 = tracker.prune_and_collect_due(&[restarted], t1 + Duration::from_secs(1));
        assert_eq!(plans3.len(), 1);
        assert!(!tracker.waiting_addresses().contains("oracle/t1"));
    }
}
