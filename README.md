# babysit

Wrap a local command in a PTY and expose its live output and exit state
through a small CLI — so an AI agent (Claude Code, Codex, …) can query a
running command on demand, the same way it already queries `gcloud` or
`kubectl`.

```console
$ babysit -- make local-ci      # wrap a command; prints a session id (e.g. ab12)
$ babysit log -s ab12 --tail 20 # another terminal/agent pulls output on demand
$ babysit status -s ab12        # state + exit code
$ babysit wait -s ab12          # block until it exits; returns its exit code
```

## How it works

The wrapped command runs under a background worker that owns the PTY,
captures all output to a log, and serves a Unix control socket. The
terminal you launched from is just *attached* to that worker (tmux-style),
so you can detach and re-attach, and an agent in another terminal can read
state at any time. babysit does no monitoring of its own — it just exposes
the command as a queryable CLI.

State lives in `~/.babysit/sessions/<id>/` (`meta.json`, `status.json`,
`output.log`, `control.sock`). `status`/`log` work even after the worker
exits (they fall back to the files); `restart`/`kill`/`send` need it alive.

## Install

```sh
cargo install babysit
```

Then `babysit upgrade` self-updates to the latest release.

## Subcommands

| Command | Description |
| --- | --- |
| `run` | Wrap a command in a PTY (`babysit -- <cmd>` is shorthand; `-d` starts it detached in the background) |
| `list` (`ls`) | List all sessions |
| `status` | Show a session's state and exit code |
| `log` | Show output; `--tail N`, `--grep <re>`, `--since <off> --json`, `--follow` for incremental/live reads |
| `screenshot` (`shot`) | Render the *current* screen via a virtual terminal — see below |
| `send` | Send text to the command's stdin (`-n`/`--no-newline` to omit the newline) |
| `key` | Send named keys (`Enter`, `Up`, `Esc`, `C-c`, `F1`, …) |
| `expect` | Block until a regex appears in the output (expect-style) |
| `wait-idle` | Block until output has been quiet for `--settle` |
| `wait` | Block until the command exits, returning its exit code |
| `resize` | Resize the wrapped command's terminal (`COLSxROWS`) |
| `flag` / `unflag` | Flag a session for human attention (shown with ⚑ in `ls`) / clear it |
| `restart` | Restart the wrapped command |
| `kill` | Terminate the wrapped command |
| `attach` / `detach` | Attach your terminal to a session (detach: `Ctrl-\ Ctrl-\`) / detach others |
| `prune` | Delete finished or dead sessions |
| `upgrade` | Self-update to the latest release |
| `config` | Print shell completions: `eval "$(babysit config zsh\|bash)"` |

Run `babysit help <command>` for flags and aliases. `-s <id>` selects a
session (or `latest`); inside the wrapped command it's implicit via
`$BABYSIT_SESSION_ID`.

### Useful flags for background agent loops

- `babysit run -d` — start detached, return immediately (survives your shell).
- `--no-tty` — use plain pipes instead of a PTY, for clean line-oriented logs.
- `--timeout <30s|10m|2h>` — auto-terminate a hung run.
- `--idle-timeout <5m>` — auto-terminate if the command goes silent for that long.
- `--size <120x40>` — fix the terminal size so a TUI lays out deterministically.

### Driving a TUI to completion (agent loop)

```sh
id=$(babysit run -d -- some-tui | awk '{print $3; exit}')  # or use --id
babysit expect -s "$id" 'ready>'        # wait until it's ready
babysit screenshot -s "$id" --trim      # read the CURRENT screen
babysit key -s "$id" Down Down Enter    # navigate with named keys
babysit send -s "$id" 'some text'       # or type a line
babysit wait -s "$id"                    # block for the exit code
```

- **`expect`** scans the whole log by default, so an already-printed marker
  still matches. To wait for a response to a *specific* action race-free,
  capture `output_bytes` from `status --json` before the action and pass it as
  `expect --since <bytes>`; use `--from-now` for pure streaming semantics.
- **Cheap polling:** `status --json` reports `output_bytes` and `screen_seq`.
  If neither changed since your last poll, nothing moved — skip the screenshot.
- **`log --grep <re>`** filters server-side so you only read matching lines.

### Human handoff

When the agent needs a human (approval, a captcha, an ambiguous prompt):

```sh
babysit flag -s "$id" 'need approval to deploy'   # ⚑ shows in `babysit ls`
babysit attach -s "$id"                            # a human takes over
babysit unflag -s "$id"                            # clear once handled
```

## `babysit screenshot`

`log` replays the raw output *stream*, which is unreadable for full-screen
TUIs that redraw in place (menus, progress bars, `htop`). `screenshot`
feeds output through a virtual terminal and renders the **single frame
currently on screen**:

```console
$ babysit screenshot -s ab12 --trim
  npm
> pnpm
  yarn
```

- `--format plain` (default) — plain text, cheapest for an agent to read.
- `--format ansi` — keeps ANSI/SGR color escapes.
- `--format json` — screen size, cursor, and per-cell `char` + attributes
  (`fg`/`bg`/`bold`/`inverse`/…), so an agent can tell e.g. which row is
  selected (often marked only by inverse video).
- `--trim` drops trailing blank lines/whitespace.

For an exited session the screen is reconstructed from the on-disk log.

## Build from source

```sh
cargo build --release   # binary at target/release/babysit
```
