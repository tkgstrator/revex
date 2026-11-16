# Clef routing for Revex

## Intent

Add an opt-in, fast decision before ordinary Revex turns: use Clef on the configured oMLX endpoint to judge whether Codex is a good fit and whether Codex is likely to refuse the task. Keep likely refusals and ambiguous work in Revex. This is a routing proposal, not a safety guarantee or an implementation plan.

## Approaches

| Approach | Tradeoff |
| --- | --- |
| Keep `/route auto` only | No new integration, but the current local chat request generates an `F` or `L` and does not expose separate fit and refusal estimates. |
| Add `/route smart` with a conservative Clef gate **(recommended)** | A warm decision took about 222–246 ms in the current oMLX probe; it needs a new endpoint adapter, calibrated thresholds, and an explicit cold-load path. |
| Forward on Codex fit alone, then try Revex after refusal | Simpler decision, but high-refusal tasks may be sent to Codex and a retry could repeat work after an uncertain CLI result. |

The approved direction is the second approach. `/route auto`, `off`, `local`, and `remote` retain their current behavior. `/route smart` is available through the existing route setting (`/route smart`, `REVEX_ROUTE=smart`, or saved `route: "smart"`), without changing Revex's global default. After implementation is verified, set this user's saved route to `smart` so ordinary turns are judged automatically. CLI `--local` and `--remote` continue to override the saved route for that run.

## Decision and execution

1. The existing explicit leading Codex/Claude invocation branch takes precedence and uses the direct helper path. Clef never rewrites or vetoes that instruction.
2. For an ordinary turn in `smart` mode, send only the current task to the configured local oMLX `/v1/decisions` endpoint. Add a dedicated `decision_model` setting (`REVEX_DECISION_MODEL`) with three accepted values: `clef` (default), `clef-flash`, and `self` (the selected chat model). This is independent of Revex's chat model, currently `deepseek-v4.1-flash-uncensored`. If `self` has no selected chat model, stay local. Use the existing oMLX base URL and key; do not send history, configuration, environment contents, or API keys in the task body. Reject unsupported setting values clearly. The verified request uses `model`, `input`, and two named predicate questions; the response gives each predicate's `probability`.
3. Normalize Clef's outputs into Codex-fit and refusal-risk scores, then validate both as finite values in `[0, 1]`; confirm the endpoint's actual field names first. Treat missing, malformed, or ambiguous output as no decision. A provisional gate is **Codex fit `>= 0.80` and refusal risk `<= 0.10`**. All other scores select Revex. Context-dependent follow-ups such as “do that” also stay in Revex when the current task alone is insufficient to classify. These numbers are starting values to calibrate, not claims of measured accuracy.
4. A passing decision invokes the enabled Codex CLI helper through the existing child record and cancellation lifecycle, then returns its answer without a parent-model summary. `smart` is specifically a Codex decision and does not silently send tasks to a generic `REVEX_FRONTIER` command, which may be Claude or another tool. If Codex is not enabled or cannot be confirmed available before launch, use Revex and show a short routing reason.
5. If Clef is unavailable, times out, or returns invalid scores, use Revex and report the fallback briefly. Once Codex has launched, surface a CLI failure or refusal and retain its record; do not automatically rerun a task that may already have changed files. Preserve the user turn in parent history, and never save a late assistant reply after cancellation.

The probes support a cautious gate: with Clef, code fix `fit .9722 / refusal .0242` would reach Codex; simple chat `.0077 / .0058`, own-app reverse engineering `.2524 / .1213`, and auth bypass `.3812 / .9454` would remain in Revex. Clef-flash gave `.9005 / .0981`, `.0158 / .0651`, `.1152 / .1515`, and `.131 / .8311` for those same four tasks, producing the same routes at the provisional gate. Four examples per model do not establish the thresholds or classifier reliability. The separate DeepSeek risk-classifier probe performed poorly and is not a dependency of this design.

## Latency, configuration, and observability

Clef's earlier observed first load was about 9.6 s and warm requests were 222–246 ms. A later run measured Clef first/warm at 1956/339–358 ms and Clef-flash at 1643/73–75 ms; this is a small sample, not a benchmark. Do not warm a decision model for users outside `smart` mode. In `smart`, show a short classifier/warmup status, use bounded connection and total deadlines that allow a first load, and measure cold and warm latency separately. A timeout returns the task to Revex without retrying the same decision request. The first version uses the existing oMLX base URL and key with the separate `decision_model`, defaulting to `clef`; it substitutes the chat model only for the explicit `self` choice.

Keep route, score bands or decision reason, endpoint failure type, and decision latency available for local diagnostics without logging raw task text, secrets, or full classifier payloads. Record provider-reported usage where available; do not invent token counts for `/decisions`. Explain in help and README that `smart` is a conservative automatic Codex route, while `auto` remains the existing one-letter frontier route. An explicit `Codexを呼び出して、...` request still fails clearly if Codex is disabled rather than falling back.

## Verification before enabling broadly

- Build a labeled calibration set covering routine code changes, simple chat, authorized reverse engineering, high-refusal security requests, mixed requests, and both Japanese and English phrasing. Choose thresholds from false-forward and false-local rates, then keep the initial rollout opt-in.
- Unit-test route parsing and precedence, the default and all three `decision_model` choices, score validation and boundary values, missing or contradictory decisions, disabled Codex, timeout and endpoint errors, and no retry after a started CLI fails.
- Use a fake `/decisions` server and fake Codex CLI in terminal tests to prove each route, that local turns make no Codex call, that forwarded turns make no local chat-generation request, that history and `/agents` records match the selected path, and that interruption discards late answers.
- Measure first-load and warm decision latency on the configured oMLX host. Verify the decision endpoint's real schema and whether it reports usage before implementing the adapter.
