# Chat-confirmations — e2e test handover (tabled 2026-05-30)

> ## ✅ e2e VALIDATED LIVE — 2026-05-30
> The live Matrix walkthrough below was executed autonomously and **all four checks
> passed green in a single run** (driver run #18), against a real `aqua-matrix-claude-p`
> daemon running the real Phase A/B code with real `claude -p`:
>
> | Check | Result |
> |---|---|
> | T0 — E2EE bidirectional round-trip | PASS |
> | T1 — Phase A **approve**: destructive `rm` → plan → `[confirm]` → `yes` → **file deleted** | PASS |
> | T2 — Phase A **deny**: → `no` → `[aborted]`, file survives | PASS |
> | T3 — Phase B **`ask_human`**: `[ask]` mid-run → answered → reply uses the answer | PASS |
>
> Driven by a new persistent harness: `crates/aqua-matrix-agent/examples/chat_e2e_driver.rs`
> (plus `examples/invite_probe.rs`, a DM-invite diagnostic). It plays the human side over
> a throwaway identity against a throwaway second daemon instance — production untouched.
>
> **Bugs found & fixed to get here** (this commit):
> 1. `relay/lib.rs` dispatch sender filter was case-sensitive → dropped all messages from a
>    mixed-case `did:key` peer. Now `eq_ignore_ascii_case`. (Masked in prod: Tim's `did:pkh`
>    is lowercase hex.)
> 2. **Addressing must use the canonical *lowercase* MXID.** Synapse lowercases localparts;
>    inviting/messaging a DID-derived mixed-case MXID hits a phantom user, so the real daemon
>    never receives invites/messages. Pass lowercase targets (the daemon's `--target`, and any
>    peer you `send_dm`/`create_dm` to).
> 3. Duplicate-room race: a freshly-connected client's `get_dm_room` is empty, so `send_dm`
>    (e.g. the daemon hello) `create_dm`'d a second room, splitting the two sides and breaking
>    Megolm. Fixed in `run_daemon`: sync-before-join, `AgentClient::mark_dm()` on join,
>    continuous invite auto-join handler, and a hello-guard that defers greeting until a DM
>    room exists (so it never spawns a stray room).
>
> **Harness notes:** the driver reconnects only when its OIDC token nears expiry (tokens are
> ~300s and the example has no rotation loop), and warms up after a reconnect to absorb a
> first-message Megolm re-key race. Verdicts grade on observable outcomes (gate fired +
> canary file state), not on catching every streamed courtesy message.
>
> To re-run: `cargo run -p aqua-matrix-agent --example chat_e2e_driver -- --key-file <drv.pem>
> --store-dir <store> --target <LOWERCASE channel MXID> --approve-canary <path-in-workdir>
> --deny-canary <path-in-workdir>`. Canaries MUST live inside the daemon's working dir
> (`~/aqua-matrix-hello`) — `claude -p`'s sandbox blocks file ops outside it. Start order:
> create the invite (one-shot driver send) → start the test daemon → run the driver.
>
> The original tabled plan and its state-at-table-time are preserved below for history.

---

Picks up where `chat-confirmations.md` (and its "## Handover — Phase B" section) left
off. Phases A and B are **coded, committed, unit-tested, and loaded into the live
daemon** — what remains is the **live Matrix e2e walkthrough**, which was prepared
and then tabled before execution.

## State at table-time

| Item | State |
|---|---|
| Branch / HEAD | `feat/chat-confirmations` @ `9360f2e` (Phase A+B), working tree clean |
| Build | `cargo build` clean, **25/25** tests green (14 ask-mcp + 11 claude-p) |
| Daemon | **restarted 2026-05-30 00:47** (PID 710565) → now runs the post-build binary. The previous live process (687363, started 20:49) was **stale** — pre-dated the 00:35 rebuild, so Phase B was on disk but not loaded until this restart. |
| ask-mcp sibling | present at `target/debug/aqua-matrix-ask-mcp` (where `ask_mcp_bin_path()` resolves it) |
| Canaries (pre-staged in /tmp) | `/tmp/phase-a-canary-approve.txt` (should be **deleted** after `yes`), `/tmp/phase-a-canary-deny.txt` (should **survive** after `no`) — may need re-creating if /tmp was cleared |

**Authorization note:** in the lost-session plan doc the rule was "Do NOT restart the
production daemon for me." The user **explicitly overrode** that this session
("Feel free to restart the agents if required to load the new code"), so the
restart above was sanctioned. Treat that override as session-scoped — re-confirm
before restarting in a future session.

## The e2e walkthrough (not yet run) — DM the claude-channel agent (`did:key:z6Mko…SDoSjtF`)

Watch internals with: `journalctl --user -u aqua-matrix-claude-channel.service -f`

- **Test 0 — liveness:** the restart `[hello]` should end with "…pause mid-task and ask you a question (`[ask] …`)". Seeing that = new code live.
- **Test 1 — Phase A approve:** DM `rm /tmp/phase-a-canary-approve.txt` → expect streamed **plan** + `[confirm] … **file deletion** … yes/no`. Reply `yes` → resumes + executes. Verify file gone.
- **Test 2 — Phase A deny:** DM `rm /tmp/phase-a-canary-deny.txt` → plan + `[confirm]`. Reply `no` → `[aborted] … Nothing was executed.` Verify file survives.
- **Test 3 — Phase B ask_human:** DM `Use your ask_human tool to ask me for my favorite color, then reply with a one-line compliment about that color.` → expect `[ask] …favorite color?… Reply with your answer.` Reply `teal` → final reply references teal. Logs show `ask-bridge: ask_human:` then `DM … resolved a pending question`. (Deliberately needs no file tools — the fresh path only allows `mcp__ask__ask_human`, isolating Phase B.)
- **Test 4 — Phase B fail-closed (optional, ~3 min):** trigger an `ask_human`, don't reply → claude gets "no answer within the timeout" denial and stops. Confirms timeout=deny on the live path.

## Boundary conditions / gotchas to remember

- **Pending-reply inversion:** while a `[confirm]`/`[ask]` is open, the user's *next* DM is consumed as the **answer**, not a new prompt (per-target run lock + `try_resolve`). Don't queue a second request mid-flow.
- "yes" synonyms: `yes/y/approve/approved/ok/proceed`; anything else aborts.
- Timeout = **deny** (180 s), both questions.
- Phase A gate and Phase B tool are **mutually exclusive per run**: destructive prompts get the plan-gate (no `ask_human`); non-destructive get `ask_human` (no gate). Phase A's plan/resume path is unchanged from its own verified handover.

## Next development phase (not started — confirm with user first)

Phase C — **enforced** per-tool gate + did-key-scoped capabilities via the claude
stream-json control protocol. `destructive::RULES` already carries `scope`
patterns (`Bash(rm:*)`, `Bash(git push --force*)`) seeded for exactly this.
This is a new phase; do not begin unprompted.

## Resume checklist for a future session

1. `git status` / `git log --oneline -3` — confirm still on `9360f2e`, tree clean.
2. Confirm daemon binary is current: compare `ps -eo pid,lstart` of `aqua-matrix-claude-p` vs `ls -la target/debug/aqua-matrix-claude-p` mtime. If process older than binary → ask to restart.
3. Re-stage canaries if `/tmp` was cleared.
4. Run the walkthrough above; verify canaries + grep logs for `ask-bridge` / `resolved a pending question`.
5. On green: report Phase B verified-live, then ask whether to start Phase C.
