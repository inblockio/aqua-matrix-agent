---
name: e2e-test
description: Run and verify the E2EE integration test between two aqua-matrix-agent identities
---

# E2E Encryption Integration Test

Runs a bidirectional end-to-end-encrypted Matrix exchange between two distinct agent identities (Agent A and Agent B) against the live `matrix.inblock.io` homeserver. Used to verify that CAIP-122 auth, OIDC auto-registration, device key upload, and Megolm key exchange all work together.

The test lives at `tests/e2e.rs` and is gated behind the `e2e` Cargo feature so it doesn't run in `cargo test`.

## Run it

```bash
cd ~/aqua-matrix-hello && cargo test --test e2e --features e2e -- --nocapture
```

`--nocapture` is recommended — the test prints the user IDs, DIDs, and message tags it exchanged. Without it you only see pass/fail.

The test wipes `/tmp/aqua-e2e-agent-a` and `/tmp/aqua-e2e-agent-b` at the start of every run so stale crypto state from a prior run does not poison the device-key upload.

## What it asserts

1. Both agents authenticate via CAIP-122 against `siwx-oidc.inblock.io` and end up with distinct Matrix user IDs.
2. Agent A creates a DM room and invites Agent B.
3. Agent B joins, then sends a tagged message to Agent A.
4. Agent A syncs, finds the message, and asserts the body is **not** `[unable to decrypt]`.
5. Agent A replies; Agent B syncs and asserts decryptability in the reverse direction.

The unique tag per run (UUID) means a passing test proves the message round-tripped in this run, not from a previous one.

## Key ordering rule (do not break this)

Agent B must send **first**, not Agent A. The reason is buried in Megolm session creation:

- Agent A creates the room and the *first* outbound Megolm session before Agent B exists in the room.
- If A then sends, A's session key has no recipient for B and the message is undecryptable on B's side.
- When B joins and sends, B creates *its* outbound session with A already present, so A receives a usable key.
- After B's send completes a successful round-trip, A's next send rotates/shares correctly and B can decrypt.

If this ordering is changed, expect `[unable to decrypt]` on one side.

## Required state

- `~/aqua-matrix-hello/agent.pem` (Agent A key) — pre-existing in the repo
- `~/aqua-matrix-hello/agent-b.pem` (Agent B key) — pre-existing in the repo
- Network access to `https://siwx-oidc.inblock.io` and `https://matrix.inblock.io`
- The corresponding Matrix accounts already provisioned (they were on first run)

Don't delete the `.pem` files — the DIDs and Matrix accounts are derived from those keys. Losing them strands the accounts.

## Failure modes

| Symptom | Likely cause |
|---|---|
| `[unable to decrypt]` on either side | Stale crypto store, or key-ordering rule above was violated |
| `Agent A failed to connect` | `siwx-oidc.inblock.io` down, or network issue |
| `no DM room found between agents` | Invite was sent but sync didn't pick it up — usually a flaky sync, re-run once |
| `agents must have different identities` | Both agents loaded the same key file — check `agent.pem` vs `agent-b.pem` paths |

## Logging

The test installs `tracing_subscriber` at `warn,aqua_matrix_agent=info`. Bump it by exporting `RUST_LOG` before the test, e.g. `RUST_LOG=debug,matrix_sdk_crypto=trace`.
