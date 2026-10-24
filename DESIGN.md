# claude++ UI/UX goals

claude++ exists to be a *light, fast* front end for the official Claude Code. The UI serves that: it should
disappear while you work and be obvious when it asks something of you. Seven goals, each with how we know we
meet it. Where a goal is enforced by a test, the test is named.

## Goals

| # | Goal | What it means in practice | How it is held |
|---|------|---------------------------|----------------|
| 1 | **Responsive** | A key press is on screen before you notice. Streaming never janks. | Frame budget: every surface under 1 ms at 200×50 (measured 0.11–0.17 ms: transcript of 3,000 turns, 60-item popup, 200-session explorer, model picker, question panel). `cargo test --release -- --ignored --nocapture frame_budget`. Events are coalesced into one redraw. Slow work (git, sips, clipboard) is off the hot path or one-shot. |
| 2 | **Light** | RAM and CPU stay near zero when you are not using a feature. | Idle UI ≈ 2–3 MB, one wake-up per second, no polling. Features that could spawn something (status line command, command/model lists) are lazy, cached, or opt-in. |
| 3 | **Uniform** | Learn a gesture once; it means the same everywhere. | One interaction grammar and one visual vocabulary (below). `theme::tests::surfaces_use_theme_roles_not_raw_colours` fails the build if a surface names a colour instead of a role. |
| 4 | **Consistent structure** | Every panel is in the same place and shape. | One renderer, `panel.rs`; surfaces supply content only (below). |
| 5 | **Honest** | You see what will happen before, and what did happen after. | Costs and side effects are stated first (re-cache sizes, fast mode pricing). The backend's reported state, not our request, decides what a note says. Failures are shown, not swallowed. |
| 6 | **Safe by construction** | You cannot answer a prompt by accident. | Pasted text is never hotkeys (`pasted_text_is_text_not_hotkeys`, `pasting_fills_other…`); hidden options cannot be triggered; project-level commands run only in folders Claude Code trusts. |
| 7 | **Degrades gracefully** | A small or plain terminal still works. | Every panel line is clipped to its width (`nothing_is_wider_than_the_panel…`); the status line hides under 14 rows; no meaning is carried by colour alone (✓ ✗ ● ☑ ❯ are text); images fall back to labels. |

## Interaction grammar

| Key | Always means |
|-----|--------------|
| ↑ ↓ | Move the cursor. Short choice lists wrap; long scrolling lists (the session explorer) clamp and add PgUp/PgDn/Home/End. |
| Enter | Confirm the highlighted thing. |
| Esc | Back out one level without changing anything. In a prompt the agent is waiting on, that level is "decline". Typing in a field: first Esc stops typing, the next backs out. |
| Space (or `f`) | Toggle (checkboxes, fast mode). |
| ← → | Adjust the other axis: effort level, question tab. |
| 1–9 | Jump to / pick the numbered row. |
| Tab | Complete (popups). |
| typing | Filters a list that has a filter box; elsewhere starts "Other" in a question. |
| ctrl / alt chords | Belong to the app (tabs, interrupt, paste image) and always work, even over a prompt. |

Key hints are always one line in one shape, built by `theme::hints`: **`key` action · `key` action**.

## Visual vocabulary (`src/theme.rs`)

| Role | Used for |
|------|----------|
| `accent` (cyan) | You: your messages, the focused tab, frames of things you opened. |
| `attention` (yellow) | **Only** "the agent is blocked on you": prompt frames and their notes. Nothing else is yellow. |
| `ok` / `danger` | Success, additions / failure, removals, denial. |
| `dim` | Descriptions, hints, separators, work still in progress. |
| `selected` | The row under the cursor (reverse video). |
| `code` | Code, commands, file contents. |
| `❯` cursor · `●` current value · `☐ ☑` checkbox · `✓ ✗` done/missing | Same marks in every list. |

The colour of a frame tells you who is waiting on whom: yellow, the agent waits on you; cyan, you opened it.

## One panel, drawn in one place

Every interactive surface is a **panel**: anchored at the bottom of the screen, directly above the input, full
width, in a titled frame, with a body that scrolls to keep the cursor row in view, optional lines pinned above
and below it, an optional detail pane beside it on wide screens, and a one-line key-hint footer. The code that
does this is `src/panel.rs` and nothing else draws a panel. A surface only builds a `View` (content):

| Surface | Built in | Frame | Keyboard |
|---------|----------|-------|----------|
| permission, `AskUserQuestion` | `prompt.rs` | yellow | the panel's |
| model / effort / fast mode | `ui::model_view` | cyan | the panel's |
| config (`/config`) | `settings.rs` | cyan | the panel's |
| usage | `usage.rs` | the panel's; plan bars turn yellow at 70%, red at 90% | the panel's |
| session config | `tabcfg.rs` | the panel's; green `allow`, red `deny` | the panel's |
| session list | `explorer.rs` | the one full-screen surface: no frame, title on the first row, hints on the last (`panel::render_full`); preview beside the list. Always available; → or Esc leaves | its own screen |
| session dock | `explorer.rs` (`dock_lines`) | cyan border when focused, dim otherwise; yellow `!` = needs you | a 30-column strip, not a panel: opt-in, ← / ctrl+r |
| `/command`, `@file` suggestions | `ui::suggest_view` | cyan | the input's (it keeps the cursor) |

Shared pieces keep them alike: `panel::row` (cursor marker + highlight), `theme::hints` (footer shape; middle
hints are dropped first on narrow screens, the first and the last always stay), `theme::frame`. Panels never
change height as you move the cursor within them (config reserves room for its longest help text).
A panel is at most the screen minus the tab strip, input, status line and two rows of transcript.
`every_panel_sits_at_the_bottom_above_the_input_with_the_same_frame` draws all seven surfaces at three widths
and fails if any frame is not directly above the input, lacks its title, or has no footer.

Adding a surface means writing a function that returns a `View`. Adding a config option means one entry in
`settings::ITEMS` and one arm in `App::apply_setting`.

## Questions (the panel this document was written for)

- One component for questions and permissions: a choice list with the grammar above.
- Several questions get tabs (`○` open, `✓` answered), answering advances to the next unanswered one, and a
  **Review** tab lists every answer before anything is sent. A single question is answered directly.
- Multi-select uses checkboxes and needs at least one choice. "Other" is always the last row, typed inline;
  typing anywhere starts it. Option `preview` text is shown for the highlighted option.
- Declining is Esc; the agent is told so.

## Known gaps (honest list, not promises)

- Clipboard image paste runs `osascript` on the UI thread (~100–200 ms freeze on ctrl+v).
- No light-theme variant; roles are chosen for dark terminals.
- Config options are choice lists only; free-form values (a number, a path) will need a text-entry row.
- The transcript's tool/diff rendering predates the theme and is only partly cleaned up.
- No mouse. Intentional (it would break text selection), but it is a choice.
