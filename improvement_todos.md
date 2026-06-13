# babysit improvement TODOs

Goal: make babysit a reliable substrate for an **AI agent driving a TUI to
completion**, with a clean **human-handoff** path. Each item is independently
shippable; keep CI (`cargo fmt --check` + `clippy -D warnings` + `cargo test`)
green at every commit.

Design decisions:
- **#5 flag/note**: unified. `babysit flag -s id "msg"` writes a `note` file in
  the session dir (works even if the worker is dead); `unflag` clears it. Shown
  in `ls` (⚑ + text) and `status`. Webhook/desktop notify is out of scope for now.
- **expect/wait-idle/key/wait-idle** are client-side pollers/encoders where
  possible to minimize protocol churn; size/resize and richer status go through
  the control socket.
- `regex` crate shared by `expect` and `log --grep`.

## 0. Scaffolding
- [x] Add `regex` dependency
- [x] Create this TODO file

## 1. Synchronization primitives (highest leverage)
- [x] `babysit expect -s id <REGEX> [--timeout DUR] [--since N] [--raw] [--json]`
      — block until the regex appears in output; exit 0 on match, 124 on timeout,
      1 if the session ends first. Default scans from current EOF (stream semantics).
- [x] `babysit wait-idle -s id [--settle 500ms] [--timeout DUR]`
      — return once output has been quiet for `settle`; 124 on timeout. Returns
      immediately if the session already finished.

## 2. Input fidelity
- [x] `babysit send --no-newline/-n` (protocol: `Send { text, newline }`)
- [x] `babysit key -s id <KEY>...` — named keys (Enter, Tab, Esc, Up/Down/Left/Right,
      Home, End, PageUp, PageDown, Delete, Backspace, Space, F1–F12, `C-x` ctrl combos)

## 3. Deterministic geometry
- [x] `babysit run --size COLSxROWS` (worker starts at this size, not 80x24)
- [x] `babysit resize -s id COLSxROWS` (control op `Resize { cols, rows }`)

## 4. Cheap change detection (token economy)
- [x] `status` includes `output_bytes` (stat log; always available) and
      `screen_seq` (AtomicU64 in Pane, bumped per processed chunk; live only)
- [x] Surface both in `--json` and the human `status` output

## 5. Human handoff (flag/note)
- [x] `babysit flag -s id [MESSAGE]` writes `note` file (default msg if omitted)
- [x] `babysit unflag -s id` removes it
- [x] `ls` shows ⚑ + note; `list --json` includes `note`
- [x] `status` prints the note line

## 6. Hang detection
- [x] `babysit run --idle-timeout DUR` — kill if no output for that long
      (last-activity timestamp in Pane; recurring check in the worker loop)

## 7. Log token economy
- [x] `babysit log --grep REGEX` (filter lines; compose with `--tail`)

## 8. Agent-readable help (`babysit --help`)
- [x] Rich top-level `long_about`: the worker/attach model, sessions & `-s`
      selector + `$BABYSIT_SESSION_ID`, the agent loop
      (run -d → expect/wait-idle → screenshot/log → send/key → wait), and the
      human-handoff path (flag → attach). Self-sufficient for an agent.
- [x] Tighten each subcommand's help text where the new flags land.

## 9. Wrap-up
- [x] Update README (subcommand table + new sections)
- [x] Update shell completions (bash + zsh) for new subcommands/flags
- [x] `cargo fmt`, `clippy -D warnings`, `cargo test` all green
- [x] PR
