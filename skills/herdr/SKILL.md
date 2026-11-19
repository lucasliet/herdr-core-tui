---
name: herdr
description: "Control hc-tui (herdr-core-tui), the terminal UI frontend for herdr-core (hc). Use only when the user explicitly mentions Herdr, hc-tui, or asks to inspect or control TUI workspaces, tabs, panes, or agents. For raw terminal/agent lifecycle or remote host commands, read `hc --skill` and use `hc`."
---

# hc-tui (`herdr-core-tui`)

`hc` (`herdr-core`) runs terminals, coding agents, and SSH machine connections and keeps them alive when any frontend closes. `hc-tui` (`herdr-core-tui`) is the terminal UI frontend on top of `hc`: it organizes panes into workspaces, tabs, splits, worktrees, agent status panels, notifications, and the collapsible right-hand **Files** / **Changes** panel.

Before issuing any control command, verify that this shell is running inside a Herdr / `hc`-managed terminal:

```bash
test -n "${HC_TERMINAL_ID:-}" || test "${HERDR_ENV:-}" = 1
```

If the check fails, say that you are not running inside a `hc` / `hc-tui` terminal and stop.

## Choose between `hc` and `hc-tui`

- Use **`hc`** (`hc --skill`) to spawn, read, send input to, wait on, or kill underlying terminals and coding agents, or execute commands on remote machines (`hc ls`, `hc get "$HC_TERMINAL_ID"`, `hc exec --on <machine>`, `hc run`). `$HC_TERMINAL_ID` is your `hc` terminal address (not a TUI pane ID).
- Use **`hc-tui`** to inspect or drive the active `hc-tui` session layout (workspaces, tabs, splits, worktrees, agent focus, TUI notifications). Note: `hc-tui <command>` talks to a running `hc-tui` instance via its API socket (`hc-tui status`). When using `herdr-app` (desktop GUI) rather than `hc-tui`, run `herdr-app --skill`.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
hc-tui --help
```

Then inspect the relevant subcommand group by running it without arguments:

```bash
hc-tui status
hc-tui agent
hc-tui pane
hc-tui workspace
hc-tui tab
hc-tui worktree
hc-tui terminal
hc-tui notification
hc-tui session
hc-tui machine
hc-tui config
hc-tui channel
```

Do not run bare `hc-tui` for discovery: without arguments it launches and attaches the TUI. Do not probe mutating subcommands such as `hc-tui workspace create` without arguments. Other useful launch/maintenance commands:

```bash
hc-tui --remote <ssh-target> [--session <name>]   # attach TUI to a remote hc server
hc-tui upgrade                                     # or `hc-tui update` / `hc-tui --upgrade`
```

Most control commands return JSON. Read identifiers and state from those responses instead of predicting them.

## Understand layout, panes, and agents

Choose the primitive that matches the job:

- Workspace, tab, and pane topology organize terminal locations (plus the right-hand collapsible **Files** and **Changes** panel in the tab bar for inspecting local or remote `hc` repository trees and diffs).
- Pane commands control raw terminals, shells, tests, servers, input, and output.
- Agent commands control the recognized coding agent currently occupying a pane.

A pane exists whether or not it contains an agent. `agent start` requires an existing available shell pane and never creates, splits, or moves layout. Use pane commands for ordinary processes. Use agent commands when `hc-tui` must validate agent identity or interpret `idle`, `working`, `blocked`, `done`, and `unknown` lifecycle states.

Agent commands accept either a unique live agent name or the pane ID currently hosting that agent. They do not accept terminal IDs (`$HC_TERMINAL_ID`) or bare agent-kind labels. Names must match `[a-z][a-z0-9_-]{0,31}` and be unique among live agents. A name follows the current pane occupant and is cleared when that agent exits, is released, or is replaced. Suspending an agent with Ctrl+Z and running a non-agent command preserves its identity while the exact process remains alive. This does not preserve the original identity across another recognized agent taking over. A retained identity does not mean the agent is in the foreground; inspect the pane before sending input.

`idle` and `done` normally indicate the agent is ready for input; neither guarantees that an unfamiliar approval dialog was recognized or that a retained agent is in the foreground. The CLI/API uses the server's seen state to distinguish them; explicit focus commands mark the target seen, while reads do not. Each TUI client tracks viewed completions independently, so its Done badge can differ from the CLI or another client's badge. Antigravity and Grok can be idle while background tasks or subagents keep running, so a settled-state wait is not proof those jobs have finished. `blocked` means Herdr recognized an approval or question UI. `unknown` means an agent is present but Herdr cannot classify it confidently; it does not prove completion.

## Use IDs and caller context

Public TUI IDs are opaque stable handles:

- workspace: `w1`
- tab: `w1:t1`
- pane: `w1:p1`

Closed tab and pane IDs are not reused. A pane moved into another workspace receives a new workspace-qualified pane ID (`w2:p1`). Check `$HERDR_WORKSPACE_ID`, `$HERDR_TAB_ID`, `$HERDR_PANE_ID`, and `$HC_TERMINAL_ID`:

```bash
printf 'ws=%s tab=%s pane=%s hc=%s\n' "${HERDR_WORKSPACE_ID:-}" "${HERDR_TAB_ID:-}" "${HERDR_PANE_ID:-}" "${HC_TERMINAL_ID:-}"
```

In `hc`-managed panes, `$HC_TERMINAL_ID` is set even when `$HERDR_PANE_ID` is absent; discover live TUI IDs first with:

```bash
hc-tui status
hc-tui workspace list
hc-tui tab list
hc-tui pane list
hc-tui agent list
```

Prefer `--current` only when `$HERDR_PANE_ID` is set and the command targets your own calling pane; otherwise pass explicit `--pane <id>`, `--workspace <id>`, or `--cwd "$PWD"` from discovery JSON.

Creation responses expose the IDs to use next:
- `workspace create` returns `.result.workspace`, `.result.tab`, and `.result.root_pane`.
- `tab create` returns `.result.tab` and `.result.root_pane`.
- `pane split` returns the new pane as `.result.pane`.

## Remote machines

In `hc` mode, `hc-tui` automatically syncs saved `hc` machines (spaces, tabs, panes, agent status, and git repository metadata). Use `hc` (`hc machine ls`, `hc exec --on <machine>`) for direct machine/file operations, or use `hc-tui --remote <ssh-target>` to attach the TUI directly over SSH.

Legacy TUI machine commands (`hc-tui machine ...`) apply only to standalone SSH profiles when used:

```bash
hc-tui --machine <label-or-id> agent list
hc-tui --machine <label-or-id> pane list
```

The selector must be a saved profile ID or unique case-sensitive label, not an arbitrary hostname. Do not combine `--machine` with `--session` or `--remote`.

## Start and coordinate an agent

Default to a sibling pane in the current tab and working directory unless the user explicitly asks for a new tab or workspace:

```bash
hc-tui pane split --pane <pane-id> --direction right --cwd "$PWD" --no-focus
```

Pass `--current` when `$HERDR_PANE_ID` is set. Split a wide pane `right` and a narrow/tall pane `down`. Avoid repeated same-direction splits that create unusable columns or rows. Read the new pane ID from `.result.pane.pane_id`.

Start a supported agent only inside an idle shell pane (no active foreground command):

```bash
hc-tui agent start reviewer --kind codex --pane <returned-pane-id>
```

Pass native agent arguments only after `--`:

```bash
hc-tui agent start reviewer --kind codex --pane <returned-pane-id> -- <agent-args...>
```

Submit work and wait for settlement:

```bash
hc-tui agent prompt reviewer "Review the current diff and report only actionable findings." --wait --timeout 120000
hc-tui agent wait reviewer --until blocked --timeout 120000
hc-tui agent send-keys reviewer esc
hc-tui agent get reviewer
hc-tui agent read reviewer --source recent-unwrapped --lines 120
```

`idle` and `done` mark the end of a turn; if `wait` returns `blocked`, inspect `agent get` and `agent read` before sending input.

## Run an ordinary command in another pane

```bash
hc-tui pane split --pane <pane-id> --direction right --cwd "$PWD" --no-focus
hc-tui pane run <returned-pane-id> "cargo test"
hc-tui pane wait-output <returned-pane-id> --match "test result" --timeout 120000
hc-tui pane read <returned-pane-id> --source recent-unwrapped --lines 120
```

Read sources (`--source`):
- `visible`: currently rendered viewport.
- `recent`: recent rendered output including soft wraps.
- `recent-unwrapped`: recent output with soft wraps joined (best for logs/transcripts).
- `detection`: plain-text bottom-buffer snapshot used for agent detection.

Use `--format ansi` when terminal styling or colors are evidence.

## Safety and coordination rules

- Use `--no-focus` for background work unless the user asked to switch focus.
- Never guess IDs from sidebar order; read them from JSON responses (`hc-tui workspace list`, `hc-tui pane list`, `hc-tui agent list`, or `hc ls --json`).
- Do not close workspaces, tabs, panes, or `hc` terminals you did not create unless explicitly requested.
- Use `--trust-repository` only after the user has verified a repository for linked worktree creation.
- Never run `hc-tui server stop` from inside an active session unless the user explicitly intends to stop that TUI server.
- Never stop the host `hc` daemon (`hc server stop`) unless explicitly asked, as it owns live background terminals.
- CLI server errors are JSON on stderr with exit status 1; syntax errors exit with status 2.
