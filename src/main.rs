mod cli;
mod control;
mod pane;
mod paths;
mod run;
mod session;
mod sub;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(cli::Command::Run { name, cmd }) => {
            let code = run::run(cmd, name).await?;
            std::process::exit(code);
        }
        Some(cli::Command::List { json }) => sub::list(json).await,
        Some(cli::Command::Status { sel, json }) => sub::status(sel.session, json).await,
        Some(cli::Command::Log { sel, tail, raw }) => sub::log(sel.session, tail, raw).await,
        Some(cli::Command::Restart { sel }) => sub::restart(sel.session).await,
        Some(cli::Command::Kill { sel }) => sub::kill(sel.session).await,
        Some(cli::Command::Send { sel, text }) => sub::send(sel.session, text).await,
        Some(cli::Command::Prune { dry_run }) => sub::prune(dry_run).await,
        None => {
            if cli.cmd.is_empty() {
                cli::print_help();
                std::process::exit(2);
            }
            let code = run::run(cli.cmd, cli.name).await?;
            std::process::exit(code);
        }
    }
}
