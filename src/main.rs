mod agent;
mod cli;
mod control;
mod keys;
mod pane;
mod paths;
mod run;
mod session;
mod sub;
mod tui;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(cli::Command::List { json }) => sub::list(json).await,
        Some(cli::Command::Status { sel, json }) => sub::status(sel.session, json).await,
        Some(cli::Command::Log { sel, tail, raw }) => sub::log(sel.session, tail, raw).await,
        Some(cli::Command::Restart { sel }) => sub::restart(sel.session).await,
        Some(cli::Command::Kill { sel }) => sub::kill(sel.session).await,
        Some(cli::Command::Send { sel, text }) => sub::send(sel.session, text).await,
        None => {
            if cli.cmd.is_empty() {
                cli::print_help();
                std::process::exit(2);
            }
            run::run(cli.prompt, cli.agent, cli.cmd).await
        }
    }
}
