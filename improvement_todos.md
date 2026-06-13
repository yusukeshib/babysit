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

## 10. Agent-ergonomics round 2 (machine-readability + safety)

Follow-up after a critical review of how an agent actually drives the tool.

- [x] **Drop the `latest` fallback.** `resolve` is now `-s <id>` →
      `$BABYSIT_SESSION_ID` only; a missing selector errors loudly instead of
      silently acting on the most-recent session. Removed `resolve_latest`, the
      `latest` reserved-word check, and `latest` from both completions.
- [x] **`run --json`** prints `{"id":"…"}` so an agent captures the id without
      scraping the human banner.
- [x] **`send`/`key --json` return `{sent, offset}`**, where `offset` is the
      raw-log byte position captured *before* the input is injected — feed it to
      `expect --since` for a race-free, 2-command request/response.
- [x] **Default timeouts on `expect`/`wait-idle`** (30s) so a missing marker
      can't hang an agent; `0`/`none`/`off`/`never` opt back into infinite via
      `parse_timeout`. `wait` stays unbounded by design (long builds).
- [x] **`--json` on every mutating command** (send, key, resize, kill, restart,
      flag, unflag, detach, prune) for machine-checkable success.
- [x] **`status --json` keeps its shape after exit**: `output_bytes` is always
      derived from the log size; `screen_seq` is `null` once the worker is gone.
- [x] **`screenshot --format json` carries `screen_seq`** so a frame + its
      sequence number come back in one call.
- [x] **Boundary-safe incremental reads**: `read_slice` holds back a trailing
      truncated UTF-8 char or unterminated ANSI escape so `--since`/`--follow`/
      `expect` never split one (fixes mojibake + missed regex matches).
- [x] **Friendly dead-worker errors**: send/key/kill/restart/resize on a
      finished session say so instead of leaking a raw socket error.
- [x] Tests for `safe_prefix_len`/`escape_complete`/`parse_timeout`; README +
      completions + `--help` updated; fmt/clippy/test green; e2e smoke-tested.
