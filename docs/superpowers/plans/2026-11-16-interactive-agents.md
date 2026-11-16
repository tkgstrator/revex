# Interactive Agents Implementation Plan

> **For agentic workers:** Implement the tasks in order, with focused verification at each step.

**Goal:** Deliver the behavior in the linked design without replacing Revex's local-model tool protocol.

**Architecture:** Use standard threads and channels for the UI coordinator and model workers. Give each agent an explicit cwd, independent history and usage record. Keep one binary; split the now-large Rust source by responsibility if that reduces integration risk.

**Tech Stack:** Rust 2024, serde_json, ureq, standard library; add a dependency only if cancellation or terminal input demonstrably needs it.

**Spec:** [Interactive agents design](../specs/2026-11-16-interactive-agents-design.md)

## Global constraints

- Main implementation workers own code. The coordinator reviews diffs and runs native verification.
- Preserve one-shot, piped, MCP and existing saved-history behavior.
- No estimated token counts or unapproved child writes.
- Do not commit until the coordinator has checked the actual diff and tests.

## Review focus

- A child cannot reach `sh`, `write`, or `replace` through unknown-verb fallback.
- A blocked parent request cannot block `/agents` input.
- An approval response cannot be consumed by another agent.
- Missing usage is not displayed as zero or a complete total.
- Restart does not automatically re-execute interrupted work.

## Tasks

### 1. Provider results and token usage

- Add a result type containing model text and optional reported input/output usage.
- Parse Chat Completions final usage chunks and Responses completion usage; retain usage after the text stop marker.
- Add fixed-SSE tests for reported, missing and interrupted usage.
- Run formatting, Clippy and tests.

### 2. Agent manager and delegation

- Add parent-only `spawn` and `wait` parsing and a bounded child registry with separate histories and statuses.
- Execute child reads/searches without shell or writes; return child results to the parent once.
- Track per-agent usage and save child metadata/results privately for restart.
- Test concurrent children, queueing, failure, repeat wait and restart.

### 3. Interactive coordinator

- Move stdin and approvals to one UI coordinator; run the parent turn off the input thread.
- Implement live `/agents`, progress and tool-result summaries, edit previews and clear approval prompts.
- Ensure `/exit` and interruption leave correct statuses and saved state.
- Test live command responsiveness and approval routing using a local fake SSE server.

### 4. Integration and documentation

- Remove process-global cwd changes from concurrent paths and use explicit cwd for tools.
- Update the short system prompt, README usage and limitations.
- Run cargo fmt, Clippy, tests, build, and an end-to-end CLI/MCP smoke test.
- Inspect the exact intended diff, obtain one independent final review if the coordinated change will be committed, then commit after focused fixes and verification.

### 5. First-run setup requested during trial

- Show a compact welcome screen with project, model/API, history and command hints on every interactive launch.
- Ask for the model API URL when an interactive launch has none, save the settings privately, and continue into the REPL.
- Preserve clear missing-configuration errors in one-shot, piped and MCP modes; never consume task input as setup answers.
- Test setup and restart, then build `target/release/revex` for the user's trial.
