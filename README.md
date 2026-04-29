# babysit

A transparent PTY wrapper that runs a shell command and exposes it to
*external* AI agents (Claude Code, Codex, …) via plain `babysit`
subcommands.

```
$ babysit make local-ci
babysit session ab12: make local-ci
  babysit log -s ab12 --tail 200
  babysit status -s ab12
Running tests...
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
$
```

(Use `--` only when the wrapped command starts with a flag babysit
would otherwise parse, e.g. `babysit -- --version`.)

There is no TUI, no alt-screen, no key grabbing. Output streams straight
to your terminal and stays in scrollback. Ctrl-C, Ctrl-Z, Ctrl-D and
every other keystroke flow through to the wrapped command exactly as if
you ran it directly.

The session id printed at the top is the only thing babysit adds.
Paste it into a Claude or Codex session running in another terminal:

> *"hey, can you tell me if anything goes wrong on babysit session `ab12`?"*

The agent reads state via the subcommands below. babysit does no
monitoring of its own — it just exposes the wrapped command as a small
CLI/file API; the agent decides when and how to use it.

## Subcommands (the agent's API)

```
babysit list                       # all sessions
babysit status -s <id>             # state of the wrapped command (--json for JSON)
babysit log -s <id> [--tail N]     # output from the wrapped command
babysit restart -s <id>            # kill + respawn the wrapped command
babysit kill -s <id>               # terminate it
babysit send -s <id> "<text>"      # write text + newline to its stdin
```

`-s <id>` is shorthand for `--session <id>`. From inside the wrapped
command itself the session is implicit via `$BABYSIT_SESSION_ID`. The
literal string `latest` is also accepted as a session reference.

## Session state on disk

Each session writes to `~/.babysit/sessions/<id>/`:

```
meta.json       # static info (cmd, started_at, …)
status.json     # live state (running / exited / killed, exit_code)
output.log      # raw bytes from the wrapped command's PTY
control.sock    # Unix socket the subcommands talk to
```

The subcommands prefer the live socket and fall back to disk if the
babysit process has gone away. `babysit list` flags sessions whose
owning babysit process has died as `dead`.

## Build

```
cargo build --release
# binary at target/release/babysit
```
