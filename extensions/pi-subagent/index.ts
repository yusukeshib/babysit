/**
 * pi-subagent: run pi subagents under babysit.
 *
 * Every subagent is a `pi --mode json -p` process wrapped in a babysit PTY
 * session. Because babysit supervises it in the background, spawning is
 * NON-BLOCKING: the tool returns a session id immediately and the parent agent
 * keeps working. The `--mode json` event stream is recorded by the worker, so
 * subagent_check can show live progress (turns, tool calls, partial answer) and
 * subagent_wait extracts the final answer; a human can `attach` to the live
 * buffer in a tmux split and take over.
 *
 * Tools (LLM):  subagent_run, subagent_check, subagent_send, subagent_wait, subagent_kill
 * Commands:     /subagents (arrow-key picker: attach/inspect)
 * Widget:       live list of running subagents (poll-driven)
 */

import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { getMarkdownTheme } from "@earendil-works/pi-coding-agent";
import { Box, Markdown, Text } from "@earendil-works/pi-tui";
import { Type } from "typebox";
import { StringEnum } from "@earendil-works/pi-ai";
import { type AgentConfig, type AgentScope, discoverAgents } from "./agents";

// Dedicated babysit state root so subagents never collide with the user's own
// manual `babysit` sessions.
const SUBAGENT_ROOT =
	process.env.PI_SUBAGENT_DIR ?? path.join(os.homedir(), ".pi-subagents");
const PI_BIN = process.env.PI_SUBAGENT_BIN ?? "pi";
const BABYSIT_BIN = process.env.PI_SUBAGENT_BABYSIT ?? "babysit";
const POLL_MS = 2500;

interface BsSession {
	id: string;
	state: string; // "running" | "exited" | "dead" ...
	exit_code?: number | null;
	note?: string | null;
	output_bytes?: number;
	screen_seq?: number | null;
}

// ---------------------------------------------------------------------------
// babysit CLI helpers
// ---------------------------------------------------------------------------

function bs(args: string[]): { stdout: string; stderr: string; code: number } {
	const r = spawnSync(BABYSIT_BIN, args, {
		encoding: "utf-8",
		env: { ...process.env, BABYSIT_DIR: SUBAGENT_ROOT },
		maxBuffer: 32 * 1024 * 1024,
	});
	return {
		stdout: r.stdout ?? "",
		stderr: r.stderr ?? "",
		code: r.status ?? 1,
	};
}

function listSessions(): BsSession[] {
	const r = bs(["list", "--json"]);
	if (r.code !== 0) return [];
	try {
		const parsed = JSON.parse(r.stdout);
		return Array.isArray(parsed) ? parsed : (parsed.sessions ?? []);
	} catch {
		return [];
	}
}

function statusOf(id: string): BsSession | null {
	// `status --json` shape: { session, status: { state, exit_code, ... } }.
	// `note` lives only in `list --json`, so fold it in from there.
	const r = bs(["status", "-s", id, "--json"]);
	if (r.code !== 0) return null;
	try {
		const parsed = JSON.parse(r.stdout);
		const inner = parsed.status ?? parsed;
		const note = listSessions().find((s) => s.id === id)?.note ?? null;
		return { id: parsed.session ?? id, note, ...inner };
	} catch {
		return null;
	}
}

// ---------------------------------------------------------------------------
// parse the subagent's `--mode json` event stream (from its babysit log)
// ---------------------------------------------------------------------------

interface ToolCall {
	name: string;
	summary: string;
}
interface Progress {
	turns: number;
	toolCalls: ToolCall[];
	finalText: string;
	tokens?: number;
	cost?: number;
	errorMsg?: string;
}

function summarizeToolCall(name: string, args: Record<string, unknown>): string {
	const s = (v: unknown, n = 60) => {
		const str = String(v ?? "");
		return str.length > n ? `${str.slice(0, n - 1)}\u2026` : str;
	};
	switch (name) {
		case "bash":
			return `$ ${s(args.command)}`;
		case "read":
			return `read ${s(args.file_path ?? args.path)}`;
		case "write":
			return `write ${s(args.file_path ?? args.path)}`;
		case "edit":
			return `edit ${s(args.file_path ?? args.path)}`;
		case "grep":
			return `grep /${s(args.pattern, 40)}/`;
		case "find":
			return `find ${s(args.pattern ?? args.path, 40)}`;
		case "ls":
			return `ls ${s(args.path)}`;
		default:
			return `${name} ${s(JSON.stringify(args), 40)}`;
	}
}

function parseEvents(logText: string): Progress {
	const p: Progress = { turns: 0, toolCalls: [], finalText: "" };
	for (const raw of logText.split("\n")) {
		const line = raw.replace(/\r$/, "").trim();
		if (!line.startsWith("{")) continue;
		let ev: Record<string, unknown>;
		try {
			ev = JSON.parse(line);
		} catch {
			continue; // partial trailing line, etc.
		}
		switch (ev.type) {
			case "turn_start":
				p.turns++;
				break;
			case "tool_execution_start": {
				const name = String(ev.toolName ?? "tool");
				p.toolCalls.push({
					name,
					summary: summarizeToolCall(name, (ev.args as Record<string, unknown>) ?? {}),
				});
				break;
			}
			case "message_end": {
				const msg = ev.message as
					| { role?: string; content?: { type: string; text?: string }[]; usage?: { totalTokens?: number; cost?: { total?: number } } }
					| undefined;
				if (msg?.role === "assistant") {
					const txt = (msg.content ?? [])
						.filter((c) => c.type === "text" && c.text)
						.map((c) => c.text)
						.join("");
					if (txt.trim()) p.finalText = txt; // keep the latest non-empty assistant text
					if (msg.usage) {
						p.tokens = msg.usage.totalTokens;
						p.cost = msg.usage.cost?.total;
					}
				}
				break;
			}
			case "error":
				p.errorMsg = String(ev.message ?? ev.error ?? line);
				break;
		}
	}
	return p;
}

// ---------------------------------------------------------------------------
// spawning
// ---------------------------------------------------------------------------

function writePromptTempFile(agentName: string, prompt: string): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-subagent-"));
	const safe = agentName.replace(/[^\w.-]+/g, "_");
	const file = path.join(dir, `prompt-${safe}.md`);
	fs.writeFileSync(file, prompt, "utf-8");
	return file;
}

interface RunOpts {
	agent?: AgentConfig;
	task: string;
	model?: string;
	tools?: string[];
	cwd: string;
	// Idle-timeout is OFF by default: a text-mode `pi -p` is silent while it
	// works (it only prints the final answer), so idle detection would false-kill
	// a busy subagent. An absolute timeout is the safety valve instead.
	idleTimeout?: string;
	timeout: string;
}

function spawnSubagent(opts: RunOpts): { id: string } | { error: string } {
	// Build the inner `pi` command. `--mode json` streams one event per line
	// (tool calls, messages) so the log shows live progress and never looks
	// falsely idle while the subagent is working.
	const piArgs: string[] = ["--mode", "json", "-p", "--no-session"];
	const model = opts.model ?? opts.agent?.model;
	if (model) piArgs.push("--model", model);
	const tools = opts.tools ?? opts.agent?.tools;
	if (tools && tools.length > 0) piArgs.push("--tools", tools.join(","));
	if (opts.agent?.systemPrompt?.trim()) {
		const f = writePromptTempFile(opts.agent.name, opts.agent.systemPrompt);
		piArgs.push("--append-system-prompt", f);
	}
	piArgs.push(`Task: ${opts.task}`);

	// babysit run -d --json --timeout DUR [--idle-timeout DUR] -- pi ...
	// A real PTY is used (NOT --no-tty): pi -p hangs with plain pipes, and a PTY
	// lets a human `attach` and fully take over the subagent.
	const bsArgs = [
		"run",
		"-d",
		"--json",
		"--size",
		"120x40",
		"--timeout",
		opts.timeout,
	];
	// Only enable idle-timeout when the caller explicitly asks for it (see note
	// on RunOpts.idleTimeout).
	if (opts.idleTimeout && opts.idleTimeout !== "none") {
		bsArgs.push("--idle-timeout", opts.idleTimeout);
	}
	bsArgs.push("--", PI_BIN, ...piArgs);

	const r = spawnSync(BABYSIT_BIN, bsArgs, {
		encoding: "utf-8",
		cwd: opts.cwd,
		env: { ...process.env, BABYSIT_DIR: SUBAGENT_ROOT },
	});
	if ((r.status ?? 1) !== 0) {
		return { error: r.stderr || r.stdout || "babysit run failed" };
	}
	try {
		const { id } = JSON.parse(r.stdout);
		return { id };
	} catch {
		return { error: `could not parse id from: ${r.stdout}` };
	}
}

// ---------------------------------------------------------------------------
// widget (live status of running subagents)
// ---------------------------------------------------------------------------

function renderWidgetLines(sessions: BsSession[]): string[] {
	const active = sessions.filter((s) => s.state === "running");
	if (active.length === 0) return [];
	const lines = [`⚙ subagents (${active.length} running)`];
	for (const s of active) {
		const flag = s.note ? ` ⚑ ${s.note}` : "";
		lines.push(`  ⏳ ${s.id}${flag}`);
	}
	lines.push("  /subagents to attach/inspect");
	return lines;
}

// ---------------------------------------------------------------------------
// tmux attach (open the live buffer in a split; human can take over)
// ---------------------------------------------------------------------------

function attachInTmux(id: string): { ok: boolean; msg: string } {
	if (!process.env.TMUX) {
		return {
			ok: false,
			msg: `Not inside tmux. Attach manually:\n  BABYSIT_DIR=${SUBAGENT_ROOT} ${BABYSIT_BIN} attach -s ${id}`,
		};
	}
	const cmd = `BABYSIT_DIR=${SUBAGENT_ROOT} ${BABYSIT_BIN} attach -s ${id}`;
	const r = spawnSync("tmux", ["split-window", "-h", cmd], { encoding: "utf-8" });
	if ((r.status ?? 1) !== 0) {
		return { ok: false, msg: r.stderr || "tmux split-window failed" };
	}
	return { ok: true, msg: `Attached to ${id} in a tmux split (detach: Ctrl-\\ Ctrl-\\).` };
}

// ---------------------------------------------------------------------------
// extension
// ---------------------------------------------------------------------------

export default function (pi: ExtensionAPI) {
	let pollTimer: ReturnType<typeof setInterval> | undefined;

	const refreshWidget = (ctx: ExtensionContext) => {
		if (!ctx.hasUI) return;
		const lines = renderWidgetLines(listSessions());
		if (lines.length > 0) ctx.ui.setWidget("pi-subagent", lines);
		else ctx.ui.setWidget("pi-subagent", []);
	};

	// Render a subagent's final answer INLINE in the transcript as formatted
	// markdown (agent output is markdown). Fed via pi.sendMessage below.
	pi.registerMessageRenderer("pi-subagent-result", (message, _opts, theme) => {
		const d = (message.details ?? {}) as { title?: string; body?: string };
		const body =
			d.body ?? (typeof message.content === "string" ? message.content : "");
		const box = new Box(1, 0, (t) => theme.bg("customMessageBg", t));
		if (d.title) box.addChild(new Text(theme.fg("accent", d.title), 0, 0));
		box.addChild(new Markdown(body, 0, 0, getMarkdownTheme()));
		return box;
	});

	pi.on("session_start", async (_event, ctx) => {
		if (pollTimer) clearInterval(pollTimer);
		pollTimer = setInterval(() => {
			try {
				refreshWidget(ctx);
			} catch {
				/* ignore poll errors */
			}
		}, POLL_MS);
	});

	pi.on("session_shutdown", async () => {
		if (pollTimer) clearInterval(pollTimer);
		pollTimer = undefined;
	});

	// ----- subagent_run -----------------------------------------------------
	pi.registerTool({
		name: "subagent_run",
		label: "Run subagent",
		description:
			"Spawn a pi subagent in the background under babysit and return immediately " +
			"(NON-BLOCKING). Use for delegating a self-contained task while you keep working. " +
			"Returns a session id; poll it with subagent_check, steer with subagent_send, " +
			"block on it with subagent_wait, or stop it with subagent_kill.",
		promptSnippet:
			"Delegate a task to a background pi subagent (non-blocking); returns a session id",
		promptGuidelines: [
			"When a task is self-contained and can run on its own (codebase recon, a long build/test run, a parallelizable subtask, or work that would otherwise pollute your context), delegate it with subagent_run instead of doing it inline.",
			"subagent_run is non-blocking: it returns a session id immediately, so launch subagents for independent work and keep making progress on the main task in parallel.",
			"Launch multiple subagents with several subagent_run calls when subtasks are independent; they run concurrently under babysit.",
			"After subagent_run, do not idle-wait: continue other work, poll with subagent_check, and only call subagent_wait when you actually need a subagent's result to proceed.",
			"If a subagent asks a question or gets stuck, steer it with subagent_send; kill runaway or no-longer-needed subagents with subagent_kill.",
		],
		parameters: Type.Object({
			task: Type.String({ description: "The task for the subagent to perform" }),
			agent: Type.Optional(
				Type.String({ description: "Named agent definition to use (see ~/.pi/agent/agents). Optional." }),
			),
			model: Type.Optional(Type.String({ description: "Model override, e.g. 'sonnet'." })),
			tools: Type.Optional(
				Type.Array(Type.String(), { description: "Tool allowlist for the subagent." }),
			),
			agentScope: Type.Optional(
				StringEnum(["user", "project", "both"] as const, {
					description: "Where to discover named agents. Default 'user'.",
				}),
			),
			timeout: Type.Optional(
				Type.String({
					description:
						"Absolute auto-kill after this long (e.g. 30m). Default 15m. Use 'none' to disable.",
				}),
			),
			idleTimeout: Type.Optional(
				Type.String({
					description:
						"Auto-kill after NO output for this long (e.g. 90s). Off by default — a text-mode subagent is silent while working, so this would false-kill it. Only set it if you know the subagent streams output.",
				}),
			),
		}),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			let agent: AgentConfig | undefined;
			if (params.agent) {
				const scope = (params.agentScope ?? "user") as AgentScope;
				const { agents } = discoverAgents(ctx.cwd, scope);
				agent = agents.find((a) => a.name === params.agent);
				if (!agent) {
					const avail = agents.map((a) => a.name).join(", ") || "none";
					return {
						content: [
							{ type: "text", text: `Unknown agent "${params.agent}". Available: ${avail}.` },
						],
						isError: true,
						details: {},
					};
				}
			}

			const res = spawnSubagent({
				agent,
				task: params.task,
				model: params.model,
				tools: params.tools,
				cwd: ctx.cwd,
				timeout: params.timeout ?? "15m",
				idleTimeout: params.idleTimeout,
			});

			if ("error" in res) {
				return {
					content: [{ type: "text", text: `Failed to spawn subagent: ${res.error}` }],
					isError: true,
					details: {},
				};
			}

			refreshWidget(ctx);
			return {
				content: [
					{
						type: "text",
						text:
							`Subagent started (id: ${res.id})${agent ? ` [agent: ${agent.name}]` : ""}.\n` +
							`Running in the background — you can keep working.\n` +
							`Poll:  subagent_check { id: "${res.id}" }\n` +
							`Wait:  subagent_wait  { id: "${res.id}" }\n` +
							`Human can watch/steer: /subagents (pick ${res.id})`,
					},
				],
				details: { id: res.id, agent: agent?.name, task: params.task },
			};
		},
	});

	// ----- subagent_check ---------------------------------------------------
	pi.registerTool({
		name: "subagent_check",
		label: "Check subagent",
		description:
			"Inspect subagent(s). With an id: returns a live progress summary — state, " +
			"turns so far, recent tool calls, and any final answer. Without an id: lists " +
			"all subagents. Cheap to poll while a subagent runs.",
		promptSnippet: "Check status and live progress of background subagents",
		parameters: Type.Object({
			id: Type.Optional(Type.String({ description: "Session id. Omit to list all subagents." })),
			tools: Type.Optional(
				Type.Number({ description: "How many recent tool calls to show (default 8)." }),
			),
		}),
		async execute(_id, params) {
			if (!params.id) {
				const sessions = listSessions();
				if (sessions.length === 0) {
					return { content: [{ type: "text", text: "No subagents." }], details: {} };
				}
				const lines = sessions.map((s) => {
					const flag = s.note ? ` ⚑ ${s.note}` : "";
					const ec = s.exit_code != null ? ` exit=${s.exit_code}` : "";
					return `${s.id}  ${s.state}${ec}${flag}`;
				});
				return { content: [{ type: "text", text: lines.join("\n") }], details: { sessions } };
			}

			const st = statusOf(params.id);
			if (!st) {
				return {
					content: [{ type: "text", text: `No such subagent: ${params.id}` }],
					isError: true,
					details: {},
				};
			}

			const prog = parseEvents(bs(["log", "-s", params.id]).stdout);
			const nTools = params.tools ?? 8;
			const recent = prog.toolCalls.slice(-nTools);

			const parts: string[] = [];
			let header = `state=${st.state}`;
			if (st.exit_code != null) header += ` exit_code=${st.exit_code}`;
			header += ` turns=${prog.turns} tools=${prog.toolCalls.length}`;
			if (prog.tokens != null) header += ` ctx=${prog.tokens}`;
			if (prog.cost != null) header += ` $${prog.cost.toFixed(4)}`;
			if (st.note) header += ` ⚑ ${st.note}`;
			parts.push(header);

			if (prog.errorMsg) parts.push(`⚠ error: ${prog.errorMsg}`);

			if (recent.length > 0) {
				const skipped = prog.toolCalls.length - recent.length;
				parts.push(
					`--- recent tool calls${skipped > 0 ? ` (+${skipped} earlier)` : ""} ---\n` +
						recent.map((t) => `  ${t.summary}`).join("\n"),
				);
			}

			if (prog.finalText.trim()) {
				parts.push(`--- answer so far ---\n${prog.finalText.trim()}`);
			} else if (prog.toolCalls.length === 0) {
				parts.push("(starting up… no events yet)");
			} else {
				parts.push("(working… no answer text yet)");
			}

			return {
				content: [{ type: "text", text: parts.join("\n") }],
				details: { status: st, progress: prog },
			};
		},
	});

	// ----- subagent_send ----------------------------------------------------
	pi.registerTool({
		name: "subagent_send",
		label: "Send to subagent",
		description:
			"Send a line of text to a running subagent's stdin (e.g. to answer a prompt or steer it).",
		promptSnippet: "Send input to a running subagent to steer or answer it",
		parameters: Type.Object({
			id: Type.String({ description: "Session id." }),
			text: Type.String({ description: "Text to send (a newline is appended)." }),
		}),
		async execute(_id, params) {
			const r = bs(["send", "-s", params.id, params.text]);
			if (r.code !== 0) {
				return {
					content: [{ type: "text", text: r.stderr || "send failed" }],
					isError: true,
					details: {},
				};
			}
			return { content: [{ type: "text", text: `Sent to ${params.id}.` }], details: {} };
		},
	});

	// ----- subagent_wait ----------------------------------------------------
	pi.registerTool({
		name: "subagent_wait",
		label: "Wait for subagent",
		description:
			"Block until a subagent exits (or the timeout elapses), then return its exit code " +
			"and final output. Use when you need the result before continuing.",
		promptSnippet: "Block until a subagent finishes and return its result",
		parameters: Type.Object({
			id: Type.String({ description: "Session id." }),
			timeout: Type.Optional(
				Type.String({ description: "Give up after this long (e.g. 5m). Default: wait indefinitely." }),
			),
		}),
		async execute(_id, params) {
			const waitArgs = ["wait", "-s", params.id];
			if (params.timeout) waitArgs.push("--timeout", params.timeout);
			const w = bs(waitArgs);
			const timedOut = w.code === 124;
			const st = statusOf(params.id);
			const prog = parseEvents(bs(["log", "-s", params.id]).stdout);

			if (timedOut) {
				return {
					content: [
						{
							type: "text",
							text:
								`⏱ wait timed out; subagent ${params.id} still ${st?.state ?? "?"} ` +
								`(turns=${prog.turns}, tools=${prog.toolCalls.length}).`,
						},
					],
					isError: true,
					details: { status: st, progress: prog, timedOut: true },
				};
			}

			const exit = st?.exit_code ?? w.code;
			const ok = exit === 0;
			const stats = `turns=${prog.turns} tools=${prog.toolCalls.length}` +
				(prog.tokens != null ? ` ctx=${prog.tokens}` : "") +
				(prog.cost != null ? ` $${prog.cost.toFixed(4)}` : "");
			const body =
				prog.finalText.trim() ||
				prog.errorMsg ||
				// Fallback: no parsed answer (crash before any assistant text).
				bs(["log", "-s", params.id, "--tail", "40"]).stdout.trim() ||
				"(no output)";
			return {
				content: [
					{
						type: "text",
						text: `Subagent ${params.id} exited (exit_code=${exit}, ${stats}).\n\n${body}`,
					},
				],
				isError: !ok,
				details: { status: st, progress: prog },
			};
		},
	});

	// ----- subagent_kill ----------------------------------------------------
	pi.registerTool({
		name: "subagent_kill",
		label: "Kill subagent",
		description: "Terminate a running subagent.",
		promptSnippet: "Terminate a running subagent",
		parameters: Type.Object({ id: Type.String({ description: "Session id." }) }),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			const r = bs(["kill", "-s", params.id, "--json"]);
			refreshWidget(ctx);
			if (r.code !== 0) {
				return {
					content: [{ type: "text", text: r.stderr || "kill failed" }],
					isError: true,
					details: {},
				};
			}
			return { content: [{ type: "text", text: `Killed ${params.id}.` }], details: {} };
		},
	});

	// The live widget already shows running subagents, so there is no manual
	// list command — the only human-facing command is attach (observe/steer).

	// ----- /subagents -------------------------------------------------------
	// Arrow up/down picker over the subagent list (like /stash). Selecting a
	// running subagent attaches to its live buffer in a tmux split; selecting a
	// finished one shows its parsed final answer (not the raw JSON event log).
	pi.registerCommand("subagents", {
		description: "Pick a subagent from the list (↑/↓) to attach or inspect",
		handler: async (_args, ctx) => {
			const sessions = listSessions().sort((a, b) =>
				a.state === b.state ? 0 : a.state === "running" ? -1 : 1,
			);
			if (sessions.length === 0) {
				ctx.ui.notify("No subagents.", "info");
				return;
			}

			const taskOf = (s: BsSession): string => {
				const cmd = (s as { cmd?: string[] }).cmd;
				if (!Array.isArray(cmd) || cmd.length === 0) return "";
				const last = cmd[cmd.length - 1];
				return last.replace(/^Task:\s*/, "").replace(/\s+/g, " ").trim();
			};

			// Labels must be unique for index mapping; the id makes them unique.
			const labels = sessions.map((s) => {
				const icon = s.state === "running" ? "⏳" : s.exit_code === 0 ? "✓" : "✗";
				const ec = s.exit_code != null ? ` exit=${s.exit_code}` : "";
				const flag = s.note ? " ⚑" : "";
				const task = taskOf(s);
				const preview = task.length > 60 ? `${task.slice(0, 57)}…` : task;
				return `${icon} ${s.id}${flag}  ${s.state}${ec}${preview ? `  — ${preview}` : ""}`;
			});

			const choice = await ctx.ui.select("Subagents:", labels);
			if (!choice) return;
			const picked = sessions[labels.indexOf(choice)];
			if (!picked) return;

			if (picked.state === "running") {
				const res = attachInTmux(picked.id);
				ctx.ui.notify(res.msg, res.ok ? "info" : "warn");
			} else {
				// Parse the --mode json event stream and show the final answer,
				// not the raw JSONL log.
				const prog = parseEvents(bs(["log", "-s", picked.id]).stdout);
				const stats =
					`turns=${prog.turns} tools=${prog.toolCalls.length}` +
					(prog.tokens != null ? ` ctx=${prog.tokens}` : "") +
					(prog.cost != null ? ` $${prog.cost.toFixed(4)}` : "");
				const body =
					prog.finalText.trim() ||
					prog.errorMsg ||
					bs(["log", "-s", picked.id, "--tail", "20"]).stdout.trim() ||
					"(no output)";
				const title =
					`${picked.id} ${picked.state}` +
					(picked.exit_code != null ? ` (exit=${picked.exit_code})` : "") +
					`  ${stats}`;
				// Agent output is markdown — render it INLINE in the transcript
				// (formatted, normal colors) via the registered message renderer,
				// not a float overlay. Falls back to notify outside TUI.
				if (ctx.hasUI) {
					pi.sendMessage({
						customType: "pi-subagent-result",
						content: title,
						display: true,
						details: { title, body },
					});
				} else {
					ctx.ui.notify(`${title}\n\n${body}`, "info");
				}
			}
		},
	});
}
