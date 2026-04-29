//! Agent CLI selection and prompt construction.
//!
//! `babysit` itself does not "monitor" anything — its job is to expose the
//! wrapped command via subcommands. The agent (claude / codex / …) reads
//! and acts on the wrapped command using those subcommands. This module
//! decides which agent to launch and shapes the initial messages so the
//! agent knows the API surface and the user's instruction.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

/// Built-in agent CLIs in detection-order. The first one found in PATH wins.
pub const KNOWN_AGENTS: &[&str] = &["claude", "codex"];

#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Display name (e.g. "claude").
    pub name: String,
    /// Resolved absolute path to the binary.
    pub bin: PathBuf,
    /// CLI arguments. May include flags that carry the system-prompt; otherwise
    /// the system prompt is prepended to the first user message instead.
    pub args: Vec<String>,
    /// First user-message to type into the agent's PTY after it starts.
    pub initial_message: String,
}

/// Resolve the agent to use. `explicit` overrides PATH detection.
pub fn resolve(explicit: Option<&str>) -> Result<(String, PathBuf)> {
    if let Some(name) = explicit {
        let bin = which::which(name)
            .with_context(|| format!("agent `{name}` not found in PATH"))?;
        return Ok((name.to_string(), bin));
    }
    for name in KNOWN_AGENTS {
        if let Ok(bin) = which::which(name) {
            return Ok(((*name).to_string(), bin));
        }
    }
    Err(anyhow!(
        "no agent CLI found. Tried: {}. Use --agent NAME to override.",
        KNOWN_AGENTS.join(", ")
    ))
}

/// Build a complete `AgentSpec` from a resolved (name, bin) plus the user's prompt.
///
/// Known agents (claude, codex) get their system prompt via a CLI flag if
/// supported; everything else falls back to a single concatenated user-message.
pub fn build_spec(
    name: String,
    bin: PathBuf,
    user_prompt: Option<String>,
    session_id: &str,
) -> AgentSpec {
    let manual = babysit_manual(session_id);
    let user_msg = user_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no instruction was provided — wait for the user to ask something)");

    match name.as_str() {
        "claude" => AgentSpec {
            name,
            bin,
            // Claude Code accepts an additional system prompt via a flag.
            // The babysit manual goes there so it doesn't clutter the chat.
            args: vec!["--append-system-prompt".into(), manual.clone()],
            initial_message: user_msg.to_string(),
        },
        "codex" => AgentSpec {
            // Codex CLI does not have a stable system-prompt flag we can rely
            // on, so we send everything as the first user message. If/when
            // codex grows a flag we can prefer it.
            name,
            bin,
            args: vec![],
            initial_message: combined_message(&manual, user_msg),
        },
        _ => AgentSpec {
            name,
            bin,
            args: vec![],
            initial_message: combined_message(&manual, user_msg),
        },
    }
}

fn combined_message(manual: &str, user_msg: &str) -> String {
    format!(
        "{manual}\n\n--- USER INSTRUCTION ---\n{user_msg}\n",
    )
}

/// The babysit "manual" — explains the subcommand API and that the session id
/// is implicit. Kept short so it doesn't dominate the agent's context.
fn babysit_manual(session_id: &str) -> String {
    format!(
        "You are running inside a babysit session. The user has launched a \
         shell command and asked you to operate on it. The command runs in \
         a separate tab; you can observe and control it via these shell \
         commands:\n\
         \n\
           babysit status         # JSON state of the wrapped command (running / exited / exit_code)\n\
           babysit log --tail 200 # last 200 lines of the wrapped command's output\n\
           babysit log            # full output\n\
           babysit restart        # restart the wrapped command\n\
           babysit kill           # terminate it\n\
           babysit send \"text\"  # write text + newline to its stdin\n\
         \n\
         The session id is fixed in $BABYSIT_SESSION_ID = {session_id}, so \
         you can omit the --session flag. Run those commands using your \
         shell tool whenever the user's instruction calls for it. Do not \
         poll continuously without reason; check the state when relevant \
         and otherwise wait. When you act on the user's behalf, briefly \
         say what you did."
    )
}
