# Plan — Human-in-the-loop confirmations over chat

> Status: **living plan**. Progressive A → B → C. Focus: gating **destructive
> commands** (`rm`, `git push --force`). Each phase is framed with the
> `/logic-model` protocol (Context → Goal → Inputs → Activities/Outputs →
> Boundaries) and ends with a **commit + handover + compact** boundary.

## Motivation

`claude-channel` forwards each DM to a one-shot `claude -p` that runs to
completion with no way to pause and ask the human. We want the agent to be able
to **loop a confirmation back through the same Matrix channel** before it does
something dangerous, and wait for the authenticated user's answer.

## The security substrate (why this channel is the right place to gate)

Each agent is a **key-identified channel**: the daemon authenticates as its own
did-key, and `dispatch` (`crates/aqua-matrix-relay/src/lib.rs`, the
`ev.sender.as_str() != target` gate) **only ever accepts messages from one
identity — `target`**, the user's did. Combined with E2E encryption, this means:

- The daemon already *knows* the authenticated identity of whoever it is talking
  to — it is `target` (a did-pkh / did-key encoded in the MXID).
- A "yes" / "grant" reply is **cryptographically attributable** to that did-key.
  No other sender can forge an approval; the relay drops it before dispatch.

**Therefore permissions can be bound to a scope that is itself bound to the
user's did-key session** — the channel is the authorization boundary. This is
the revision that reshapes Approach C (see below).

---

## Core primitive (shared by A, B, C): `ask_user`

Today `dispatch` treats every inbound DM as a *fresh* prompt. Confirmations need
the inverse: while a run awaits an answer, the user's **next DM is the answer**.

```
AgentClient / relay seam:
  ask_user(target, question, options) -> Answer
    1. send_dm(target, question)                     // streamed like any reply
    2. register a pending oneshot keyed by target    // Mutex<HashMap<did, oneshot::Sender<String>>>
    3. await the oneshot with a timeout (<= CLAUDE_TIMEOUT) → default-DENY on timeout
  handle_message(target, body):
    if a pending question exists for target → resolve its oneshot with `body`
    else → start a fresh run (current behaviour)
```

Properties:
- **Serialize one active run per `target`** (already ~one-conversation-per-user)
  so at most one question is open at a time — no answer/question correlation
  ambiguity.
- Timeout → default-deny so a silent user can never wedge the `claude` process.
- The watermark dedup already treats the reply as an ordinary message.
- **Optional UX:** also accept a 👍/👎 **Matrix reaction** as the answer. Needs a
  second `add_event_handler` for reaction events — deferred enhancement, text
  `yes`/`no` is the baseline.

This primitive *is* the "loop in confirmations via chat" capability. A/B/C
differ only in **what triggers `ask_user`** and **what an answer authorizes**.

---

## Scope-bound permission model (the revision — central to C, reused by B)

Per-command yes/no is annoying and does not scale. Instead, an answer can grant
a **scope**: a capability bound to the **user's did-key session**.

```
Scope grant (authenticated as `target`'s did-key):
  pattern   e.g. Bash(rm:*)  |  Bash(git push --force*)  |  Bash(git push -f*)
  ttl       session (daemon process / connection lifetime) | once | persisted
  grantor   target did       (the only identity that could have sent it)
```

- A **session** = the authenticated `target` did-key's interaction with this
  daemon. Scopes live in `Mutex<HashMap<did, ScopeSet>>` for the process
  lifetime; optionally persisted to Matrix **global account data** (same channel
  as the fleet registry, `update_registry`) so they survive a reconnect/restart.
- When the agent wants a destructive action the gate asks:
  **`Deny` / `Allow once` / `Allow for session (grant scope)`**.
  - *Allow once* → permit this single call.
  - *Allow for session* → record `Bash(rm:*)` (etc.) in the did's `ScopeSet`;
    subsequent matching calls are **auto-allowed without re-asking**.
- Granted scopes map directly onto Claude Code's `--allowedTools` patterns
  (`Bash(rm:*)`, `Bash(git push --force*)`), so the same vocabulary drives both
  the daemon-side gate (C) and any `claude` re-invocation.
- **Revocation:** a `#scopes` command channel — `#scopes list`, `#scopes clear`,
  `#scopes revoke <pattern>` — all authenticated as `target` by construction.

This makes approvals **OAuth-scope-like, authenticated by the chat identity's
did-key**, instead of an endless yes/no stream.

---

## Destructive-command focus

The gate keys on a small, high-value matcher (not every tool):

| Class | Match | Scope pattern |
|---|---|---|
| File deletion | `rm ` / `rm -rf` / `rmdir` / `find … -delete` | `Bash(rm:*)` |
| Force push | `git push --force` / `git push -f` / `--force-with-lease` | `Bash(git push --force*)` |
| (later) | `git reset --hard`, `> file` truncation, `dd`, `mkfs` | extensible matcher table |

Everything else flows through unchanged (or under the agent's existing
permission mode). The matcher lives in one place so it is auditable.

---

## Phase A — Plan / Approve / Execute (MVP)

**Logic model**
- **Context:** `claude` v2.1.157 has `--permission-mode plan` and
  `--continue`/`--resume`; `claude-channel` already streams replies.
- **Goal:** *A destructive request is shown as a plan, and only executes after
  the authenticated user approves over chat.*
- **Inputs:** core `ask_user` primitive; `claude -p --permission-mode plan`;
  session continuity via `--continue`; existing `stream_claude`.
- **Activities → Outputs:**
  1. Run `claude -p --permission-mode plan <prompt>` → stream the **plan** (no
     side effects). `IF plan mode THEN no tool executes`.
  2. `ask_user(target, "Approve this plan? yes/no")`.
  3. On **yes** → `claude --continue` (execution mode) → stream result.
     On **no** → abort with a note. `IF approved THEN execute ELSE abort`.
- **Outcome (verify):** a prompt that would `rm`/force-push does nothing until a
  chat `yes`; `no` leaves the workspace untouched.
- **Boundaries:** plan mode must actually suppress execution headless (verify);
  `--continue` must resume the same session; out of scope: per-tool gating.

## Phase B — MCP `ask_human` tool

**Logic model**
- **Context:** `claude` supports `--mcp-config` + `--append-system-prompt`.
- **Goal:** *The agent can ask the human a free-form question mid-run and block
  on the authenticated answer, for any step it judges risky.*
- **Inputs:** core `ask_user`; a tiny MCP server exposing
  `ask_human(question)`; IPC from that server back to the daemon (the daemon
  owns the Matrix session — the MCP subprocess does not).
- **Activities → Outputs:**
  1. Build MCP server (stdio) exposing `ask_human`. `IF claude calls ask_human
     THEN server forwards over IPC`.
  2. Daemon listens on a per-run unix socket; on request → `ask_user` → returns
     answer over IPC → tool result. `IF IPC answer THEN claude resumes`.
  3. Wire via `--mcp-config` + a system-prompt instruction to call `ask_human`
     before destructive ops.
- **Outcome (verify):** instructing claude to delete a file makes it call
  `ask_human`, which surfaces in chat; the answer steers the run.
- **Boundaries:** advisory (claude must *choose* to call it) — acceptable for B;
  IPC socket lifetime scoped to the run; out of scope: enforcement.

## Phase C — Enforced per-tool gate + scoped capabilities

**Logic model**
- **Context:** `claude --input-format stream-json --output-format stream-json`
  exposes a bidirectional **control protocol** (the Agent SDK `canUseTool`
  path); permission requests can be answered programmatically. `--permission-mode
  plan/default/dontAsk` and `--allowedTools` shape what is asked.
- **Goal:** *Every destructive tool call is enforced through the did-key-scoped
  gate: auto-allowed if a session scope covers it, else the user is asked, and
  may grant a session scope.*
- **Inputs:** core `ask_user`; the scope store (did → ScopeSet); the destructive
  matcher; stream-json control-request handling.
- **Activities → Outputs:**
  1. Drive `claude` over stream-json (persistent bidirectional session).
  2. On a permission/control request: classify against the destructive matcher.
     `IF non-destructive THEN allow`. `IF covered by a session scope THEN
     auto-allow`.
  3. Else `ask_user(Deny / Allow once / Allow for session)`. On *Allow for
     session* → insert pattern into the did's ScopeSet (optionally persist).
     `IF granted-for-session THEN future matches auto-allow`.
- **Outcome (verify):** first `rm` asks; choosing *Allow for session* makes a
  second `rm` proceed silently; `git push --force` still asks (different scope);
  `#scopes clear` re-arms the gate.
- **Boundaries:** couples to the stream-json control-message schema (verify
  exact shapes before coding); scope store must be keyed by the **authenticated
  `target` did**, never by free-text; force-push and rm scopes are **distinct**.

---

## Verification plan (all phases, destructive focus)

Run against a throwaway path so nothing real is harmed, e.g. ask the agent to
`rm` files under `/tmp/confirm-test/` and to `git push --force` a scratch branch
of a throwaway repo. Confirm: (1) the gate fires, (2) deny leaves state intact,
(3) allow proceeds, (4) for C, a session scope suppresses the second ask and
`#scopes clear` re-arms it. The authenticated-sender property is already
guaranteed by the relay's `target` gate.

## Boundary conditions (global)

- **Assumptions to verify before coding each phase:** headless plan-mode
  suppresses execution (A); MCP stdio + `--mcp-config` wiring (B); stream-json
  control-request schema (C).
- **Invariants:** never auto-allow a destructive op without an explicit session
  scope; scopes are bound to the authenticated `target` did only; timeout =
  deny; a gate failure fails **closed** (deny), never open.
- **Exclusions:** non-destructive tools keep current behaviour; reactions UX,
  multi-user rooms, and cross-agent scope sharing are out of scope.

## Handover / compact protocol

Between phases: **commit** the phase, append a `## Handover — Phase X` section
below recording (a) what shipped, (b) verified outcome, (c) live state /
follow-ups, then **compact** so the next phase starts from the durable plan +
commits rather than a long transcript.

---

## Handover log

_(appended per phase during execution)_

## Handover — Phase A

### What shipped

`crates/aqua-matrix-claude-p` gained two modules and a gated run flow:

- **`src/destructive.rs`** — the single auditable matcher. Table-driven
  (`RULES: &[Rule]`), each row carrying whole-**word** needles (`rm`, `rmdir` —
  token-boundary matched so "alarm"/"form" don't trip it), **substring** needles
  (`git push --force`, `git push -f`, `--force-with-lease`, `-delete`), and the
  Phase-C `--allowedTools` scope (`Bash(rm:*)`, `Bash(git push --force*)`).
  Public API: `classify(text) -> Option<&'static str>` and
  `looks_destructive(text) -> bool`. 4 unit tests.
- **`src/pending.rs`** — the shared **`ask_user` primitive** (the pending-reply
  router). `PendingMap` = `Arc<Mutex<HashMap<String /*target MXID*/,
  oneshot::Sender<String>>>>`, `Clone`/`Default`. Methods:
  - `try_resolve(target, body) -> bool` — called first in `handle_message`; if a
    question is open for `target`, consumes the DM as the answer (returns true).
  - `ask(agent, target, question, timeout) -> Option<String>` — registers the
    oneshot **before** sending the DM (closes the fast-reply race), awaits with
    timeout; `None` on timeout / send-fail / dropped channel and callers MUST
    treat `None` as **deny** (fail closed). Poisoned lock → deny.
  - `is_pending(target)` — reserved for B/C (`#[allow(dead_code)]`).
  4 unit tests covering routing + the overwrite-denies-old-waiter race.
- **`src/main.rs`** — `ClaudePHandler` now holds `pending: PendingMap` plus a
  per-target `run_locks` map (`Arc<tokio::Mutex<()>>` per target) so only **one
  run / one open question per user** at a time. `handle_message` checks
  `try_resolve` first; otherwise spawns a task that holds the per-target lock and
  branches: destructive → `run_gated`, else original `stream_claude` path.
  `stream_claude` was refactored to take a `ClaudeRun { prompt, permission_mode,
  resume_session }` (`::fresh` / `::plan` / `::resume`) and return
  `RunOutcome { session_id }` (parsed from the `init` event, `result` as
  fallback).

### `ask_user` seam for B/C to reuse

```rust
// crates/aqua-matrix-claude-p/src/pending.rs
impl PendingMap {
    pub fn try_resolve(&self, target: &str, body: &str) -> bool;
    pub async fn ask(&self, agent: &AgentClient, target: &str,
                     question: &str, timeout: Duration) -> Option<String>; // None == DENY
    pub fn is_pending(&self, target: &str) -> bool;
}
```

It lives on the handler crate (not the relay trait) deliberately — matches the
opt-in `typing_guard`/`reply_stream` house style and keeps `MessageHandler`
minimal. Phase B's MCP `ask_human` IPC handler and Phase C's control-request
gate both call `pending.ask(...)` the same way; only the trigger differs.
Per-target serialization is provided by the handler's `run_locks` (B/C should
reuse the same lock so a confirmation can't race a new prompt).

### Verified `claude` plan-mode / resume behaviour (v2.1.157, empirical)

Exact flags and shapes the code relies on:

1. **Plan preview (no execution):**
   `claude -p --permission-mode plan "<prompt>" --output-format stream-json --verbose`
   - Model investigates with **read-only** tools (it DID run `ls`/`cat`/`Read`),
     then calls `ExitPlanMode { plan, allowedPrompts, planFilePath }`.
   - Headless, `ExitPlanMode` is **auto-DENIED** (`result.permission_denials =
     ["ExitPlanMode"]`); **no destructive tool runs**. Confirmed across 3 runs —
     target files always survived plan mode.
   - The plan text is delivered in the terminal `{"type":"result", "result":
     "<plan/summary>", "session_id":"…"}` event (and `ExitPlanMode.plan`).
   - Session id is in the `{"type":"system","subtype":"init","session_id":"…"}`
     event (also echoed on `result`).
2. **Approve → execute (same session):**
   `claude -p --resume <session_id> "Approved. Proceed with the plan." --permission-mode acceptEdits --output-format stream-json --verbose`
   - Resumes the SAME `session_id`, exits plan mode, runs the `rm`
     (`result.permission_denials = []`, `is_error=false`). File deleted only
     here.
3. **Deny → no resume → state intact:** proven by running step 1 only and
   confirming the target file still exists.

So Phase A gates **all destructive-classified prompts** via
plan-then-approve-then-resume (acceptEdits). Non-destructive prompts keep the
original direct one-shot stream — no behaviour change.

### Autonomous verification (done, no Matrix client needed)

- `cargo build` — clean, **zero warnings**.
- `cargo test` — 8/8 pass (4 matcher, 4 pending-router incl. race cases).
- `claude` plan/resume/deny behaviour confirmed empirically (above), all
  filesystem targets under `/tmp/confirm-test/` only.

### Needs a LIVE user Matrix test

- The end-to-end chat round-trip: DM a destructive request to the
  claude-channel daemon, see the streamed plan + the `[confirm] … reply yes/no`
  question, reply `yes`/`no`, and confirm execute-vs-abort. The `ask_user`
  routing, timeout-deny, typing/stream UX, and the `send_dm` inside `ask` are
  only unit/empirically tested, not exercised over a real E2EE room. (Do NOT
  restart the production daemon for me — user will run this.)

### Deviations / notes / follow-ups

- Approval words accepted: `yes/y/approve/approved/ok/proceed` (case-insensitive,
  trimmed); anything else (incl. `no`, empty, timeout) = **deny**.
- `APPROVAL_TIMEOUT` = 180s (== `CLAUDE_TIMEOUT`, the plan's "≤ CLAUDE_TIMEOUT").
- The resume run is `acceptEdits`, not full `--dangerously-skip-permissions`:
  it auto-accepts the already-planned actions but is not a blanket bypass. If a
  resumed run hits a *new* permission prompt with no TTY it would stall to
  timeout (fail-safe) — Phase C's stream-json control path is the real fix.
- Per-target `run_locks` entries are never evicted; fine for a small fixed
  fleet (one `target` per daemon today). Revisit if multi-target.
- Matcher classifies the **prompt text**, not the eventual tool call — coarse by
  design (fail-loud). Phase C reuses `classify`/`RULES` against concrete
  `Bash(...)` calls for precise enforcement.
