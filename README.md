# babysit

[![crates.io](https://img.shields.io/crates/v/babysit.svg)](https://crates.io/crates/babysit)
[![docs.rs](https://img.shields.io/docsrs/babysit)](https://docs.rs/babysit)
[![license](https://img.shields.io/crates/l/babysit.svg)](https://github.com/yusukeshib/babysit/blob/main/LICENSE)

Wrap a command in a PTY and control it from another terminal. The command runs
in the background under a worker that owns the PTY and records its output. From
anywhere you can read the output, screenshot the screen, send input, wait for
exit, or attach interactively.

Useful for scripts and coding agents that need to drive a command they didn't
start.

![babysit demo](assets/screenshot.gif)

```console
$ babysit run -d --json -- make local-ci   # {"id":"ab12"}
$ babysit log -s ab12 --tail 20
$ babysit screenshot -s ab12
$ babysit wait -s ab12
```

`babysit -- make local-ci` is the interactive shorthand. For scripting, use
`run -d --json` and capture the `id`.

## How it works

The command runs under a background worker that owns the PTY, logs all output,
and serves a Unix control socket. Your terminal is just attached to it, so you
can detach, re-attach, and query from other terminals.

State lives in `~/.babysit/sessions/<id>/`. Set `$BABYSIT_DIR` (absolute path) to
change the root. `status`, `log`, and `screenshot` work after the worker exits;
`send`, `key`, `restart`, and `kill` need it alive.

`-s <id>` selects a session; there is no "most recent" fallback. Inside the
wrapped command the id is exported as `$BABYSIT_SESSION_ID`.

## Install

Pick one:

```sh
# crates.io
cargo install babysit
```

```sh
# Nix flakes
nix profile install github:yusukeshib/babysit
```

```sh
# Prebuilt binary via install script
curl -fsSL https://raw.githubusercontent.com/yusukeshib/babysit/main/install.sh | sh
```

The install script drops a prebuilt binary in `~/.local/bin` (override with
`BABYSIT_INSTALL_DIR`, pin with `BABYSIT_VERSION`). `babysit upgrade`
self-updates cargo/binary installs; Nix installs are managed by Nix.

Run without installing: `nix run github:yusukeshib/babysit -- -- make local-ci`.

## Subcommands

| Command | Description |
| --- | --- |
| `run` | Wrap a command in a PTY (`babysit -- <cmd>` shorthand; `-d` detached; `--json` prints the id) |
| `list` (`ls`) | List sessions |
| `status` | Session state and exit code |
| `log` | Show output; `--tail`, `--grep`, `--follow`, `--since` |
| `screenshot` (`shot`) | Render the current screen; `--format plain\|ansi\|json`, `--trim` |
| `send` | Send text to stdin (`-n` no newline; `--json`) |
| `key` | Send named keys (`Enter`, `Up`, `Esc`, `C-c`, `F1`, …) |
| `expect` | Block until a regex appears (`--screen` matches the rendered TUI) |
| `wait-idle` | Block until output is quiet for `--settle` |
| `wait` | Block until exit, return the exit code |
| `resize` | Resize the terminal (`COLSxROWS`) |
| `flag` / `unflag` | Flag a session for attention / clear it |
| `restart` | Restart the command |
| `kill` | Terminate the command |
| `attach` / `detach` | Attach your terminal (detach: `Ctrl-\ Ctrl-\`) / detach others |
| `prune` | Delete finished or dead sessions |
| `upgrade` | Self-update |
| `config` | Shell completions: `eval "$(babysit config zsh\|bash)"` |

Each command documents its own flags and gotchas: `babysit help <command>`.
`babysit --help` covers the model and the typical agent loop.

## Build from source

```sh
cargo build --release   # target/release/babysit
```
