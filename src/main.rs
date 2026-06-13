mod cli;
mod control;
mod pane;
mod paths;
mod run;
mod session;
mod sub;
mod upgrade;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    // Rust sets SIGPIPE to SIG_IGN at startup, which turns a closed pipe into
    // a panic on the next write (`babysit log | head`) and is also inherited
    // by the wrapped command. Restore the default so we exit quietly on a
    // broken pipe and children get normal SIGPIPE behavior.
    #[cfg(unix)]
    unsafe {
        use nix::sys::signal::{SigHandler, Signal, signal};
        let _ = signal(Signal::SIGPIPE, SigHandler::SigDfl);
    }

    // Short wrap forms: `babysit [-d] -- <cmd> [args…]`. Handled before clap
    // so that `babysit listt` (a typo of `list`) goes through clap and gets a
    // proper "did you mean 'list'?" error instead of silently being treated
    // as a wrap of the non-existent command `listt`.
    let raw: Vec<String> = std::env::args().collect();
    let short = match raw.get(1).map(String::as_str) {
        Some("--") => Some((false, 2)),
        Some("-d") | Some("--detach") if raw.get(2).map(String::as_str) == Some("--") => {
            Some((true, 3))
        }
        _ => None,
    };
    if let Some((detach, start)) = short {
        let cmd: Vec<String> = raw[start..].to_vec();
        if cmd.is_empty() {
            eprintln!("babysit: empty command after `--`");
            std::process::exit(2);
        }
        let code = run::run(cmd, None, detach, None).await?;
        std::process::exit(code);
    }

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Command::Run {
            id,
            detach,
            detached_id,
            cmd,
        } => {
            let code = run::run(cmd, id, detach, detached_id).await?;
            std::process::exit(code);
        }
        cli::Command::List { json } => sub::list(json).await,
        cli::Command::Status { sel, json } => sub::status(sel.session, json).await,
        cli::Command::Log { sel, tail, raw } => sub::log(sel.session, tail, raw).await,
        cli::Command::Restart { sel } => sub::restart(sel.session).await,
        cli::Command::Kill { sel } => sub::kill(sel.session).await,
        cli::Command::Send { sel, text } => sub::send(sel.session, text).await,
        cli::Command::Prune { dry_run } => sub::prune(dry_run).await,
        cli::Command::Upgrade => {
            let code = upgrade::run()?;
            std::process::exit(code);
        }
    }
}
