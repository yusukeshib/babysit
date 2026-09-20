//! The babysit CLI — a thin dispatcher over the `babysit` library (see lib.rs).
//! All logic lives in the library; this binary only parses args, builds the
//! [`Babysit`] context (the ONE place the environment is consulted, via
//! `from_env`), and routes each subcommand to a method on it.

use anyhow::{Context, Result, bail};
use babysit::{Babysit, attach, cli, remote, session};
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
            .run(
                cmd, None, detach, None, false, None, None, None, None, false,
            )
            .await?;
        std::process::exit(code);
    }

    let cli = cli::Cli::parse();

    if matches!(cli.command, cli::Command::RemoteInfo) {
        return remote::print_info();
    }

    // Route non-local operational commands over SSH. The remote worker itself
    // is invoked without --host and therefore uses its own local state root.
    if cli.host != "local" {
        let host = cli.host.clone();
        match cli.command {
            cli::Command::Run {
                id,
                detach,
                detached_id,
                root,
                no_tty,
                timeout,
                idle_timeout,
                size,
                view_cmd,
                json,
                cmd,
            } => {
                if detached_id.is_some() || root.is_some() {
                    bail!("internal worker flags cannot be routed remotely");
                }
                if !detach {
                    remote::verify(&host).await?;
                }
                let generated = id.is_none();
                let id =
                    id.unwrap_or_else(|| format!("{}{}", session::new_id(), session::new_id()));
                let mut args = vec![
                    "run".into(),
                    "--id".into(),
                    id.clone(),
                    "--detach".into(),
                    "--json".into(),
                ];
                if no_tty {
                    args.push("--no-tty".into());
                }
                if let Some(value) = timeout {
                    args.extend(["--timeout".into(), value]);
                }
                if let Some(value) = idle_timeout {
                    args.extend(["--idle-timeout".into(), value]);
                }
                if let Some(value) = size {
                    args.extend(["--size".into(), value]);
                }
                if let Some(value) = view_cmd {
                    args.extend(["--view-cmd".into(), value]);
                }
                args.push("--".into());
                args.extend(cmd.clone());
                let output = remote::capture(&host, &args).await?;
                let created = if output.status.success() {
                    true
                } else if generated && output.status.code() == Some(255) {
                    remote::confirm_session(&host, &id).await?
                } else {
                    false
                };
                if !created {
                    let detail = String::from_utf8_lossy(&output.stderr);
                    if output.status.code() == Some(255) {
                        bail!(
                            "remote run outcome is unknown for session `{id}`; check with `babysit --host {} status -s {id}`\n{}",
                            host,
                            detail.trim()
                        );
                    }
                    bail!("remote run failed ({}): {}", output.status, detail.trim());
                }
                if json {
                    println!("{}", serde_json::json!({"id": id}));
                } else {
                    eprintln!("babysit: [{}] session {}: {}", host, id, cmd.join(" "));
                }
                if detach {
                    std::process::exit(0);
                }
                let code = remote::attach(&host, id, true).await?;
                std::process::exit(code);
            }
            cli::Command::Attach { sel, no_reconnect } => {
                let id = sel
                    .session
                    .or_else(|| std::env::var("BABYSIT_SESSION_ID").ok())
                    .context("remote attach requires --session <ID>")?;
                let code = remote::attach(&host, id, !no_reconnect).await?;
                std::process::exit(code);
            }
            cli::Command::Config { .. }
            | cli::Command::RemoteInfo
            | cli::Command::RemoteBridge { .. } => {
                bail!("this command cannot be routed with --host");
            }
            _ => {
                let args = remote::strip_host_args(&raw[1..])?;
                let code = remote::proxy(&host, &args).await?;
                std::process::exit(code);
            }
        }
    }

    // Build the context once. The detached worker re-exec carries its root
    // explicitly via `--root` so it never depends on inherited env; every other
    // invocation derives the root from `$BABYSIT_DIR` (or `~/.babysit`).
    let root_override = match &cli.command {
        cli::Command::Run { root: Some(r), .. } => Some(r.clone()),
        _ => None,
    };
    // This is the `babysit` CLI binary, so mark the context cli_mode (the
    // --root branch is the detached-worker re-exec, still the CLI): it exposes
    // BABYSIT_SESSION_ID to wrapped commands and prints the attach banner.
    // Library embedders construct Babysit::new directly and stay invisible.
    let bs = match root_override {
        Some(r) => Babysit::new(r).cli_mode(),
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
            view_cmd,
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
                    view_cmd,
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
        cli::Command::Attach {
            sel,
            no_reconnect: _,
        } => {
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
        cli::Command::RemoteBridge { sel } => remote::bridge(&bs, sel.session).await,
        cli::Command::RemoteInfo => unreachable!(),
    }
}
