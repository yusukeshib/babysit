use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "babysit",
    version,
    about = "Wrap a shell command in a PTY and expose it to external agents via subcommands",
    long_about = LONG_ABOUT,
    arg_required_else_help = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Shown on `babysit --help`. Written so an AI agent can read it once and know
/// the whole model + the loop for driving an interactive command to completion.
const LONG_ABOUT: &str = "\
Wrap a command in a PTY and drive it from outside — built for an AI agent to run
an interactive/long-running command, watch it, type into it, and finish the task,
with a human able to step in.

MODEL
  The command runs under a headless background worker that owns the PTY, records
  all output to a log, and serves a per-session control socket. Your terminal (or
  an agent's `babysit` calls) are just clients of that worker, so you can detach,
  re-attach, and query state from anywhere. State lives in ~/.babysit/sessions/<id>/
  (override the ~/.babysit root with $BABYSIT_DIR, e.g. for tests or demos).

SELECTING A SESSION
  `run --json` prints the session id as JSON. Other commands take `-s <id>`;
  there is no most-recent fallback, so name the session you mean. Inside the
  wrapped command the id is in $BABYSIT_SESSION_ID, so nested calls can omit -s.

AGENT LOOP (typical)
  1. babysit run -d --json -- <cmd>    start detached; capture .id from the JSON
  2. babysit expect -s ID 'prompt>'    block until the program is ready
     babysit wait-idle -s ID           …or block until output settles
  3. babysit screenshot -s ID --trim   read the CURRENT screen (TUIs redraw in place)
     babysit log -s ID --tail 50       …or the raw output stream
  4. babysit send -s ID --json 'text'  type a line; returns {sent, offset}
     babysit key  -s ID Down Down Enter press named keys (arrows, Esc, C-c, F1…)
  5. babysit expect -s ID --since OFF 're'   wait for the reply race-free, using
                                             the `offset` returned by step 4
  6. repeat 2–5 until done, then:
     babysit wait -s ID                block for the exit code

  Poll cheaply: `status --json` reports `output_bytes` and `screen_seq`; if they
  haven't changed, nothing moved — no need to re-fetch a screenshot. Blocking
  commands (expect, wait-idle) time out after 30s by default so a stuck program
  can't hang you; pass --timeout 0 to wait indefinitely.

MATCHING TUIs vs STREAMS (gotchas)
  • `expect` scans the raw OUTPUT STREAM. Full-screen TUIs (menus, pickers)
    redraw in place, so the text you SEE isn't a contiguous run in the stream —
    use `babysit expect --screen 're'`, which matches the rendered screen grid.
  • Don't `expect` the text you just `send`: the PTY usually echoes your input
    back, so you'd match your own keystrokes. Wait for the program's reply
    marker instead.
  • `wait-idle` measures output VOLUME, so a spinner/progress bar that keeps
    redrawing never settles. For those, poll `screenshot`'s `screen_hash`
    (stable until the on-screen text changes) instead.

HUMAN HANDOFF
  Stuck or need approval? `babysit flag -s ID 'why'` marks the session (shown with
  a ⚑ in `babysit ls`); a human runs `babysit attach -s ID` to take over, then
  detaches (Ctrl-\\ Ctrl-\\). Clear it with `babysit unflag -s ID`.

MORE
  Every command has more flags than the loop above shows — run
  `babysit help <command>` (e.g. `babysit help expect`) before guessing.
";

/// Session selector flag, shared across read/operate subcommands.
///
/// Resolution: --session arg → $BABYSIT_SESSION_ID env. There is no
/// "most recently active" fallback — a missing selector errors out.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionSel {
    /// Session id (defaults to $BABYSIT_SESSION_ID; required otherwise)
    #[arg(short = 's', long, value_name = "ID")]
    pub session: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Wrap a shell command in a PTY and expose it via the other subcommands
    Run {
        /// Session id to assign (default: auto-generated). Must be unique;
        /// allowed characters: ASCII letters, digits, `-`, `_`, `.`.
        #[arg(long, value_name = "ID")]
        id: Option<String>,
        /// Run detached: start the command in the background and return
        /// immediately. babysit keeps supervising it; query later with
        /// `babysit log`/`status`.
        #[arg(short = 'd', long)]
        detach: bool,
        /// Internal: session id handed down by the parent when it re-execs
        /// itself to run detached. Not for direct use.
        #[arg(long = "detached-id", value_name = "ID", hide = true)]
        detached_id: Option<String>,
        /// Internal: state root handed down by the parent when it re-execs
        /// itself to run detached, so the worker reconstructs the same context
        /// without reading the environment. Not for direct use.
        #[arg(long = "root", value_name = "DIR", hide = true)]
        root: Option<String>,
        /// Run with plain pipes instead of a PTY. Programs that detect a
        /// non-tty then emit clean, line-oriented output — nicer for log
        /// scraping (e.g. by an agent). Disables interactive/TUI rendering.
        #[arg(long = "no-tty")]
        no_tty: bool,
        /// Auto-terminate the command after this long (e.g. 30s, 10m, 2h).
        /// A safety valve for unattended runs that may hang.
        #[arg(long, value_name = "DUR")]
        timeout: Option<String>,
        /// Auto-terminate if the command produces no output for this long
        /// (e.g. 30s, 5m) — catches hangs an absolute --timeout can't.
        #[arg(long = "idle-timeout", value_name = "DUR")]
        idle_timeout: Option<String>,
        /// Initial terminal size as COLSxROWS for deterministic TUI layout
        /// (default 80x24; an attaching client overrides it).
        #[arg(long, value_name = "COLSxROWS")]
        size: Option<String>,
        /// Pipe the attach view through this shell command before it reaches an
        /// attached client (e.g. a JSONL→human formatter). Both the replayed
        /// backlog and the live stream are transformed; only the interactive
        /// attach/stream view is affected — the recorded log and screenshots
        /// stay raw, so `log`/`screenshot` and machine scrapers are unaffected.
        #[arg(long = "view-cmd", value_name = "CMD")]
        view_cmd: Option<String>,
        /// Print the session id as JSON (`{"id":"..."}`) instead of the human
        /// banner — the machine-readable way for an agent to capture the id.
        #[arg(long)]
        json: bool,
        /// The command to wrap, plus its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        cmd: Vec<String>,
    },
    /// List all babysit sessions
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
        /// Continuously refresh the list in place (like `watch`), until
        /// interrupted with Ctrl-C. Ignored with --json.
        #[arg(short = 'w', long)]
        watch: bool,
        /// Refresh interval for --watch (e.g. 500ms, 2s).
        #[arg(long, value_name = "DUR", default_value = "1s")]
        interval: String,
    },
    /// Show status of a session
    ///
    /// `--json` reports `output_bytes` (raw-log size) and `screen_seq` (a
    /// counter bumped on every output chunk). Poll cheaply: if neither changed
    /// since your last check, the command produced nothing new — no need to
    /// re-screenshot. `screen_seq` is live-only; it is `null` once the worker
    /// has exited. Use `output_bytes` as the `--since` offset for `expect`/`log`
    /// to read only what arrives after a point.
    #[command(aliases = ["st", "info"])]
    Status {
        #[command(flatten)]
        sel: SessionSel,
        #[arg(long)]
        json: bool,
    },
    /// Show recent output from the wrapped command
    #[command(alias = "logs")]
    Log {
        #[command(flatten)]
        sel: SessionSel,
        /// Last N lines (default: full)
        #[arg(long)]
        tail: Option<usize>,
        /// Only show lines matching this regex (applied before --tail)
        #[arg(long, value_name = "REGEX")]
        grep: Option<String>,
        /// Include raw ANSI escapes (default: stripped)
        #[arg(long)]
        raw: bool,
        /// Only output bytes after this raw-log offset. Pair with --json to
        /// get the new offset back for incremental polling.
        #[arg(long, value_name = "BYTES")]
        since: Option<u64>,
        /// Stream new output live until the session exits (like tail -f).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Emit JSON `{text, offset, done}` instead of raw text.
        #[arg(long)]
        json: bool,
    },
    /// Capture the current visible screen of the wrapped command.
    ///
    /// Unlike `log` (which replays the raw output stream), this renders a
    /// virtual terminal grid — so TUIs that redraw in place (menus, full-screen
    /// apps) come out as the single frame currently on screen.
    ///
    /// `--format json` also carries `screen_seq` and `screen_hash`. `screen_seq`
    /// bumps on every output chunk (even identical redraws); `screen_hash` only
    /// changes when the on-screen *text* changes — so equal hashes across two
    /// frames mean the screen is genuinely settled (useful for spinners/progress
    /// bars that `wait-idle` can't detect).
    #[command(alias = "shot")]
    Screenshot {
        #[command(flatten)]
        sel: SessionSel,
        /// Output format: plain text, ANSI (color escapes kept), or structured JSON
        #[arg(long, value_enum, default_value = "plain")]
        format: ShotFormat,
        /// Drop trailing blank lines and trailing whitespace (smaller output)
        #[arg(long)]
        trim: bool,
    },
    /// Block until the wrapped command exits, then return its exit code
    Wait {
        #[command(flatten)]
        sel: SessionSel,
        /// Give up waiting after this long; exits 124 (e.g. 30s, 10m). No
        /// default — `wait` blocks until exit; guard long unattended runs with
        /// `run --timeout`/`--idle-timeout`. `0`/`none` also mean wait forever.
        #[arg(long, value_name = "DUR")]
        timeout: Option<String>,
    },
    /// Restart the wrapped command
    #[command(alias = "r")]
    Restart {
        #[command(flatten)]
        sel: SessionSel,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Terminate the wrapped command
    #[command(alias = "stop")]
    Kill {
        #[command(flatten)]
        sel: SessionSel,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Send text to the wrapped command's stdin (newline appended)
    #[command(alias = "type")]
    Send {
        #[command(flatten)]
        sel: SessionSel,
        /// Text to send
        text: String,
        /// Don't append a trailing newline
        #[arg(short = 'n', long = "no-newline")]
        no_newline: bool,
        /// Emit JSON `{sent, offset}`. `offset` is the raw-log byte position
        /// just before the input was injected — pass it to `expect --since`
        /// to wait for the response race-free.
        #[arg(long)]
        json: bool,
    },
    /// Send named keys to the wrapped command (Enter, Tab, Esc, Up, Down,
    /// Left, Right, Home, End, PageUp, PageDown, Delete, Backspace, Space,
    /// F1-F12, or `C-x` for Ctrl combinations)
    Key {
        #[command(flatten)]
        sel: SessionSel,
        /// One or more key names, applied in order
        #[arg(required = true, num_args = 1.., value_name = "KEY")]
        keys: Vec<String>,
        /// Emit JSON `{sent, offset}`. `offset` is the raw-log byte position
        /// just before the keys were injected — pass it to `expect --since`.
        #[arg(long)]
        json: bool,
    },
    /// Block until a regex appears in the output (expect-style)
    ///
    /// Don't `expect` the text you just `send`: a PTY echoes input back, so the
    /// raw stream contains your own keystrokes — wait for the program's *reply*
    /// marker, not what you typed. By default the whole log is scanned, so an
    /// already-printed marker still matches; use `--since`/`--from-now` to wait
    /// for a specific new response race-free.
    Expect {
        #[command(flatten)]
        sel: SessionSel,
        /// Regex to wait for
        #[arg(value_name = "REGEX")]
        pattern: String,
        /// Give up after this long; exits 124. Defaults to 30s so a missing
        /// marker can't hang an agent forever; pass `0`/`none` to wait
        /// indefinitely (e.g. 30s, 2m).
        #[arg(long, value_name = "DUR", default_value = "30s")]
        timeout: String,
        /// Start scanning from this raw-log byte offset. Capture it from
        /// `send`/`key --json` (`offset`) or `status --json` (`output_bytes`)
        /// BEFORE the action that triggers the output, to wait for that
        /// specific response race-free.
        #[arg(long, value_name = "BYTES")]
        since: Option<u64>,
        /// Only match output produced from now on (ignore the existing log).
        /// Default scans the whole log, so an already-printed marker matches.
        #[arg(long = "from-now")]
        from_now: bool,
        /// Match against raw output including ANSI escapes (default: stripped)
        #[arg(long)]
        raw: bool,
        /// Match against the RENDERED screen (the virtual-terminal grid)
        /// instead of the raw output stream. Use this for full-screen TUIs
        /// that redraw in place (menus, pickers), where the text you see on
        /// screen never appears as a contiguous run in the byte stream.
        /// Matches the whole current screen each poll; `--since`/`--from-now`
        /// (stream offsets) don't apply and are ignored.
        #[arg(long)]
        screen: bool,
        /// Emit JSON `{matched, offset}` instead of the matched text
        #[arg(long)]
        json: bool,
    },
    /// Block until the output has been quiet for a while (settled)
    ///
    /// Measures output *volume*: a spinner or progress bar that keeps redrawing
    /// never settles. For those, poll `screenshot --format json`'s `screen_hash`
    /// instead (stable until the on-screen text changes).
    #[command(name = "wait-idle")]
    WaitIdle {
        #[command(flatten)]
        sel: SessionSel,
        /// Required quiet period (e.g. 500ms, 2s)
        #[arg(long, default_value = "500ms", value_name = "DUR")]
        settle: String,
        /// Give up after this long; exits 124. Defaults to 30s; pass
        /// `0`/`none` to wait indefinitely.
        #[arg(long, value_name = "DUR", default_value = "30s")]
        timeout: String,
    },
    /// Resize the wrapped command's terminal
    Resize {
        #[command(flatten)]
        sel: SessionSel,
        /// New size as COLSxROWS (e.g. 120x40)
        #[arg(value_name = "COLSxROWS")]
        size: String,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Flag a session for human attention, with an optional note
    Flag {
        #[command(flatten)]
        sel: SessionSel,
        /// Note explaining why attention is needed
        #[arg(value_name = "MESSAGE")]
        message: Option<String>,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Clear a session's attention flag
    Unflag {
        #[command(flatten)]
        sel: SessionSel,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Attach your terminal to a session (detach with Ctrl-\ Ctrl-\)
    #[command(alias = "a")]
    Attach {
        #[command(flatten)]
        sel: SessionSel,
    },
    /// Detach any terminal currently attached to a session
    Detach {
        #[command(flatten)]
        sel: SessionSel,
        /// Emit a JSON result instead of a human message
        #[arg(long)]
        json: bool,
    },
    /// Delete sessions whose wrapped command has finished or whose owner died
    Prune {
        /// Print what would be deleted, but don't delete
        #[arg(long)]
        dry_run: bool,
        /// Emit the deleted/would-delete sessions as JSON
        #[arg(long)]
        json: bool,
    },
    /// Self-update to the latest version
    Upgrade,
    /// Print shell integration (completions) to eval from your shell rc,
    /// e.g. `eval "$(babysit config zsh)"`.
    Config {
        /// Shell to emit integration for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum Shell {
    Zsh,
    Bash,
}

/// Screenshot rendering format.
#[derive(ValueEnum, Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ShotFormat {
    /// Plain text grid, no escapes — cheapest for an LLM to read.
    Plain,
    /// Text with ANSI/SGR color escapes preserved (visual fidelity).
    Ansi,
    /// Structured JSON: size, cursor, and per-cell text + colors/attributes.
    Json,
}
