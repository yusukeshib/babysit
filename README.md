# babysit

A transparent PTY wrapper that runs a shell command and exposes it to
*external* AI agents (Claude Code, Codex, …) via plain `babysit`
subcommands.

```console
$ babysit make local-ci
babysit session ab12: make local-ci
  babysit log -s ab12 --tail 200
  babysit status -s ab12
Running tests...
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
$ echo $?
2
```

`babysit run make local-ci` is the explicit form and behaves identically.
Use `--` (or the `run` form) when the wrapped command's name collides
with a babysit subcommand: `babysit -- list` or `babysit run list`.

There is no TUI, no alt-screen, no key grabbing. Output streams straight
to your terminal and stays in scrollback. Ctrl-C, Ctrl-Z, Ctrl-D and
every other keystroke flow through to the wrapped command exactly as if
you ran it directly. Babysit exits with the same exit code as the
wrapped command, and to "quit babysit" you just kill the wrapped command
(Ctrl-C, `exit`, etc.).

The session id printed at the top is the only thing babysit adds.
Paste it into a Claude or Codex session running in another terminal:

> *"hey, can you tell me if anything goes wrong on babysit session `ab12`?"*

The agent reads state via the subcommands below. babysit does no
monitoring of its own — it just exposes the wrapped command as a small
CLI/file API; the agent decides when and how to use it.

## Subcommands

```
babysit [--name NAME] <cmd> [args…]        # wrap a command (bare form)
babysit run [--name NAME] <cmd> [args…]    # wrap a command (explicit form)
babysit list [--json]                       # all sessions
babysit status -s <id> [--json]             # state of the wrapped command
babysit log -s <id> [--tail N] [--raw]      # output (ANSI stripped unless --raw)
babysit restart -s <id>                     # kill + respawn the wrapped command
babysit kill -s <id>                        # terminate it
babysit send -s <id> "<text>"               # write text + newline to its stdin
babysit prune [--dry-run]                   # delete finished / dead sessions
```

`-s <id>` is shorthand for `--session <id>` and accepts either the id,
a name set via `--name`, or the literal string `latest`. From inside
the wrapped command itself the session is implicit via
`$BABYSIT_SESSION_ID`, so the flag can be omitted.

`status` and `log` work even after babysit has exited — they fall back
to the on-disk state files. `restart`, `kill`, and `send` need the live
control socket and will fail if the babysit process is gone.

## Session state on disk

Each session writes to `~/.babysit/sessions/<id>/`:

```
meta.json       # static info (cmd, started_at, …)
status.json     # live state (running / exited / killed, exit_code)
output.log      # raw bytes from the wrapped command's PTY
control.sock    # Unix socket the subcommands talk to
```

`babysit list` flags sessions whose owning babysit process has died as
`dead` (e.g. crash, kill -9, reboot before a clean exit could be
recorded). Run `babysit prune` to clear out anything that's no longer
running.

## Build

```
cargo build --release
# binary at target/release/babysit
```
