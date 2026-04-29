use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "babysit",
    version,
    about = "Run a command inside a TUI with a sidecar AI agent that can observe and operate it",
    long_about = None,
    arg_required_else_help = false,
    // When no subcommand is given, trailing args become the wrapped command.
    subcommand_negates_reqs = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// First message to send to the agent (its initial user-message)
    #[arg(short = 'p', long, value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// Agent CLI to spawn. Defaults: claude, codex (PATH lookup)
    #[arg(long, value_name = "NAME")]
    pub agent: Option<String>,

    /// Optional name for the session (visible in `babysit list`)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// The command to wrap, plus its arguments. Use `--` to separate.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub cmd: Vec<String>,
}

/// Session selector flag, shared across subcommands.
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
}

pub fn print_help() {
    use clap::CommandFactory;
    Cli::command().print_help().ok();
    println!();
}
