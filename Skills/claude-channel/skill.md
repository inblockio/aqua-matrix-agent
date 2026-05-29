---
name: claude-channel
description: Matrix LLM channel — DMs from --target are forwarded to `claude -p` and the stdout is DM'd back. Separate identity from the heartbeat.
---

# Matrix Claude Channel

Separate Matrix identity (`claude-channel.pem` + `~/.aqua-matrix-claude-channel/` store) running the `aqua-matrix-claude-p` binary. Each inbound DM from the configured `--target` becomes a fresh `claude -p <prompt>` invocation; stdout is sent back as a Matrix reply. See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the full design and why this is a separate daemon from the heartbeat.

## Quick check

```bash
systemctl --user is-active aqua-matrix-claude-channel    # expect: active
journalctl --user -u aqua-matrix-claude-channel -f       # live log
cat ~/.aqua-matrix-claude-channel/config.toml            # session cache + OIDC
```

## Install

The unit ships at `systemd/aqua-matrix-claude-channel.service`. First run also generates the identity:

```bash
cp ~/aqua-matrix-hello/systemd/aqua-matrix-claude-channel.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now aqua-matrix-claude-channel
```

Verify the new identity was minted:

```bash
ls ~/aqua-matrix-hello/claude-channel.pem                # the Ed25519 key
journalctl --user -u aqua-matrix-claude-channel -n 30 | grep "agent DID"
```

## Sending it a prompt

From `--target` (Tim's account in Element), DM the **claude-channel identity** any plain prose (NOT starting with `#shell` — that prefix is the heartbeat's). The daemon will:

1. See the message via stream sync (~1 sec).
2. Spawn `claude -p "<your message>"` with a 180s timeout.
3. DM back the stdout.

Stateless per message: each invocation starts fresh, no conversation continuity. For continuity, attach locally to the `claude-bridge` tmux session (see `claude-bridge` skill).

## Respawn from Matrix

The heartbeat's `#shell respawn-channel` (sent from `--target` to the **heartbeat** identity) runs `systemctl --user restart aqua-matrix-claude-channel`. Useful when the claude-channel daemon is wedged but the heartbeat still works.

`#shell respawn` (no suffix) restarts a different unit — `claude-bridge.service`, the local interactive tmux session. Don't confuse them.

## Sync model

Stream sync (`client.sync(...)` running forever in a background tokio task). Event handler dispatches incoming messages. No polling. Reply latency = `claude -p` latency, not a polling interval.

## Failure modes

- **`claude -p timed out`** in the reply: the prompt was too complex or claude hung. The daemon stays running; just send another shorter prompt.
- **`claude -p exited with status ...`**: claude CLI rejected the prompt. Stderr is included in the reply.
- **Empty reply (`[claude-channel] (no output)`)**: claude produced no stdout. Check `claude --version` works locally; also check `which claude`.
- **No reply at all**: check `systemctl --user is-active aqua-matrix-claude-channel`. If active but unresponsive, `#shell respawn-channel` from the heartbeat side, or `systemctl --user restart aqua-matrix-claude-channel` locally.
- **`heartbeat sent` continues but no claude replies**: that's correct — the two daemons are independent. Different identities, different DMs, different processes.

## Limits

- Stateless conversation. Each Matrix message is a one-shot. For multi-turn flows use the local `claude-bridge` tmux.
- Output is truncated at ~16KB so very long claude outputs get a `[...truncated]` tail.
- No allow-list beyond `--target`: only DMs from the configured target identity are processed. Anyone else who DMs the claude-channel identity is silently ignored.
