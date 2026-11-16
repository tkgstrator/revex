# Terminal TUI implementation plan

**Goal:** Replace the interactive print-and-read interface with the persistent TUI in the linked [design](../specs/2026-11-16-terminal-tui-design.md).

**Code ownership:** Implementation workers own Rust code and tests. The main agent owns coordination, visual inspection, native verification, docs and the final diff.

## 1. Terminal foundation

- Add scoped `ratatui` and `crossterm` dependencies and a `src/tui.rs` renderer/state reducer.
- Enter raw mode and alternate screen only for an actual interactive TTY. Restore on every exit path, including panic and signals.
- Draw a compact title/workspace area, scrollable transcript and prominent editor with Revex's own visual identity. Style user messages, space assistant replies, highlight fenced code and approval previews, and show active model work as a status indicator. Handle resize and small terminals.

## 2. Setup and workspace

- Replace startup `ask_terminal`/`start >` with a launch-directory confirmation and keyboard-driven provider, URL, key and model screens in the TUI. Include an oMLX preset at `http://127.0.0.1:8000/v1`.
- Reuse private settings writes; query model IDs with a bounded timeout; permit manual model entry and connection retry.
- Add a multi-select helper-provider choice in setup and `/config`, stored only in private user settings. Support local, Codex CLI and Claude Code CLI; launch external helpers with the authorized permission modes and report the actual provider in `/agents`.
- Confirm each canonical launch directory once for both tool cwd and history identity, including when launched from `target/release`; remember acceptance privately and skip the question on later launches from that workspace. Declining exits with guidance to restart in the desired directory.

## 3. Live session controls

- Route keyboard events and worker events through one coordinator. Render `/agents`, `/status`, `/config` and approval overlays without blocking model work. Make `/model` select a model and optional reasoning effort, then persist and send that effort using each API's supported request shape.
- Keep approval input fresh and isolated. Move direct stderr output into the transcript or an explicitly restored fallback.
- Retain shell process cancellation, saved state and existing one-shot/piped/MCP behavior.
- Persist the latest 100 sent inputs per workspace, including across `/clear`, and recall them with Up/Down while retaining an unfinished draft. Answer simple model-identity questions directly from effective configuration, and add model/effort metadata to other requests so the model can report configured values accurately without inspecting the environment.

## 4. Verification and delivery

- Add reducer/render tests and focused PTY fake-SSE tests for setup, main screen, live agents, approval, resize, exit and terminal restoration.
- Run formatting, Clippy, tests, release build, CLI/MCP smoke and visual inspection of actual terminal frames.
- Inspect owned diff, request one independent review if this is a large commit, fix material findings, and commit with CommitGuard. Preserve unrelated work and do not push without a request.
