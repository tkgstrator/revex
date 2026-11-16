# Interactive agents for Revex

## Intent

Make Revex usable as a terminal coding assistant: the user enters a natural-language task, sees what the agent is doing, approves edits, and can inspect automatically spawned agents with `/agents` while work continues. Revex remains a local-model client and a single binary. The existing one-shot and MCP modes remain usable.

## User-visible behavior

- Starting `revex` in a terminal opens an interactive session for the current directory and resumes its saved conversation.
- Every interactive start shows a compact Revex welcome screen with the working directory, model/API setting, history state and command hints. If no model API URL is configured, setup appears within that screen: Revex asks for the OpenAI-compatible API URL and optional key/model settings, saves them privately, then opens the session. A returning user can edit the saved connection settings from the welcome screen. Non-interactive modes report the missing configuration without consuming task input.
- A natural-language task runs in the background so the prompt can still accept `/agents` and `/status`. A second natural-language task waits until the first finishes. Output identifies the agent producing it.
- The parent model may use `>> spawn TASK` for independent investigation and `>> wait ID [ID ...]` to collect results. Spawn returns an ID immediately. Children have separate model histories, cannot spawn children, and may only use read-only file/search tools. The parent owns edits and final conclusions.
- `/agents` lists parent and children with ID, status, short task, model, reported input/output tokens and whether usage is complete. It works while the parent or children run. Token totals include every model request in that agent, not just final answers.
- The UI shows tool activity and exit status. Before a parent edit, it shows the target and a bounded change preview, then asks for approval. No worker reads standard input directly.
- `/exit` and Ctrl+C do not leave misleading running states. After restart, previously running jobs are shown as interrupted; they are never silently rerun.
- `/clear` removes parent and helper history, statuses and usage when no helper is active. With an active helper it refuses clearly. An interactive shell command is stopped and reaped before exit; an edit is refused if its target changed during approval.

## Constraints and boundaries

- Keep the `>>` text protocol and compact system prompt. Add only the two delegation verbs and any read-only search verb children need. Do not add a second orchestration server, database, or TUI framework.
- Limit simultaneous children to two. Further children may queue; status must distinguish queued from running. Prevent duplicate result insertion when `wait` is repeated.
- The CLI owns all input and approval decisions. Worker threads communicate through channels. Tool and history operations use an explicit per-agent working directory; no process-global `set_current_dir` during parallel work.
- Record usage only from provider-reported SSE data. Chat Completions may provide a final usage chunk; Responses includes usage in `response.completed`. Show unavailable or partial when it is missing or a stream fails. Do not estimate tokens from characters.
- Keep history files private (directory 0700, file 0600 on Unix) and atomically written. Parent history and child records must survive restart. A running job on restart becomes interrupted. Corrupt state is reported rather than overwritten.
- Child agents cannot write or execute arbitrary shell commands. A dedicated search command may use `rg` with arguments passed without a shell. Parent shell and edits retain interactive approval.
- Existing `REVEX_FRONTIER` routing is separate from child agents. One-shot, piped and MCP modes remain synchronous and keep their documented behavior.

## Completion evidence

- Focused tests cover spawn/wait, two concurrent children and queueing, `/agents` responsiveness during a blocked model stream, approval routing, read-only child enforcement, usage present/missing/partial for both APIs, persistence/restart, and existing modes.
- An end-to-end test using a local fake OpenAI-compatible SSE server demonstrates a natural-language parent task, automatic child spawn, live `/agents`, result collection, edit approval, and token display. No real model is configured in this workspace, so actual model decision quality is outside this verification.
- First-run and returning-session tests cover the welcome screen, setup, private saved settings, restart without a second setup prompt, and non-interactive failure when the URL is absent.
