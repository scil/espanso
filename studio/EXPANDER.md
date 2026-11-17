# Expander design, phase one

Status: phase one built on 2026-11-15, not yet used for a day. Forms and `choice` variables built on 2026-11-15. See [Build notes](#build-notes) for what differs from this design and what the tests found.

`snip` grows into the one resident program for daily work. It expands full triggers, as the espanso daemon does, and keeps the `;;` popup. After phase one, you stop espanso and run only this program.

The program reuses three library crates of this repository: `espanso-config` reads the configuration, `espanso-match` finds triggers, and `espanso-render` fills variables. The espanso worker connects these crates through `espanso-engine`. The program does not use `espanso-engine`. It copies the small parts of that glue that it needs.

## Scope

| Feature | Phase |
| --- | --- |
| Trigger expansion, `triggers:` lists, `left_word`, `right_word` | 1 |
| Per-application configurations (`filter_exec`, `filter_title`) | 1 |
| Variables: `date`, `echo`, `random`, `shell`, `script`, `clipboard`, global variables, nested `match` variables | 1 |
| `propagate_case` and `uppercase_style` | 1 |
| Cursor hint `$|$` | 1 |
| `undo_backspace` (Backspace right after an expansion brings the trigger back) | 1 |
| `force_mode` (`keys` types, `clipboard` pastes) | 1 |
| Regex triggers | 1, because the matcher supports them at no extra cost |
| The `;;` popup inserts any snippet, also snippets with variables | 1 |
| Forms and `choice` variables, rebuilt in gpui | 2 |
| Image snippets (`image_path`) | 2 |
| Automatic reload of changed configuration files | 2 |
| Tray icon, pause switch, start at sign-in | 2 |
| Markdown and HTML snippets | Not planned. No snippet uses them. |
| espanso's application patches (Brave, OneNote, VS Code) | Not planned. They only change espanso's injection timing. |

In phase one, a snippet with a form or a `choice` variable did nothing. The form window replaced that on 2026-11-15.

## Parts

New module `popup_core::expand`, with no Windows code, so that its tests run anywhere:

- `Engine::load(base)` calls `espanso_config::load`. It builds a map from match id to match. It builds one `RollingMatcher` and one `RegexMatcher` over all matches. It also keeps a template for each match and the global variables of each configuration. espanso numbers matches with a thread-local counter, so one thread loads everything.
- `Engine::key(event) -> Option<Expansion>` feeds one key press to the matchers. It keeps a history of matcher states, at most `backspace_limit` long. Backspace drops the last state. Arrow keys, Home, End, Page Up, Page Down, Escape and mouse clicks clear the history.
- When a trigger matches, the engine keeps only the matches of the active configuration. It finds that configuration from the foreground window: the full path of its program and its title. If more than one match is left, the popup opens with only those matches.
- `Engine::render(id, trigger, args)` builds the render context of the active configuration, adds regex captures as `echo` variables, works out the casing style, and calls the renderer.
- The result is an `Expansion`: how many Backspaces to send, the text, how many Left arrow presses to send for `$|$`, and the insert method.

The hook thread only passes key events on a channel, as now. A new engine thread runs the matcher and the renderer, because a `shell` variable can take seconds. The hook stays fast.

The engine thread inserts the text with the existing rules: Backspaces, then `type_text` (short text typed, long text pasted with the history markers), then the Left presses. `force_mode` overrides the short and long rule for one snippet. After an expansion, the matcher gets a word separator, as in espanso, so a trigger can follow an expansion.

The program ignores its own injected key presses, as now. espanso no longer runs, so the rule "injected input from another program closes the popup" stays only for other tools.

## Order of the work

1. Add `espanso-match` and `espanso-render` to the studio workspace. Write `expand::Engine` with tests: plain trigger, `left_word`, Backspace in the middle of a trigger, case propagation, cursor hint, a `date` variable, per-application filtering.
2. Connect it to the hook and the engine thread in `snip`. Test in Notepad with `--allow-injected`.
3. Let the popup render snippets with variables through the same engine.
4. Run it beside espanso with a test trigger prefix, then ask you to stop espanso and use it for a day.

## Risks

- If espanso still runs, both programs expand the same trigger. At start, the program checks for `espanso.exe` and logs a warning.
- A `shell` variable that waits forever blocks the next expansion. espanso has the same limit.
- The trigger prefix `;;` opens the popup, and many triggers start with `;;`. The popup stays open while you type a full trigger. When the trigger matches, the expansion closes the popup.

## Build notes

Differences from the design:

- If more than one match is valid for a trigger, the engine takes the one with the lowest `snip` priority. If several share the lowest priority, the engine shows them as a pick list in the popup, as espanso shows a list. It writes a log line. See [Pick lists in the popup](#pick-lists-in-the-popup). See [XPTEMPLATE.md](XPTEMPLATE.md#priority).
- The hook passes each key press to the engine through a callback of `hook::start`. A key that the popup swallows (Up, Down, Enter on a result) reaches the engine as a reset, because the application never got it. Injected input from another program also resets the engine.
- Tab is now its own key in the hook. In the popup, it still picks the selected result, like Enter.
- In the popup, a snippet with an image shows an `image` badge. Enter on it shows a note and inserts nothing.

Tests on 2026-11-15, in the Windows 11 Notepad, with keys sent by a test tool and `--allow-injected`:

- The engine loads the 1999 matches of the test configuration in about 60 ms.
- These cases put in the correct text: `:espanso` (plain text), `:date` (a `date` variable) and `:shell` (a `shell` variable).
- `:trig` has two lines, so it was pasted. Its `$|$` moved the caret correctly. `undo_backspace` and a popup insert with `;;espanso` and Enter also worked.
- With fast keys, Notepad dropped and repeated characters. This also happened with 8 Backspaces in one `SendInput` call, which erased text before the trigger. Typing and the Backspace and Left presses now go one key per call with 5 ms pauses, and a pause after the last Backspace. After this change, the expanded text was correct. The test tool's own fast keys were still dropped at times, and the undo test once erased one character too many.

Next: you type by hand for a day with espanso stopped. Then decide whether the pace needs to change.

## Forms and choice variables, phase two

A form opens near the caret, so you see the text around the place where the result goes.

Decisions, agreed on 2026-11-15:

- The form window takes the keyboard focus, so an input method (IME) works in it, for example for Chinese. After Submit or Cancel, the focus goes back to the window that had it.
- The trigger stays in the text while the form is open. Submit erases it and inserts the result. Cancel leaves the text as it was.

How it works:

- espanso's `FormExtension` and `ChoiceExtension` call a provider while they render. The new module `popup_core::form` turns their parameters into a `Form`: the layout split into text parts and `[[field]]` parts, and the fields in layout order. It copies espanso's `convert_fields`. A field has the type `text`, `choice` or `list`, and a `default`. A `text` field can be `multiline`. A `choice` or `list` field has `values`: a list, or a string split into lines, trimmed, without empty lines. A field that has no entry in `fields` is a single-line text field.
- A `choice` variable becomes a pick list in the popup since 2026-11-16 (see [Pick lists in the popup](#pick-lists-in-the-popup)). The provider returns the `id` of the chosen value.
- The engine thread renders, so the provider runs there. It records the foreground window, finds the caret, sends the form to the UI thread and waits for the answer. After the answer, it gives the focus back to the recorded window. Then the render goes on, and the engine erases the trigger and inserts the text as for any expansion.
- The UI opens a new window for each form, below the caret line. If there is no room below, the window opens above the line. The window never covers the caret line. It shows the layout as a sentence, with an input box in place of each field, and below it a preview of the result: the snippet text with the form values filled in.
- A `choice` or `list` field shows its value as text. When it has the focus, it becomes a filter box. A list of its values then opens at the bottom of the form, like the popup list.
- Keys: Tab and Shift+Tab move between fields. Enter submits. In a multiline field, Enter adds a line and Ctrl+Enter submits. Up and Down move in the list of a `choice` or `list` field. Esc cancels. If the form window loses the focus to another window, the form is canceled.
- The keyboard hook ignores all keys while a window of snip has the focus. Otherwise the popup and the expander get the keys typed into the form.

Build notes for forms:

- A `list` field and a `choice` field look the same. espanso shows `list` as a list box and `choice` as a drop-down list, and both pick one value.
- If the snippet text uses a form value, for example `{{form1.app}}`, the form shows a preview. Otherwise it shows none. The `:editmatch` snippet passes the value to a `shell` variable, so its form has no preview.
- If a form ends because another window took the focus, the focus stays with that window.
- If the session already ended, the popup ignores a late `Open` message. Before this change, the popup opened under a form that started from a `;;` trigger.

Tests on 2026-11-15, in Notepad, with keys sent by a test tool:

- `;;espt` opened the form below the caret line. The preview changed with each typed key. Enter erased the trigger and inserted the text, with the caret at `$|$`.
- Esc on `:editmatch` closed the form and left the trigger in the text.
- In `:editmatch`, typing filtered the list of files, and Down moved the selection.
- You tested `;;scU` by hand in Cmder, with snip and Cmder run as administrator. The form worked, and Chinese typed with an input method went into the field.
- Not tested yet: a `choice` variable, and a form picked in the popup.

## Triggers that start a longer trigger

Added on 2026-11-15. espanso expands a trigger as soon as it is complete. If one trigger is the start of another, for example `;;scu` and `;;scun`, the longer one never expands. A scan of your match files found about 34 such pairs.

snip waits instead:

- When a typed trigger is also the start of a longer trigger of the active configuration, it does not expand at once. Regex triggers do not count.
- If the next keys make the longer trigger, the longer one expands. If it is also the start of a longer trigger, it waits again.
- If a key fits no longer trigger, the short trigger expands. The engine erases the trigger and the keys after it, and types the keys again after the text.
- If no key comes in 500 ms, the short trigger expands. `--prefix-wait-ms` changes this time.
- While the `;;` popup is open, the wait does not end by time, because the user picks in the popup. For example, `;;anki` shows `;;anki` and `;;anki2`, and neither expands on its own. The next key decides: Enter inserts the selected snippet, and a key that fits no longer trigger expands the short one. If the popup closes without a key, nothing expands.
- Backspace erases a key after the trigger and keeps the wait. A Backspace on the trigger itself, an arrow key or a click ends the wait, and nothing expands.

When a trigger matches, espanso's matcher drops its state. So while a trigger waits, the engine compares the typed keys with its own list of triggers. It follows the `propagate_case` setting of each trigger for upper and lower case.

## Pick lists in the popup

Added on 2026-11-16. The `;;` popup and the list of a `choice` variable are now one window. You agreed to set aside input methods (IME) in the list for now.

A pick list opens in two cases:

- A `choice` variable.
- A trigger that several matches share with the same priority (a tie). See [XPTEMPLATE.md](XPTEMPLATE.md#priority).

How it works:

- The engine calls `Ask::pick` with a list of `PickItem` rows: a title, a second line and a badge. The popup shows the title and the badge on one line. For a tie, the title is the match `label` or the first line of its text. The second line is the text. The badge is the file and the match number, for example `_prompts.yml:5`. For a `choice` variable, the title is the value label.
- `snip` calls `Shared::start_pick`. From then on, the hook swallows every typed character and Backspace. So the text before the caret stays the trigger, and the engine erases the right number of characters. The typed characters still filter the list.
- The popup opens below the caret with the rows. It looks like the `;;` popup. A pick list replaces an open `;;` session. For example, a tie trigger can start with `;;`.
- Keys: typing filters, Up and Down move, Enter or Tab picks, Ctrl+1 to Ctrl+9 picks the row with that number, a click on a row picks. Esc, Backspace on an empty filter, or a click outside cancels and leaves the trigger in the text. Any other key cancels and goes through.
- The engine thread waits for the answer, then ends the hook session and inserts the text.

Limits:

- An input method cannot type into the filter, because the keys never reach a text box.
- A form with more than one field still opens the form window. Its `choice` and `list` fields keep their own list in that window.

Tests on 2026-11-16: unit tests for the hook's pick mode and for the engine. Not tested by hand yet, because the running `snip` (with administrator rights) holds `target/release/snip.exe`.
