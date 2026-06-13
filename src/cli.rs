use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "babysit",
    version,
    about = "Wrap a shell command in a PTY and expose it to external agents via subcommands",
    long_about = None,
    arg_required_else_help = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

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
