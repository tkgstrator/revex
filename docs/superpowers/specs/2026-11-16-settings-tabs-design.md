# Settings tabs for Revex

## Intent and current behavior

Make setup and `/config` easier to navigate, and make it clear whether a model server provider is merely suggested or has been chosen. In an isolated first-run PTY, the screen already shows `> Provider: Ollama` and `[Ollama]` before Enter, although no `~/.revex` file exists. `setup_view` defaults the provider and URL to Ollama and focuses the Provider field; `draw_setup` brackets that value unconditionally. Enter only moves to URL. Left/Right on Provider immediately changes both the candidate and URL.

Setup currently has one scrolling form with fields 0–6, rather than separate settings screens. Tab moves between some fields but toggles manual mode on Model and Reasoning. Left/Right move the cursor in text fields and change choices elsewhere. Existing `/model` opens this same form at Model or Reasoning. Nothing is saved until the final Confirm action.

## Approaches

| Approach | Tradeoff |
| --- | --- |
| Keep the form and only clarify Provider focus | Smallest change and fixes the misleading bracket, but it does not provide settings-screen navigation. |
| Add Server, Model, and Helpers tabs over one shared draft **(proposed)** | Matches the requested screen navigation while retaining current settings and one final save. Requires explicit focus rules and changes to existing Tab behavior. |
| Give each tab its own Save action | Simple per-screen ownership, but partial settings can be committed before the user reviews the whole configuration. |

The proposed direction is the shared-draft tab layout. This is a design for user approval, not an implementation decision already made.

## Proposed interaction

- Show a visible tab bar with **Server**, **Model**, and **Helpers**. Server contains Provider, URL, and API key. Model contains model ID and reasoning effort. Helpers contains the helper checkboxes and final Confirm.
- Keep edits to all three tabs in one unsaved setup draft. Setup opens with the tab bar focused. Tab moves to the next tab and Shift+Tab to the previous tab, leaving the bar focused and preserving each tab's field focus and draft values. Left/Right also change tabs while the bar is focused. Enter moves from the bar into that tab's fields; Esc from a field returns to the bar, while Esc from the bar cancels setup. Left/Right within URL, API key, or manual model/effort fields retain cursor movement; choice lists use Up/Down. Show the current keyboard action in the footer.
- On first run, display Ollama as a **suggestion** without a selected marker. Moving over provider choices changes the candidate only. Enter commits the candidate to the draft, applies its preset URL (or leaves Custom API for manual URL entry), and advances to URL. In `/config`, distinguish the currently saved provider from a newly focused candidate; opening the screen alone does not represent a new choice. Leaving a candidate without Enter must not replace the draft URL.
- Final Confirm validates and saves the entire draft using the existing home and project settings rules. Esc from `/config` discards the draft and returns to chat; first-run Esc exits without saving. `/model` opens the Model tab at its relevant field. A pending `/models` response may update the Model tab only for the current endpoint and request, even if the user switched tabs while it was in flight.

## Scope and verification

This changes setup rendering, setup key handling, and their tests. It does not add a settings format or change existing configured values, helper CLI availability checks, endpoint authentication, or chat navigation.

- Rendering tests distinguish first-run suggestion, saved provider, candidate focus, and committed draft choice. A narrow terminal still shows the active tab, focused control, and final Confirm.
- Isolated PTY tests cover Tab/Shift+Tab and tab-bar Left/Right, retention of URL/key/model/helper draft edits across tabs, and Left/Right cursor movement inside editable fields. Enter on Provider must update the draft URL but must not write a settings file; only final Confirm writes settings.
- PTY tests cover `/config` Esc without writes, `/model` opening Model, key masking, existing endpoint-change protection, and a stale `/models` result after an endpoint or tab change. Update current tests that assume `[Ollama]` before Enter or Tab from URL to API key.
