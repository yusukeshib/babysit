use clap::{Parser, Subcommand};

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
    /// Session id or name (defaults to $BABYSIT_SESSION_ID or `latest`)
    #[arg(short = 's', long, value_name = "ID_OR_NAME")]
    pub session: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Wrap a shell command in a PTY and expose it via the other subcommands
    Run {
        /// Optional name for the session (visible in `babysit list`)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The command to wrap, plus its arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
        cmd: Vec<String>,
    },
    /// List all babysit sessions
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show status of a session
    Status {
        #[command(flatten)]
        sel: SessionSel,
        #[arg(long)]
        json: bool,
    },
    /// Show recent output from the wrapped command
    Log {
        #[command(flatten)]
        sel: SessionSel,
        /// Last N lines (default: full)
        #[arg(long)]
        tail: Option<usize>,
        /// Include raw ANSI escapes (default: stripped)
        #[arg(long)]
        raw: bool,
    },
    /// Restart the wrapped command
    Restart {
        #[command(flatten)]
        sel: SessionSel,
    },
    /// Terminate the wrapped command
    Kill {
        #[command(flatten)]
        sel: SessionSel,
    },
    /// Send text to the wrapped command's stdin (newline appended)
    Send {
        #[command(flatten)]
        sel: SessionSel,
        /// Text to send
        text: String,
    },
    /// Delete sessions whose wrapped command has finished or whose owner died
    Prune {
        /// Print what would be deleted, but don't delete
        #[arg(long)]
        dry_run: bool,
    },
}
