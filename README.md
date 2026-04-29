# babysit

[日本語版 README](README_JA.md)

A transparent PTY wrapper for local commands — so external AI agents
(Claude Code, Codex, …) can read their live output the same way they
already read `gcloud` or `kubectl` logs.

```console
$ babysit -- make local-ci
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

Then, from another terminal, hand the session id to your agent:

> *"hey, can you tell me if anything goes wrong on babysit session `ab12`?"*

The agent calls `babysit log` / `babysit status` to read state. babysit
does no monitoring of its own — it exposes the wrapped command as a
small CLI/file API; the agent decides when and how to use it.

## Why

Remote execution platforms (`gcloud`, `kubectl`, CI providers, …) ship
APIs that let an AI agent pull logs and status on demand. Local
execution doesn't: a command running in your terminal is a black box to
any agent that isn't already attached to that TTY, so analyzing an
in-progress run usually means copy-pasting scrollback by hand.

babysit closes that gap. Wrap a command once, and its live output and
exit state become queryable through a small CLI an agent already knows
how to drive — no scraping, no screen sharing, no extra daemon.

## Stays out of your way

No TUI, no alt-screen, no key grabbing. Output streams straight to your
terminal and stays in scrollback. Ctrl-C, Ctrl-Z, Ctrl-D and every
other keystroke flow through to the wrapped command exactly as if you
ran it directly. babysit exits with the same exit code as the wrapped
command, and to "quit babysit" you just kill the wrapped command
(Ctrl-C, `exit`, etc.). The session id printed at the top is the only
thing babysit adds.

## Install

Grab a prebuilt binary from
[GitHub Releases](https://github.com/yusukeshib/babysit/releases)
(macOS / Linux), or install from source:

```
cargo install --git https://github.com/yusukeshib/babysit
```

Once installed, `babysit upgrade` self-updates to the latest release.

## Subcommands

```
babysit -- <cmd> [args…]                    # wrap a command (short form)
babysit run [--name NAME] <cmd> [args…]     # wrap a command (named form)
babysit list [--json]                       # all sessions          (alias: ls)
babysit status -s <id> [--json]             # state of wrapped cmd  (aliases: st, info)
babysit log -s <id> [--tail N] [--raw]      # output, ANSI stripped (alias: logs)
babysit restart -s <id>                     # kill + respawn        (alias: r)
babysit kill -s <id>                        # terminate it          (alias: stop)
babysit send -s <id> "<text>"               # write text + newline  (alias: type)
babysit prune [--dry-run]                   # delete finished / dead sessions
babysit upgrade                             # self-update to latest release
```

`-s <id>` is shorthand for `--session <id>` and accepts either the id,
a name set via `--name`, or the literal string `latest`. From inside
the wrapped command itself the session is implicit via
`$BABYSIT_SESSION_ID`, so the flag can be omitted.

`status` and `log` work even after babysit has exited — they fall back
to the on-disk state files. `restart`, `kill`, and `send` need the live
control socket and will fail if the babysit process is gone.

`babysit <unknown>` is treated as an unknown subcommand (with a
`did you mean …?` hint), not as a wrap attempt — use `babysit -- <cmd>`
or `babysit run <cmd>` to wrap.

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

## Build from source

```
cargo build --release
# binary at target/release/babysit
```
