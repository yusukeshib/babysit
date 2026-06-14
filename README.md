# babysit

Wrap a command in a PTY and control it from the outside through a small CLI.
The command keeps running in the background under a worker process that owns the
PTY and records everything it prints; from any terminal you can read its output,
screenshot its current screen, send input, wait for it to exit, or attach to it
interactively (tmux-style detach and re-attach).

This makes it easy for a script — or an AI coding agent like Claude Code or
Codex — to drive a command it didn't start and react to what it does.

```console
$ babysit run -d --json -- make local-ci   # wrap detached; prints {"id":"ab12"}
$ babysit log -s ab12 --tail 20            # read recent output from anywhere
$ babysit screenshot -s ab12               # render the current screen of a TUI
$ babysit wait -s ab12                     # block until it exits; returns exit code
```

(`babysit -- make local-ci` is the interactive shorthand; for scripting/agents
use `run -d --json` and capture the `id`.)

## How it works

The wrapped command runs under a background worker that owns the PTY, captures
all output to a log, and serves a Unix control socket. The terminal you launched
from is just *attached* to that worker, so you can detach and re-attach, and
read state from another terminal at any time.

State lives in `~/.babysit/sessions/<id>/` (`meta.json`, `status.json`,
`output.log`, `control.sock`). `status`, `log`, and `screenshot` work even after
the worker exits (they fall back to the files); `send`, `key`, `restart`, and
`kill` need it alive.

`-s <id>` selects a session. There is no "most recent" fallback: a command with
neither `-s` nor `$BABYSIT_SESSION_ID` errors out, so a forgotten selector fails
loudly instead of acting on the wrong session. Inside the wrapped command the id
is exported as `$BABYSIT_SESSION_ID`, so nested calls can omit `-s`.

## Install

```sh
cargo install babysit                            # from crates.io
nix profile install github:yusukeshib/babysit    # with Nix flakes
```

No Rust toolchain? Download a prebuilt release binary instead (macOS/Linux,
x86_64/aarch64) — installs to `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/yusukeshib/babysit/main/install.sh | sh
```

Override the location with `BABYSIT_INSTALL_DIR`, or pin a version with
`BABYSIT_VERSION=v0.2.4` (default: latest release).

Or run it without installing: `nix run github:yusukeshib/babysit -- -- make local-ci`.

For a `cargo install` / released binary, `babysit upgrade` self-updates to the
latest release (Nix installs are managed by Nix instead).

## Subcommands

| Command | Description |
| --- | --- |
| `run` | Wrap a command in a PTY (`babysit -- <cmd>` is shorthand; `-d` runs it detached; `--json` prints `{"id":"…"}`) |
| `list` (`ls`) | List all sessions |
| `status` | Show a session's state and exit code |
| `log` | Show output; `--tail N`, `--grep <re>`, `--follow` (live, like `tail -f`); `--since` for incremental reads (see [Waiting on output](#waiting-on-output)) |
| `screenshot` (`shot`) | Render the *current* screen via a virtual terminal (readable for full-screen TUIs that redraw in place); `--format plain\|ansi\|json`, `--trim` |
| `send` | Send text to the command's stdin (`-n`/`--no-newline` to omit the newline; `--json` returns `{sent, offset}`) |
| `key` | Send named keys (`Enter`, `Up`, `Esc`, `C-c`, `F1`, …) |
| `expect` | Block until a regex appears in the output |
| `wait-idle` | Block until output has been quiet for `--settle` |
| `wait` | Block until the command exits, returning its exit code |
| `resize` | Resize the wrapped command's terminal (`COLSxROWS`) |
| `flag` / `unflag` | Flag a session with a note (shown with ⚑ in `ls`) / clear it |
| `restart` | Restart the wrapped command |
| `kill` | Terminate the wrapped command |
| `attach` / `detach` | Attach your terminal to a session (detach: `Ctrl-\ Ctrl-\`) / detach others |
| `prune` | Delete finished or dead sessions |
| `upgrade` | Self-update to the latest release |
| `config` | Print shell completions: `eval "$(babysit config zsh\|bash)"` |

Run `babysit help <command>` for flags and aliases.

## Run options

- `-d` — run detached: start in the background and return immediately (survives
  your shell).
- `--no-tty` — use plain pipes instead of a PTY, for clean line-oriented output.
- `--timeout <30s|10m|2h>` — auto-terminate after a fixed duration.
- `--idle-timeout <5m>` — auto-terminate if the command produces no output for
  that long.
- `--size <120x40>` — fix the terminal size so a full-screen program lays out
  deterministically (an attaching terminal overrides it).

## Waiting on output

- `expect <regex>` scans the whole log by default, so a marker that has already
  been printed still matches. To wait for a *specific* response without races,
  capture an offset before the action and pass it as `--since <bytes>`: either
  the `offset` returned by `send`/`key --json` (the byte position just before
  your input was injected) or `output_bytes` from `status --json`. `--from-now`
  ignores the existing log entirely.
- `expect` and `wait-idle` time out after **30s by default** so a stuck program
  can't hang an agent; pass `--timeout 0` (or `none`) to wait indefinitely.
  `wait` has no default timeout — guard long unattended runs with
  `run --timeout`/`--idle-timeout` instead.
- `wait-idle --settle <dur>` returns once output has been quiet for that long.
- `status --json` reports `output_bytes` and `screen_seq`; if neither changed
  since the last check, the command hasn't produced anything new. (`screen_seq`
  is live-only — it is `null` once the worker has exited.)
- `screenshot --format json` also carries `screen_seq`, so you can fetch a frame
  and its sequence number in one call.
- `log --grep <re>` filters to matching lines. `log --since <bytes> --json`
  returns `{text, offset, done}` for incremental polling — pass the returned
  `offset` back as the next `--since` to read only what's new.
- Mutating commands (`send`, `key`, `kill`, `restart`, `resize`, `flag`,
  `unflag`, `detach`, `prune`) accept `--json` for a machine-readable result.

## Build from source

```sh
cargo build --release   # binary at target/release/babysit
```
