//! The babysit CLI — a thin dispatcher over the `babysit` library (see lib.rs).
//! All logic lives in the library; this binary only parses args, builds the
//! [`Babysit`] context (the ONE place the environment is consulted, via
//! `from_env`), and routes each subcommand to a method on it.

use anyhow::Result;
use babysit::{Babysit, attach, cli};
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
        let bs = Babysit::from_env()?;
        let code = bs
            .run(cmd, None, detach, None, false, None, None, None, false)
            .await?;
        std::process::exit(code);
    }

    let cli = cli::Cli::parse();

    // Build the context once. The detached worker re-exec carries its root
    // explicitly via `--root` so it never depends on inherited env; every other
    // invocation derives the root from `$BABYSIT_DIR` (or `~/.babysit`).
    let root_override = match &cli.command {
        cli::Command::Run { root: Some(r), .. } => Some(r.clone()),
        _ => None,
    };
    let bs = match root_override {
        Some(r) => Babysit::new(r),
        None => Babysit::from_env()?,
    };

    match cli.command {
        cli::Command::Run {
            id,
            detach,
            detached_id,
            no_tty,
            timeout,
            idle_timeout,
            size,
            json,
            root: _,
            cmd,
        } => {
            let code = bs
                .run(
                    cmd,
                    id,
                    detach,
                    detached_id,
                    no_tty,
                    timeout,
                    idle_timeout,
                    size,
                    json,
                )
                .await?;
            std::process::exit(code);
        }
        cli::Command::List {
            json,
            watch,
            interval,
        } => bs.list(json, watch, interval).await,
        cli::Command::Status { sel, json } => bs.status(sel.session, json).await,
        cli::Command::Log {
            sel,
            tail,
            grep,
            raw,
            since,
            follow,
            json,
        } => {
            bs.log(sel.session, tail, grep, raw, since, follow, json)
                .await
        }
        cli::Command::Screenshot { sel, format, trim } => {
            bs.screenshot(sel.session, format, trim).await
        }
        cli::Command::Restart { sel, json } => bs.restart(sel.session, json).await,
        cli::Command::Kill { sel, json } => bs.kill(sel.session, json).await,
        cli::Command::Send {
            sel,
            text,
            no_newline,
            json,
        } => bs.send(sel.session, text, !no_newline, json).await,
        cli::Command::Key { sel, keys, json } => bs.key(sel.session, keys, json).await,
        cli::Command::Expect {
            sel,
            pattern,
            timeout,
            since,
            from_now,
            raw,
            screen,
            json,
        } => {
            let code = bs
                .expect(
                    sel.session,
                    pattern,
                    timeout,
                    since,
                    from_now,
                    raw,
                    screen,
                    json,
                )
                .await?;
            std::process::exit(code);
        }
        cli::Command::WaitIdle {
            sel,
            settle,
            timeout,
        } => {
            let code = bs.wait_idle(sel.session, settle, timeout).await?;
            std::process::exit(code);
        }
        cli::Command::Resize { sel, size, json } => bs.resize(sel.session, size, json).await,
        cli::Command::Flag { sel, message, json } => bs.flag(sel.session, message, json).await,
        cli::Command::Unflag { sel, json } => bs.unflag(sel.session, json).await,
        cli::Command::Wait { sel, timeout } => {
            let code = bs.wait(sel.session, timeout).await?;
            std::process::exit(code);
        }
        cli::Command::Attach { sel } => {
            let code = attach::attach(&bs, sel.session).await?;
            std::process::exit(code);
        }
        cli::Command::Detach { sel, json } => attach::detach(&bs, sel.session, json).await,
        cli::Command::Prune { dry_run, json } => bs.prune(dry_run, json).await,
        cli::Command::Upgrade => {
            #[cfg(feature = "upgrade")]
            {
                let code = babysit::upgrade::run()?;
                std::process::exit(code);
            }
            #[cfg(not(feature = "upgrade"))]
            {
                eprintln!("babysit: built without upgrade support");
                std::process::exit(1);
            }
        }
        cli::Command::Config { shell } => {
            match shell {
                cli::Shell::Zsh => print!("{}", include_str!("completions/babysit.zsh")),
                cli::Shell::Bash => print!("{}", include_str!("completions/babysit.bash")),
            }
            Ok(())
        }
    }
}
