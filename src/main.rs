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
    // Short wrap form: `babysit -- <cmd> [args…]`. Handled before clap so
    // that `babysit listt` (a typo of `list`) goes through clap and gets
    // a proper "did you mean 'list'?" error instead of silently being
    // treated as a wrap of the non-existent command `listt`.
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() >= 2 && raw[1] == "--" {
        let cmd: Vec<String> = raw[2..].to_vec();
        if cmd.is_empty() {
            eprintln!("babysit: empty command after `--`");
            std::process::exit(2);
        }
        let code = run::run(cmd, None).await?;
        std::process::exit(code);
    }

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Command::Run { name, cmd } => {
            let code = run::run(cmd, name).await?;
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
