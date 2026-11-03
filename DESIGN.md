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
| `working` (Claude orange) | The agent at work: the `✻ Thinking… (12s · ↓ 340 tokens · esc to interrupt)` line above the input, and the welcome frame. |
| `user_band` | The band behind your messages in the transcript (`❯` text, as in Claude Code). |
| `added_line` / `removed_line` | Diff bands: the changed lines of an Edit/Write, numbered from Claude Code's own hunks, with context lines plain. |
| `syn_keyword` / `syn_string` / `syn_number` / `syn_comment` | Fenced code with a language. Function names stay plain: yellow is not for decoration. |
| `❯` cursor · `●` value in effect · `☑ ☐ ▣ ◑` shown / hidden / some / shortened | Same marks in every list; choices are `●value  value`, not words in brackets. |
| `✓ ? ✗` allow / ask / deny · `⌂ ▤ ◇` user / project / local file | Rules and where they live. |
| `◆ ◐ ✓ ✗ ·` needs you / working / done / failed / stopped | One state glyph in the tab bar, dock and session list (`jobs::State::glyph`). |
| `$ ≡ ± + ⌕ ◎ ◈` | Bash, read, edit, write, search, web, agent: a tool call is its glyph plus what it touched. Unknown tools keep `Name(...)`. |
| `⏎ ⎋ ␣ ⇥ ⌃ ⌥ ⇧` | Keys in hint footers (`theme::hints` converts the words). |

Colour is for meaning, never decoration: keys in hints and frame titles are cyan (yours to press), tool icons carry a hue per kind (`theme::tool_hue`) while the line colour says how the call went, tab marks and the status line colour only what needs noticing (hit rate and cost green; context over 85% and a cold cache red).

Glyphs are single-column and non-emoji (`theme::tests::every_glyph_is_one_column_wide`), so columns never shift. A glyph carries state; a word appears only where the glyph alone would be ambiguous.

The colour of a frame tells you who is waiting on whom: yellow, the agent waits on you; cyan, you opened it.

Colours work in either mode, and keep working when the terminal switches between them:
- Named ANSI colours (red, green, yellow, grey...) come from the terminal's own palette, which each theme tunes for its
  background, so they need nothing.
- Anything that sets a background (your message band, diff lines) sets its text colour too. Left to the terminal's
  default, text on a band disappears as soon as the terminal switches modes under it (black on dark grey).
- Fixed 256-colour shades (the `/color` accents, syntax colours, some tool icons) have a darker variant for a light
  background, picked when drawn.
- Which mode: `/config` → Colors. `auto` asks the terminal for its background (OSC 11, with a DA1 query behind it so
  the read always ends) at start and again every few seconds while you are not typing, and restyles everything when it
  changes; `dark` / `light` fix it. The terminal's own change notification (mode 2031) is not used: crossterm 0.29
  cannot parse it and would hold the keys typed after it. No answer means dark, and is not asked again.
- Diff bands use quiet 24-bit tints where `COLORTERM` says the terminal takes them.

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
| optimize | `optimize.rs` | level chips in the head; ticked rows are applied; sizes dim at the right | the panel's |
| permissions | `permissions.rs` | tabs Allow/Ask/Deny in the head; the file a rule comes from is dim; delete asks in red | the panel's |
| instructions | `instructions.rs` | checkbox tree: `[x]` in, `[-]` partly, `[ ]` hidden (dim) | the panel's |
| usage | `usage.rs` | the panel's; plan bars turn yellow at 70%, red at 90% | the panel's |
| context (`/context`) | `context.rs` | stacked bars in the head (the window, then how the used part divides); one row per category in its own colour; on a wide screen a grid of the window and the row's detail beside the list. The gauge turns yellow at 70%, red at 85% | the panel's; enter acts on skills and memory |
| session config | `tabcfg.rs` | the panel's; green `allow`, red `deny` | the panel's |
| editor pane | `pane.rs` | accent rules; the editor draws inside | the editor's, except `ctrl+\\` (cancel); a prompt takes the keyboard back |
| session list | `explorer.rs` | the one full-screen surface: no frame, title on the first row, hints on the last (`panel::render_full`); preview beside the list. Always available; → or Esc leaves | its own screen |
| session dock | `explorer.rs` (`dock_lines`) | cyan border when focused, dim otherwise; yellow `!` = needs you | a 30-column strip, not a panel: opt-in, ← / `/sessions` |
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

- Config options are choice lists only; free-form values (a number, a path) will need a text-entry row.
- The transcript's tool/diff rendering predates the theme and is only partly cleaned up.
- No mouse. Intentional (it would break text selection), but it is a choice.

## Session lists (full screen and side panel)

Rows are grouped under headings; Tab cycles *by state* (◆ needs you, ◐ working, ✗ failed, ✓ done, · stopped, ○ earlier), *recent* and *by folder*. Headings are rows too: Enter on one folds (▸) or unfolds (▾) its group, and the choice (like the grouping) is shared by the side panel and the full list and saved in the config (`session_folds`, `session_group`), so you can hide "done" or "earlier" and keep what needs you in view. An open tab reports its own state (needs you = a prompt is waiting, working = mid-turn, done = idle). In the side panel the cursor follows you: moving onto an open conversation switches to its tab, moving onto a stored one previews it in the main area, and Enter opens it in a new tab (or switches to it) and hands the keyboard back to the input.

The permission mode is always in the status line (◇ manual, ⏵⏵ accept edits, ◈ plan, ✦ auto), with ⇧⇥ to cycle it.

Both lists show the newest 200 conversations of every folder, named or not (an unnamed one is titled by its first prompt), so a conversation closed anywhere can be reopened; other folders' rows carry the folder name and open in that folder. Titles are cached per transcript (path, mtime, size), so the side panel's periodic refresh does not re-read unchanged files.
