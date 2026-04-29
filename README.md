# babysit

Run a shell command inside a 2-tab TUI alongside an AI agent (Claude Code,
Codex, …) that can observe and operate the command via plain `babysit`
subcommands.

```
babysit -p "tell me when there's an error and restart the command" -- make local-ci
```

```
┌─[1] make local-ci · running   [2] claude · running ───────── ab12 ──┐
│                                                                     │
│   Running tests...                                                  │
│   ✓ test_a                                                          │
│   ✗ test_b: assertion failed                                        │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
  Ctrl-1/2 switch tabs · click tab to focus · Ctrl-Q quit
```

- **Tab 1** — the wrapped command, running inside a real PTY. Full
  fidelity: colors, curses apps, Ctrl-C, stdin all work.
- **Tab 2** — an AI agent CLI launched with your `-p` prompt as its first
  message. The agent has access to the `babysit` subcommands below and
  can act on your behalf.

The agent and the user share the same surface. There is no "monitoring
loop" inside babysit: babysit just exposes the wrapped command as a small
CLI/file API, and the agent decides when and how to use it.

## Subcommands (the agent's API)

```
babysit list                       # all sessions
babysit status                     # state of the wrapped command (JSON via --json)
babysit log [--tail N] [--raw]     # output from the wrapped command
babysit restart                    # kill + respawn the wrapped command
babysit kill                       # terminate it
babysit send "<text>"              # write text + newline to its stdin
```

When invoked from within the agent's tab, the session is resolved
implicitly via `$BABYSIT_SESSION_ID`. From elsewhere, pass
`--session <id>` or set the same env var. `latest` is also accepted as a
session reference.

## Keybindings (TUI)

| Key                | Action                                       |
| ------------------ | -------------------------------------------- |
| `Ctrl-1` / `Ctrl-2`| Switch active tab                            |
| Click tab bar      | Switch active tab                            |
| `r`                | Restart the wrapped command (after it exits) |
| `Ctrl-Q`           | Quit babysit                                 |

`Ctrl-C` is **not** intercepted by babysit — it flows through to the
active tab so you can interrupt the wrapped command or the agent normally.

## Session state on disk

Each session writes to `~/.babysit/sessions/<id>/`:

```
meta.json       # static info (cmd, agent, prompt, started_at, …)
status.json     # live state (running / exited / killed, exit_code)
output.log      # raw bytes from the wrapped command's PTY
control.sock    # Unix socket the subcommands talk to
```

The agent reads `status.json` / `output.log` indirectly via the
subcommands; both fall back to disk if the babysit process has gone away.

## Agent selection

Defaults to PATH lookup in order: `claude`, `codex`. Override with
`--agent <name>`. The agent is spawned in interactive mode inside its
own PTY; the babysit "manual" goes into the agent's system prompt
(via `--append-system-prompt` for Claude) or into the first user
message (other agents). `BABYSIT_SESSION_ID` is set in the agent's env.

## Build

```
cargo build --release
# binary at target/release/babysit
```
