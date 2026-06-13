# babysit

[日本語版 README](README_JA.md)

Gives local terminal commands an API, so external AI agents (Claude
Code, Codex, …) can query their live output and exit state — the same
way they already query `gcloud` or `kubectl`.

Two patterns it's built for:

1. **Hand a run to an agent** — wrap a command, give the agent the
   session id, and it pulls live output / exit state on demand (below).
2. **Let an agent orchestrate** — an agent loop launches tasks in the
   background (`babysit run -d`), lists them (`babysit ls`), reads their
   logs (`babysit log`), and joins on them (`babysit wait`). See
   [Driving agents in the background](#driving-agents-in-the-background).

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

With Nix (flakes):

```
nix run github:yusukeshib/babysit        # run without installing
nix profile install github:yusukeshib/babysit
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
  wait     Block until the wrapped command exits, then return its exit code
  attach   Attach your terminal to a session (detach with Ctrl-\ Ctrl-\)
  detach   Detach any terminal currently attached to a session
  prune    Delete sessions whose wrapped command has finished or whose owner died
  upgrade  Self-update to the latest version
  config   Print shell integration (completions)
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

Run `babysit help <command>` for flags and aliases. `babysit -- <cmd>`
is a short form for `babysit run <cmd>`.

By default a session gets an auto-generated id; pass `--id <id>` to
`run` to choose your own (e.g. `babysit run --id ci -- make local-ci`).
Ids must be unique and contain only letters, digits, `-`, `_`, `.`.

`-s <id>` is shorthand for `--session <id>` and accepts either the id
or the literal string `latest`. From inside the wrapped command itself
the session is implicit via `$BABYSIT_SESSION_ID`, so the flag can be
omitted.

## Attach / detach

The wrapped command always runs under a background worker that owns the
PTY; the terminal you see it in is just *attached* to that worker
(tmux-style). So you can come and go without stopping the command:

```console
$ babysit run -- make local-ci    # runs attached (auto-attaches)
…                                 # press Ctrl-\ Ctrl-\ to detach
$ babysit attach -s ci            # re-attach later, from anywhere
$ babysit detach -s ci            # kick off whoever's attached, keep it running
```

- **Detach hotkey:** `Ctrl-\ Ctrl-\` (press Ctrl-backslash twice) —
  leaves the command running and returns your shell. (A flow-control key
  like Ctrl-Q, or one TUIs grab like Ctrl-P, would be unreliable.)
- **Full-screen TUIs** (editors, `pi`, etc.) often enable an enhanced
  keyboard mode that re-encodes control keys, so the hotkey may not be
  detected. In that case detach from another terminal with
  `babysit detach -s <id>` — that path is independent of the keyboard and
  always works. babysit resets the terminal (alt-screen/mouse/…) on detach
  so your shell is left usable.
- `babysit attach -s <id>` replays the recent output, then streams live
  and forwards your keystrokes/resizes.
- `babysit detach -s <id>` detaches clients from another terminal.

### Start detached (`-d`)

`babysit -d -- <cmd>` (or `babysit run -d -- <cmd>`) starts the command
in the background and returns immediately, without attaching:

```console
$ babysit -d --id ci -- make local-ci
babysit session ci: make local-ci
  babysit log -s ci --tail 200
  babysit attach -s ci
$ # prompt returns right away; the agent polls `ci`, or you can attach
```

The worker survives this shell exiting. Output is captured to the
session log regardless of whether anyone is attached, so
`babysit log`/`status`/`send`/`kill` work the same either way.

`status` and `log` work even after babysit has exited — they fall back
to the on-disk state files. `restart`, `kill`, and `send` need the live
control socket and will fail if the babysit process is gone.

`babysit <unknown>` is treated as an unknown subcommand (with a
`did you mean …?` hint), not as a wrap attempt — use `babysit -- <cmd>`
or `babysit run <cmd>` to wrap.

## Shell integration

Add completions to your shell by eval'ing `babysit config`:

```sh
# zsh (~/.zshrc)
eval "$(babysit config zsh)"

# bash (~/.bashrc)
eval "$(babysit config bash)"
```

This completes subcommands and their aliases, per-command flags, and —
most usefully — live session ids for `-s` (read straight from
`~/.babysit/sessions`, plus the `latest` selector). `babysit run`
delegates to your shell's normal command completion.

## Driving agents in the background

babysit works well as a supervisor when an orchestrator (e.g. an agent
loop) launches other agents in the background and watches them:

```sh
# Launch a task, named after e.g. a ticket, headless so the log is clean.
babysit run -d --no-tty --id ENG-123 --timeout 30m -- my-agent --task ENG-123

babysit ls --json                 # what's running, and each one's state
babysit log -s ENG-123 --tail 50  # what is it doing right now
babysit wait -s ENG-123           # block until done; exit code = the agent's
```

- **`--no-tty`** runs the command with plain pipes instead of a PTY, so
  tools that detect a non-tty emit clean, line-oriented output that's much
  easier to scrape from the log (no full-screen redraw noise).
- **`--timeout <dur>`** auto-terminates a run that hangs (`30s`, `10m`,
  `2h`, `1d`, or bare seconds) — a safety valve for unattended loops.
- **`babysit wait`** blocks until the command exits and returns its exit
  code, so an orchestrator can join on tasks instead of busy-polling. With
  `--timeout` it gives up and exits `124` (the session keeps running).

Each wrapped command also sees `$BABYSIT_SESSION_ID`, so an agent can refer
to its own session. For per-task isolation (separate git worktrees, etc.),
pair babysit with a workspace manager.

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
