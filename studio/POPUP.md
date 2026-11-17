# Caret popup prototypes

A caret popup is a small snippet search window that opens next to the text caret, the way an input method candidate window does. Two prototypes build the same popup with two UI frameworks, so that you can compare them on the same task.

| Crate | Purpose |
| --- | --- |
| `popup-core` | Shared logic: caret lookup, placement, focus, snippet search, timing. |
| `popup-slint` | The popup built with Slint 1.18. |
| `popup-gpui` | The popup built with gpui through gpui-kit 0.9.1. The crate also builds `snip`, the resident input method style popup. |

The frameworks share everything except the window and its widgets. A difference in the results comes from the framework, not from the logic.

There are two modes. In the one-shot mode (`popup-slint`, `popup-gpui`), a new process starts for each call and the popup has its own search box. In the resident mode (`snip`), one process stays in the background, and the search keyword stays in the target application. The resident mode is described in [Resident mode](#resident-mode).

## How it works

1. The popup remembers the window that has focus.
2. A thread loads the snippets through `snip-core`.
3. `popup-core` finds the caret. It tries these methods in order and uses the first one that works:
   1. UI Automation `TextPattern2::GetCaretRange`.
   2. UI Automation `TextPattern::GetSelection`.
   3. The Win32 caret from `GetGUIThreadInfo`.
   4. The bottom-left corner of the focused UI Automation element.
   5. The mouse pointer.
4. The popup opens 6 px below the caret. If it does not fit below, it opens above. It always stays inside the work area of the caret's monitor.
5. You type to search. Up and Down select a snippet. Enter inserts it. If you type a full espanso trigger, such as `;;doc`, espanso expands it at once and the popup closes. The popup sees espanso's injected key presses and knows that the keyword at the caret is gone.

Escape closes the popup.
6. The popup gives focus back to the previous window, then sends the text.

UI Automation runs on its own thread with a 250 ms limit, because some applications answer slowly.

If a snippet is plain text with no variables, the popup can insert it. Other snippets show an `espanso` badge, and Enter shows "Cannot insert". These snippets need espanso to render them.

## Use it from espanso

By default the popup prints the chosen text to standard output. An espanso `shell` variable can start it:

```yaml
matches:
  - trigger: "//"
    replace: "{{pick}}"
    vars:
      - name: pick
        type: shell
        params:
          cmd: "E:\\GitHub\\scil\\espanso\\studio\\target\\release\\popup-slint.exe"
```

espanso deletes `//`, waits for the popup, and types what the popup printed. This path is not tested yet.

## Command line

```bash
cargo build --release -p popup-slint -p popup-gpui
target/release/popup-slint.exe --timing --keep-open --type
```

| Flag | Effect |
| --- | --- |
| `--type` | Insert the text into the previous window instead of printing it. [Inserting text](#inserting-text) describes how. |
| `--timing` | Print timing marks to standard error. |
| `--keep-open` | If the popup loses focus, it stays open. Use it for tests. |
| `--exit-after-ms N` | Close N ms after the first frame. Use it for benchmarks. |
| `--query TEXT` | Start with this search text. |
| `--base DIR` | espanso folder. Without it, the tools use `SNIP_BASE` or the standard folder. |

`cargo run -p popup-core --example caret_probe -- 3` waits 3 seconds, then prints the caret position of the focused window and the method that found it.

`cargo run -p popup-core --example filter_probe -- doc ";;"` prints the first three matches for `doc` after the trigger `;;`, in popup order, with their text.

`cargo run -p popup-core --example paste_probe -- 3 1 hello` waits 3 seconds, sends 1 Backspace to the focused window, then types `hello`. Two more arguments set the chunk size and the pause in ms. If the fourth argument is `paste`, it pastes instead. A text of the form `@20` stands for 20 numbered test lines.

## Results

Measured on 2026-11-15 on Windows 11 with two monitors at 100 percent scale, release builds with the GNU toolchain, and 2000 snippets in 24 files.

| | Slint | gpui |
| --- | --- | --- |
| First frame, median of 10 runs | 233 ms | 271 ms |
| First frame, range | 220 to 253 ms | 267 to 280 ms |
| Memory with the popup open | 84 MB | 71 MB |
| Release binary | 40 MB | 66 MB |
| Placement on the second monitor | Correct | Correct after the fix below |
| Search, select and insert in Notepad | Works | Works |
| Default look | Plain, needs styling work | Polished, with shadow and rounded corners |

Both popups spend about 25 ms on the caret and about 30 ms on the snippets. Most of the time goes to the framework start. Slint spends about 130 ms in `Popup::new`. gpui spends about 200 ms in `application()` and `gpui_kit::init`. A popup that must open in under 100 ms needs a resident process. Such a process starts once and stays hidden. For each call, it only shows its window.

In Notepad on Windows 11, UI Automation found the exact caret in 16 to 18 ms.

## Problems found and fixed

- If `WindowOptions::display_id` is empty, gpui checks the window bounds against the primary display. Bounds on another monitor fail that check, and gpui opens the window with default bounds. The popup now passes the display that holds the caret.
- The first focus method sent an Alt key tap. Notepad showed its menu access keys. The popup now sends an empty mouse event, which also lifts the foreground lock.
- Unicode key events garbled long snippets in Notepad: characters were dropped and repeated. By default, long text is now pasted. See [Inserting text](#inserting-text).
- Slint showed the search icon glyph as a box, because the font has no such glyph. The icon is removed.

## Resident mode

`snip` works like an input method candidate window. You type in the target application as usual:

1. You type the trigger, `;;` by default. The popup opens below the caret. It lists the snippets whose triggers start with `;;` first.
2. You type the keyword. The characters go into the target application, and the popup filters its list after each one. Many triggers start with the popup trigger, so the popup compares the keyword with each snippet trigger without that prefix. For the keyword `doc`, `;;doc` comes first.
3. Up and Down select a snippet. Enter or Tab inserts it. Ctrl+1 to Ctrl+9 inserts the row with that number. After each typed key, the first row is selected, so Enter or Tab inserts the first row. The `--tab` option changes what Tab does.

`;;.` repeats the last search. If `.` is the first keyword character, snip replaces it with the keyword of the last `;;` search, inserted or not. snip types that keyword into the application, and you can go on typing. Before the first search, `.` is an ordinary keyword character.

Each row has one line: a number for the first nine rows, then the snippet `label`. A snippet without a label shows the start of its text. The rows do not show triggers, but the keyword still matches them.

An exact match is a snippet with a trigger that is the typed text in full, ignoring case. For `;;anki`, the snippet with the trigger `;;anki` is an exact match, and `;;anki2` is not. Exact matches come first. Each one has a colored bar on its left edge, a bold title, and its trigger in a colored badge on the right.
4. The popup erases the trigger and the keyword with Backspace key presses, then inserts the snippet text as [Inserting text](#inserting-text) describes.

If you type a full espanso trigger, such as `;;doc`, espanso expands it at once and the popup closes. The popup sees espanso's injected key presses and knows that the keyword at the caret is gone.

Ctrl+E or Right opens the selected snippet in an edit form. A right click on a row opens that snippet. The popup itself grows to the right and shows the form there, beside its list, and takes focus. A click on another row, or Alt+Up and Alt+Down, opens that row in the form. Tab moves between the boxes of the form. Ctrl+F gives the list the focus, and you type there to search. Up and Down then show each row in the form while the list keeps the focus, and Right puts the cursor in the form. Esc closes the form, and the popup shows only its list, with focus: Enter then inserts, and Esc again closes the popup. If no snippet matches, Right closes the popup and moves the caret. Ctrl+N opens the form with a new snippet, and the typed trigger and keyword become its first trigger. Both erase the typed text first (see [EDITOR.md](EDITOR.md)).

Escape closes the popup. Backspace on an empty keyword, any other key, a key with Ctrl, Alt or Win, and a click outside the popup also close it. If no snippet matches, Enter closes the popup and goes to the application. A click on a row inserts that row.

```bash
cargo build --release -p popup-gpui
target/release/snip.exe --timing
```

| Flag | Effect |
| --- | --- |
| `--trigger TEXT` | Characters that open the popup. The default is `;;`. No snippet in the test configuration has `;;` alone as its trigger. |
| `--timing` | Print timing marks to standard error. |
| `--base DIR` | espanso folder, as in the one-shot mode. |
| `--log FILE` | Write events to FILE. It records no typed text. |
| `--allow-injected` | Also read key presses that another program sends with `SendInput`. Use it for tests with a tool that types. |

The insert flags in [Inserting text](#inserting-text) also apply.

### How the resident mode works

- `popup_core::hook` installs low-level keyboard and mouse hooks (`WH_KEYBOARD_LL`, `WH_MOUSE_LL`) on a thread of its own. A state machine keeps the last typed characters and the keyword. It has no Windows code, so its tests run anywhere.
- The hook passes characters and Backspace to the application. It swallows Up, Down, Enter, Tab (unless `--tab ignore`), Escape, Ctrl+E, Ctrl+N and clicks inside the popup, right clicks too. It also swallows Right with a result, and Ctrl+1 to Ctrl+9 with that row. The application never sees these keys.
- The hook reads characters with `ToUnicodeEx` in the keyboard layout of the foreground window. Flag 4 keeps dead keys working in the application.
- The hook ignores injected input for the keyword, so espanso's expansions and the popup's own key presses never extend it. Injected input from another program closes an open popup, because espanso then replaced the text at the caret. The popup marks its own input with `dwExtraInfo = OWN_INPUT`, so `--allow-injected` still ignores it.
- A controller thread finds the caret for each session, so UI Automation never blocks the hook or the UI thread.
- gpui creates the window once, hidden. For each session, the popup only moves and shows it. The window has `WS_EX_NOACTIVATE`, so it never takes focus from the application.
- The popup inserts text on a thread of its own, because a paste waits 300 ms for the target to read the clipboard.
- `--log FILE` writes events to a file: open, move, accept, each close with its reason, and how many key events Windows accepted. It records no typed text. The program has no console. So if a shell starts it, standard error is lost.

The hook sees every key press. It keeps only the last few characters and the current keyword in memory, and it logs nothing.

### Resident mode results

Measured on 2026-11-15 in Notepad on Windows 11, with the caret on the second monitor at 100 percent scale:

| Step | Time from the trigger key |
| --- | --- |
| Caret found through UI Automation | 12 to 25 ms |
| First frame of the popup | 24 to 37 ms |

The process starts in about 275 ms, once. It uses about 70 MB of memory.

A test typed `@@sig` with `--trigger @@`, then pressed Enter. The popup erased the 5 characters and typed the first match (`:stop_sign:`).

### Problems found and fixed in the resident mode

- `SetWindowPos` and `ShowWindow` send messages such as `WM_SIZE` to the window procedure at once. gpui needs its app state for these messages. The UI thread holds that state while it runs the popup's code. So a direct call panicked with "RefCell already borrowed". A separate window thread now makes these calls, and gpui gets the messages from its message queue.
- On the test machine, a chosen emoji came out as `v` in Notepad. The log showed that Windows accepted all Ctrl+V events, so Notepad got the V without Ctrl. Short text is now typed as Unicode key events, which need no Ctrl. For a paste, Ctrl goes down in a call of its own, and the V follows 30 ms later.
- After the first hide, a new session showed an empty window. gpui draws a newly visible window on `WM_SHOWWINDOW`. `SetWindowPos` with `SWP_SHOWWINDOW` does not send that message. The popup now moves the window with `SetWindowPos`, then shows it with `ShowWindow(SW_SHOWNOACTIVATE)`.

### Resident mode limits

- The popup closes on injected input from any other program, not only from espanso. A tool that sends keys in the background, such as a key remapper, can close it. `--allow-injected` turns this check off, because test input is injected too.

- A program that shows its own completion list, such as a code editor, also reacts to Up, Down and Enter. The hook swallows those keys while the popup is open, so the program's list does not get them.
- The hook counts the keyword from the key presses. If the application changes the text itself, for example with auto-correction, the Backspace count is wrong.
- Snippets with an image show the `image` badge. Enter shows a note and inserts nothing. Snippets with a form open the form window near the caret. A `choice` variable opens a pick list in the popup (see `EXPANDER.md`).
- Windows does not give the keys of an administrator window to the hook of a normal process. If you use an administrator window, such as Cmder, run snip as administrator. `scripts/win/snip.ps1 -Action start` does this with one UAC prompt, and `-Action stop` stops it. A normal process cannot stop a snip that runs as administrator. The script writes the log to `%LOCALAPPDATA%\snip\events.log`.
- A low-level hook must answer within the system timeout, about 300 ms by default. The hook only updates its state and sends a message, so it answers in microseconds.

## Inserting text

Both modes use the same rules. A UTF-16 unit is one character, or half of an emoji. If text has at most 32 UTF-16 units and no line break, it is short. Short text is always typed as Unicode key events. Long text is pasted by default.

| Flag | Effect |
| --- | --- |
| `--short-limit N` | Text with at most N UTF-16 units and no line break is short. The default is 32. |
| `--long-text paste` | Paste long text through the clipboard. This is the default. |
| `--long-text type` | Type long text as key events. The clipboard stays untouched. |
| `--type-chunk N` | Typing sends N characters per `SendInput` call. The default is 1. |
| `--type-gap-ms N` | Typing waits N ms between calls. The default is 5. |
| `--prefix-wait-ms N` | A typed trigger that is also the start of a longer trigger waits N ms for the next key. The default is 500. While the popup is open, the wait has no time limit. See `EXPANDER.md`. |
| `--tab accept\|move\|ignore` | What Tab does while the popup is open. `accept` (default) inserts the selected row, as Enter does, and Shift+Tab does the same. `move` selects the next row, and Shift+Tab selects the previous row. `ignore` leaves Tab to the application, and the popup closes. A pick list follows the same setting. |

A paste does not leave the snippet in the clipboard history:

1. The popup saves the clipboard text.
2. It puts the snippet into the clipboard with four marker formats. `Clipboard Viewer Ignore` tells Ditto to skip it. `ExcludeClipboardContentFromMonitorProcessing` tells other clipboard managers to skip it. Ditto recorded pastes that had only this format. `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard`, set to 0, keep it out of the Windows clipboard history (Win+V) and the cloud clipboard.
3. It sends Ctrl+V.
4. After 300 ms, it puts the old text back, with the same markers.

Only text comes back. If the clipboard held an image or a list of files, it is empty after a paste. A clipboard manager that ignores the markers still records the snippet.

Tested on 2026-11-15. A pasted marker text left 0 entries in Ditto's database, and a plain copy of a control text left 1. A pasted snippet did not show in Win+V.

A line break counts as long, because typing sends it as an Enter key press, and a chat application sends the message on Enter.

Measured on 2026-11-15 in Notepad on Windows 11, with 20 lines (1059 characters):

| Method | Time | Result |
| --- | --- | --- |
| Paste | 371 ms | Correct. The old clipboard text came back. |
| Type, 8 characters per call, 10 ms pauses | 2.9 to 3.2 s | Garbled: characters repeated and dropped, emoji broken. Same in the plain text and the formatted mode. |
| Type, 1 character per call, 5 ms pauses (423 characters) | 2.9 s | Garbled. |

Short text, measured the same day with a 36-character line typed by `paste_probe`:

| Pace | Result |
| --- | --- |
| 8 or 2 characters per call, 5 to 10 ms pauses | Garbled, also with `snip` stopped. |
| 1 character per call, 0 or 2 ms pauses | Garbled. |
| 1 character per call, 5 or 20 ms pauses | Correct. 5 ms took 250 ms. |

So the default is 1 character per call with 5 ms pauses. Backspace and Left presses use the same pace. A burst of 8 Backspaces in one call erased more than 8 characters.

Most of the typing time goes into `SendInput`, because each event passes through every low-level hook on the machine. The test machine runs espanso, AutoHotkey and `snip`. The cause of the garbled text is not known. So `--long-text type` is not usable for long snippets in Notepad on the test machine.

## Not tested yet

- Chinese input in the search box through a real input method. The tests typed ASCII only. In the resident mode, an input method composes text before the application gets it, so the hook does not see the composed characters.
- The resident mode with a person typing. All tests typed through `SendInput` with `--allow-injected`.
- The resident mode in browsers, Electron applications, Office, terminals and code editors.
- The espanso `shell` variable path, including how espanso decodes the popup's output.
- Caret lookup in browsers, Electron applications, Office and terminals.
- Monitors with a scale above 100 percent.
- `--long-text type` in applications other than Notepad.
- Slint's software and Skia renderers. The software renderer does not support the first-frame hook, so the benchmark does not work with it.
