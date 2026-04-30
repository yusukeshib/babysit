# babysit

[日本語版 README](README_JA.md)

Gives local terminal commands an API, so external AI agents (Claude
Code, Codex, …) can query their live output and exit state — the same
way they already query `gcloud` or `kubectl`.

**Your shell** — wrap the command you'd normally run. babysit prints a
session id, then runs the command transparently:

```console
$ babysit -- make local-ci
babysit session ab12: make local-ci
  babysit log -s ab12 --tail 200
  babysit status -s ab12
Running tests...
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
```

**Your agent, in another terminal** — hand it the session id (`ab12`)
and it can pull state on demand:

```console
$ babysit status -s ab12
session: ab12
cmd:     make local-ci
state:   exit:2
exit:    2

$ babysit log -s ab12 --tail 3
✓ test_a
✗ test_b: assertion failed
make: *** [local-ci] Error 1
```

babysit does no monitoring of its own — it exposes the wrapped command
as a small CLI/file API; the agent decides when and how to use it.

## Example prompts

Once you've handed your agent the session id, the prompts that work
well are the kind you'd give a coworker keeping an eye on the run:

> Watch session `ab12` with the `babysit` CLI. Tell me when
> `make local-ci` finishes, and if it fails, summarize which tests
> broke and why.

> Keep an eye on session `ab12` using the `babysit` command. Ping me
> only if something goes wrong.

The agent polls `babysit status` / `babysit log` on its own loop —
babysit itself does not push notifications.

## Why

Remote execution platforms (`gcloud`, `kubectl`, CI providers, …) ship
APIs that let an AI agent pull logs and status on demand. Local
execution doesn't: a command running in your terminal is a black box to
any agent that isn't already attached to that TTY, so analyzing an
in-progress run usually means copy-pasting scrollback by hand.

babysit closes that gap. Wrap a command once, and its live output and
exit state become queryable through a small CLI an agent already knows
how to drive — no scraping, no screen sharing, no extra daemon.

## Install

```
curl -fsSL https://raw.githubusercontent.com/yusukeshib/babysit/main/install.sh | sh
```

Drops a checksum-verified binary at `~/.local/bin/babysit` (override
with `BABYSIT_INSTALL_DIR`, pin a version with `BABYSIT_VERSION=v0.2.4`).
macOS / Linux on x86_64 or aarch64.

Or grab a prebuilt binary directly from
[GitHub Releases](https://github.com/yusukeshib/babysit/releases), or
build from source:

```
cargo install --git https://github.com/yusukeshib/babysit
```

Once installed, `babysit upgrade` self-updates to the latest release.

## Subcommands

```
$ babysit help
Wrap a shell command in a PTY and expose it to external agents via subcommands

Usage: babysit <COMMAND>

Commands:
  run      Wrap a shell command in a PTY and expose it via the other subcommands
  list     List all babysit sessions
  status   Show status of a session
  log      Show recent output from the wrapped command
  restart  Restart the wrapped command
  kill     Terminate the wrapped command
  send     Send text to the wrapped command's stdin (newline appended)
  prune    Delete sessions whose wrapped command has finished or whose owner died
  upgrade  Self-update to the latest version
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

Run `babysit help <command>` for flags and aliases. `babysit -- <cmd>`
is a short form for `babysit run <cmd>`.

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
output.log      # raw output from the wrapped command
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
