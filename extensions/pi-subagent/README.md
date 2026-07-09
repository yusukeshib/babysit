# pi-subagent

A [pi](https://github.com/earendil-works/pi) extension that runs subagents as
**babysit-supervised PTY sessions**.

Unlike the stock subagent plugin (which `spawn()`s a child and blocks the parent
while streaming its stdout), every subagent here is a `pi --mode json -p` process
wrapped in a babysit worker. That fixes the three usual pain points:

| Pain point (stock plugin)        | Fix here                                                              |
| -------------------------------- | --------------------------------------------------------------------- |
| Can't see the buffer well        | The `--mode json` stream is recorded; `subagent_check` shows live progress (turns, recent tool calls, partial answer), or `attach` to the live buffer in a tmux split |
| Gets stuck / hangs               | The worker owns the PTY; an absolute `--timeout` (default 15m) is the safety valve. Idle-timeout is opt-in (a busy agent can be legitimately silent during a long tool call) |
| Can't do anything while it runs  | `subagent_run` returns a session id **immediately** — fully non-blocking |

## Tools (LLM-callable)

| Tool               | What it does                                                        |
| ------------------ | ------------------------------------------------------------------- |
| `subagent_run`     | Spawn a background subagent, return a session id immediately        |
| `subagent_check`   | Live progress for one id (turns, recent tool calls, partial answer), or list all |
| `subagent_send`    | Send a line to a running subagent's stdin (answer/steer)            |
| `subagent_wait`    | Block until a subagent exits, return exit code + final output       |
| `subagent_kill`    | Terminate a subagent                                                |

Subagents are launched **only by the agent** (via the `subagent_run` tool); there
is no human launch command by design.

## Commands (human, observe-only)

| Command       | What it does                                              |
| ------------- | --------------------------------------------------------- |
| `/subagents`  | Open an arrow-key (↑/↓) picker over the subagent list (like `/stash`). Pick a **running** one to attach to its live buffer in a tmux split (watch / fully take over; detach with `Ctrl-\ Ctrl-\`); pick a **finished** one to see its status and log tail. |

A widget above the editor shows running subagents live (⏳, plus ⚑ for flagged
sessions), so no manual list command is needed.

## How it works

`subagent_run` shells out to:

```sh
BABYSIT_DIR=~/.pi-subagents \
  babysit run -d --json --size 120x40 --timeout 15m \
    -- pi --mode json -p --no-session [--model M] [--tools ...] [--append-system-prompt PROMPT] "Task: ..."
```

A real PTY is used on purpose — `pi -p` hangs under plain pipes (`--no-tty`), and
the PTY is what lets you `attach` and take over. The subagent's `--mode json`
event stream is parsed from the babysit log: `subagent_check` summarizes turns +
recent tool calls, and `subagent_wait` returns the final assistant answer.
Subagents live in a dedicated babysit root (`~/.pi-subagents`) so they never
collide with your own manual `babysit` sessions.

> Idle-timeout is **off by default**: a subagent can be legitimately silent
> during a long tool call (e.g. a build/test), so idle detection would false-kill
> it. Pass `idleTimeout` explicitly only when you know the subagent streams
> steadily. The absolute `timeout` (default 15m) bounds runaways.

Named agents are discovered from `~/.pi/agent/agents/*.md` (and, with
`agentScope: "project"|"both"`, `<project>/.pi/agents/*.md`), same format as the
stock subagent example.

## Install

```sh
mkdir -p ~/.pi/agent/extensions/pi-subagent
ln -sf "$PWD/extensions/pi-subagent/index.ts"  ~/.pi/agent/extensions/pi-subagent/index.ts
ln -sf "$PWD/extensions/pi-subagent/agents.ts" ~/.pi/agent/extensions/pi-subagent/agents.ts
```

Then `/reload` in pi (or restart). Requires `babysit` and `pi` on `PATH`, and
tmux for attaching from `/subagents`.

## Environment overrides

| Var                    | Default          | Purpose                          |
| ---------------------- | ---------------- | -------------------------------- |
| `PI_SUBAGENT_DIR`      | `~/.pi-subagents`| babysit state root for subagents |
| `PI_SUBAGENT_BIN`      | `pi`             | agent binary to run              |
| `PI_SUBAGENT_BABYSIT`  | `babysit`        | babysit binary                   |
