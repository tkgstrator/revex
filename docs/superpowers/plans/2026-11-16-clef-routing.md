# Clef Smart Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `/route smart` send suitable low-refusal tasks to Codex after one Clef decision, while retaining Revex for other tasks.

**Architecture:** `src/main.rs` owns the decision HTTP adapter, score gate, route selection, and persistence. The existing `Agents::invoke` runs selected Codex and records the child. Interactive UI settings and install work have separate owners.

**Tech Stack:** Rust 2024, `ureq`, `serde_json`, existing fake HTTP tests and PTY harness.

**Spec:** `docs/superpowers/specs/2026-11-16-clef-routing-design.md`

## Global Constraints

- Keep `auto`, `off`, `local`, and `remote` behavior intact; explicit Codex/Claude instructions run first.
- Use the configured oMLX base URL/key with a separate `decision_model`: `clef` (default), `clef-flash`, or `self` (the selected chat model).
- Forward only when Codex fit is at least `0.80` and refusal risk is at most `0.10`.
- A classifier failure or unavailable Codex stays local; a Codex failure after launch is reported without local retry.
- Keep user history, cancellation, and helper records accurate. Do not expose credentials or full task text in diagnostics.

## Review Focus

- An invalid probability must select Revex, never Codex (Task 1 test).
- A slow first Clef load must not be mistaken for a warm timeout (Task 1 test).
- A direct Codex instruction must bypass Clef even in smart mode (Task 2 test).
- A disabled Codex helper must select Revex for an automatic decision (Task 2 test).
- A started Codex CLI failure must not repeat the task locally (Task 2 test).

---

### Task 1: Decision adapter and score gate

**Files:** Modify `src/main.rs` tests and routing helpers.

**Interfaces:** `smart_decision(cfg: &Cfg, task: &str) -> Result<(DecisionScores, Option<Usage>), DecisionError>` posts the verified predicate request to `{base}/decisions`; `should_use_codex(scores: DecisionScores) -> bool` applies the conservative gate.

- [ ] Add a failing fake-server test for the exact `model`, `input`, and two questions, plus parsing valid and malformed scores.
- [ ] Run the focused test and confirm the expected failure.
- [ ] Implement the HTTP adapter, score validation, bounded connection and first-load-tolerant total timeout, and gate.
- [ ] Run the focused test and confirm it passes.

### Task 2: Smart route integration

**Files:** Modify `src/main.rs` and focused tests; coordinate `src/interactive.rs` with its owner; update README routing/config sections.

**Interfaces:** Add `Route::Smart` and `Cfg.decision_model: String`; `parent_turn` calls Clef after explicit-helper dispatch and before existing frontier routing. Reuse `Agents::invoke(Provider::Codex)` for a passing score.

- [ ] Add failing tests for route precedence, local fallback, successful Codex result, failed Codex without local retry, history, and cancellation boundaries.
- [ ] Run the focused tests and confirm expected failures.
- [ ] Implement smart selection and CLI/config/help wiring; preserve existing routes.
- [ ] Run focused and full Rust tests, Clippy, formatting, and PTY checks with fake endpoint and CLI. Report any work outside owned files to the integration owner.
