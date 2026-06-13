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
  re-attach, and query state from anywhere. State lives in ~/.babysit/sessions/<id>/.

SELECTING A SESSION
  `run` prints a session id. Other commands take `-s <id>` (or `latest`). Inside
  the wrapped command the id is exported as $BABYSIT_SESSION_ID, so nested calls
  can omit -s.

AGENT LOOP (typical)
  1. babysit run -d -- <cmd>           start detached; note the session id
  2. babysit expect -s ID 'prompt>'    block until the program is ready
     babysit wait-idle -s ID           …or block until output settles
  3. babysit screenshot -s ID --trim   read the CURRENT screen (TUIs redraw in place)
     babysit log -s ID --tail 50       …or the raw output stream
  4. babysit send -s ID 'text'         type a line; or:
     babysit key  -s ID Down Down Enter press named keys (arrows, Esc, C-c, F1…)
  5. repeat 2–4 until done, then:
     babysit wait -s ID                block for the exit code

  Poll cheaply: `status --json` reports `output_bytes` and `screen_seq`; if they
  haven't changed, nothing moved — no need to re-fetch a screenshot.

HUMAN HANDOFF
  Stuck or need approval? `babysit flag -s ID 'why'` marks the session (shown with
  a ⚑ in `babysit ls`); a human runs `babysit attach -s ID` to take over, then
  detaches (Ctrl-\\ Ctrl-\\). Clear it with `babysit unflag -s ID`.
";

/// Session selector flag, shared across read/operate subcommands.
///
/// Resolution: --session arg → $BABYSIT_SESSION_ID env → most recently active.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionSel {
    /// Session id (defaults to $BABYSIT_SESSION_ID or `latest`)
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
        /// The command to wrap, plus its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        cmd: Vec<String>,
    },
    /// List all babysit sessions
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show status of a session
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
        /// Give up waiting after this long (e.g. 30s, 10m); exits 124.
        #[arg(long, value_name = "DUR")]
        timeout: Option<String>,
    },
    /// Restart the wrapped command
    #[command(alias = "r")]
    Restart {
        #[command(flatten)]
        sel: SessionSel,
    },
    /// Terminate the wrapped command
    #[command(alias = "stop")]
    Kill {
        #[command(flatten)]
        sel: SessionSel,
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
    },
    /// Block until a regex appears in the output (expect-style)
    Expect {
        #[command(flatten)]
        sel: SessionSel,
        /// Regex to wait for
        #[arg(value_name = "REGEX")]
        pattern: String,
        /// Give up after this long; exits 124 (e.g. 30s, 2m)
        #[arg(long, value_name = "DUR")]
        timeout: Option<String>,
        /// Start scanning from this raw-log byte offset. Capture it from
        /// `status --json` (`output_bytes`) BEFORE the action that triggers the
        /// output, to wait for that specific response race-free.
        #[arg(long, value_name = "BYTES")]
        since: Option<u64>,
        /// Only match output produced from now on (ignore the existing log).
        /// Default scans the whole log, so an already-printed marker matches.
        #[arg(long = "from-now")]
        from_now: bool,
        /// Match against raw output including ANSI escapes (default: stripped)
        #[arg(long)]
        raw: bool,
        /// Emit JSON `{matched, offset}` instead of the matched text
        #[arg(long)]
        json: bool,
    },
    /// Block until the output has been quiet for a while (settled)
    #[command(name = "wait-idle")]
    WaitIdle {
        #[command(flatten)]
        sel: SessionSel,
        /// Required quiet period (e.g. 500ms, 2s)
        #[arg(long, default_value = "500ms", value_name = "DUR")]
        settle: String,
        /// Give up after this long; exits 124
        #[arg(long, value_name = "DUR")]
        timeout: Option<String>,
    },
    /// Resize the wrapped command's terminal
    Resize {
        #[command(flatten)]
        sel: SessionSel,
        /// New size as COLSxROWS (e.g. 120x40)
        #[arg(value_name = "COLSxROWS")]
        size: String,
    },
    /// Flag a session for human attention, with an optional note
    Flag {
        #[command(flatten)]
        sel: SessionSel,
        /// Note explaining why attention is needed
        #[arg(value_name = "MESSAGE")]
        message: Option<String>,
    },
    /// Clear a session's attention flag
    Unflag {
        #[command(flatten)]
        sel: SessionSel,
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
    },
    /// Delete sessions whose wrapped command has finished or whose owner died
    Prune {
        /// Print what would be deleted, but don't delete
        #[arg(long)]
        dry_run: bool,
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
