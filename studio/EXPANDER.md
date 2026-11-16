# Expander design, phase one

Status: phase one built on 2026-11-15, not yet used for a day. See [Build notes](#build-notes) for what differs from this design and what the tests found.

`popup-ime` grows into the one resident program for daily work. It expands full triggers, as the espanso daemon does, and keeps the `;;` popup. After phase one, you stop espanso and run only this program.

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

A snippet with a form or a `choice` variable shows the `espanso` badge in phase one. If you type its trigger, the program logs it and does nothing.

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
2. Connect it to the hook and the engine thread in `popup-ime`. Test in Notepad with `--allow-injected`.
3. Let the popup render snippets with variables through the same engine.
4. Run it beside espanso with a test trigger prefix, then ask you to stop espanso and use it for a day.

## Risks

- If espanso still runs, both programs expand the same trigger. At start, the program checks for `espanso.exe` and logs a warning.
- A `shell` variable that waits forever blocks the next expansion. espanso has the same limit.
- The trigger prefix `;;` opens the popup, and many triggers start with `;;`. The popup stays open while you type a full trigger. When the trigger matches, the expansion closes the popup.

## Build notes

Differences from the design:

- If more than one match is valid for a trigger, the engine takes the first one and writes a log line. The popup does not open with those matches.
- The hook passes each key press to the engine through a callback of `hook::start`. A key that the popup swallows (Up, Down, Enter on a result) reaches the engine as a reset, because the application never got it. Injected input from another program also resets the engine.
- Tab is now its own key in the hook. In the popup, it still picks the selected result, like Enter.
- In the popup, a snippet with a form, a `choice` variable or an image shows a `form` badge. Enter on it shows a note and inserts nothing.

Tests on 2026-11-15, in the Windows 11 Notepad, with keys sent by a test tool and `--allow-injected`:

- The engine loads the 1999 matches of the test configuration in about 60 ms.
- These cases put in the correct text: `:espanso` (plain text), `:date` (a `date` variable) and `:shell` (a `shell` variable).
- `:trig` has two lines, so it was pasted. Its `$|$` moved the caret correctly. `undo_backspace` and a popup insert with `;;espanso` and Enter also worked.
- With fast keys, Notepad dropped and repeated characters. This also happened with 8 Backspaces in one `SendInput` call, which erased text before the trigger. Typing and the Backspace and Left presses now go one key per call with 5 ms pauses, and a pause after the last Backspace. After this change, the expanded text was correct. The test tool's own fast keys were still dropped at times, and the undo test once erased one character too many.

Next: you type by hand for a day with espanso stopped. Then decide whether the pace needs to change.
