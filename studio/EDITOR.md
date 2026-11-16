# Tray icon and snippet editor design

Status: design agreed on 2026-11-16. All four steps built on 2026-11-16. Steps 3 and 4 wait for a test by hand.

Build notes for steps 1 and 2:

- While expansion is paused, the engine gets a reset for each key, so a trigger typed half before the pause does not expand after it.
- "Open configuration folder" and "Open log" call `ShellExecuteW` on a thread of their own. On the test machine, the folder window took about 2.4 s with `explorer.exe <folder>` and about 0.25 s with `ShellExecuteW`, with or without administrator rights.
- If the first load fails, the engine waits for "Reload snippets". If a later load fails, the old snippets stay, and the tooltip shows the error.

Build notes for step 3:

- The code is in `popup-gpui/src/bin/snip/editor.rs`. The form model is `snip_core::matchedit`. It has its own YAML writer, because `serde_norway` writes some multiline strings wrongly. After each change, the writer parses its output again and refuses a change that does not read back the same.
- `snip-core/examples/form_roundtrip.rs` takes every match of the real configuration through the form and back. On the test machine, all 2000 matches came back unchanged.
- The YAML text is the source of truth. The form reads it when the Form tab opens, and writes the changed keys back when the YAML tab opens or on Save. The two views do not sync while you type.
- Delete asks with a second click on the button, not with a dialog.
- The last target file is kept in `%LOCALAPPDATA%\snip\last-file.txt`.
- Keys: Ctrl+S saves, Ctrl+Enter saves and closes, Ctrl+N starts a new snippet, Ctrl+F goes to the search box. Esc closes the window. With unsaved edits, the first Esc or close button shows a warning, and a second one closes without saving.
- When the editor opens a snippet, the cursor goes to the replacement text.
- Tab and Shift+Tab move between the text boxes of the open form tab, and the search box when the list shows. In the YAML box, Tab indents.
- Layout after the first test: the form has two tabs, "Snippet" and "More options". The YAML text is always visible below the tabs. The form and the YAML text stay in sync while you type. If the YAML text is not valid, the form keeps its last state and does not write until the text is valid again.

Build notes for the usability round:

- Unsaved edits: a click on another snippet, New, Ctrl+N, or a request from the popup or the tray shows a bar with Save, Discard and Keep editing. Nothing changes until you pick one.
- The window title shows the trigger of the snippet. A dot in front of the title and on the Save button means unsaved edits.
- Under the triggers, a warning names another snippet that has the same trigger. Prefix triggers are not a conflict, because the snip engine waits for the longer trigger.
- Under the variables, a warning names a `{{name}}` in the text that has no variable in the snippet and no global variable, and a variable that the text and the other variables do not use. Form snippets are not checked.
- Up and Down in the search box open the next or previous snippet of the list. Enter in the search box goes to the replacement text.
- A menu above the list shows only one file. Each menu item shows the number of snippets in the file.
- The window rectangle is kept in `%LOCALAPPDATA%\snip\editor-window.txt` when the window closes with the list shown. From the tray, the editor opens there if the title bar is still on a monitor. Otherwise it opens in the middle of the monitor with the mouse.
- Test renders the YAML as it is now, without a save and without an insert. It writes a temporary folder with a copy of `config/default.yml` and one match file. That file imports every match file, so global variables work, and it holds the snippet with a test label. The engine loads the folder on a background thread. Shell and script variables run. Forms and choice lists are not shown, so such a snippet reports that. The name of the temporary match file must not start with `_`, because espanso skips such files. Each test leaks one loaded configuration, because the engine keeps it for the life of the process.
- Open file opens the match file at the line of the snippet in VS Code when `code` is on the PATH, otherwise in the default program for `.yml` files. snip runs as administrator, so the editor that it starts also runs as administrator.

Build notes for step 4:

- The hook classifies the Ctrl key alone as `Key::Ctrl`, which changes nothing. Ctrl with a letter is `Key::CtrlLetter`. Before this step, the Ctrl key alone closed the popup.
- Right also edits the selected snippet, like Ctrl+E. Both need at least one result. With no result, Right closes the popup and goes to the application. Ctrl with another letter closes the popup, and the key goes to the application.
- The engine thread erases the text with Backspaces first, and then it asks the UI to open the editor. So the Backspaces go to the target application, not to the editor.
- From the popup, the editor opens compact: no list, 760 by 780 logical pixels, with its top left corner where the popup was, on the same monitor. The window stays hidden until it is in place. "Show list" and Ctrl+F bring the list back. From the tray, the editor opens with the list, in the middle of the main monitor.
- The popup knows a snippet only by its triggers and label, not by its file. The editor opens the first snippet in the files with the same triggers, and with the same label if one matches. The popup joins several triggers with two spaces, and the editor compares them the same way. If no snippet matches, the editor shows the list, searched for the first trigger.

`snip` replaces the espanso daemon for daily work (see [EXPANDER.md](EXPANDER.md)). This design adds three parts to it:

1. A tray icon with a menu of common actions.
2. An editor window that creates and edits snippets. It covers every part of an espanso match: several triggers, variables of each type, shell commands, forms, and options.
3. Shortcuts in the `;;` popup that open the editor for the selected snippet or for a new one.

All three parts run in the `snip` process. The editor is a gpui window, like the form window. So the editor can reload the engine after a save, and the keyboard hook already ignores keys while a window of `snip` has the focus.

## Decisions

Agreed on 2026-11-16:

- The editor has two views of one match: a form and the YAML text. The form covers the common fields. The YAML view shows the real text and covers everything else. An edit in one view changes the other view at once.
- A form edit changes only the YAML keys that it touches. Comments, quoting and key order of the other keys stay as they were.
- If you open the editor from the popup, `snip` first erases the popup trigger and the keyword in the target application. For a new snippet, the keyword fills the first trigger.
- The editor writes a new snippet to the file that you picked last time. The first default is `match/base.yml`. A drop-down list in the editor changes the file.

## Order of the work

| Step | Part |
| --- | --- |
| 1 | Reload: the engine loads the configuration again on request. |
| 2 | Tray icon and menu. |
| 3 | Editor window: list, form view, YAML view, save, delete. |
| 4 | Popup shortcuts for edit and new. |

Each step ends with a test in the real program before the next step starts.

## Step 1: reload

Today the engine loads the configuration once, and `ENTRIES` is a `OnceLock`. The editor needs a reload after each save.

- A new message `ToEngine::Reload` makes the engine thread load the configuration again. If the load fails, the old engine stays, and the error goes to the log and to the tray tooltip.
- `ENTRIES` becomes a lock that holds the current list. The popup reads the list again on each open.
- The tray menu item "Reload" sends the same message.

This design does not reload a file automatically after a change on disk.

## Step 2: tray icon

The `tray-icon` crate draws the icon. Its menu crate `muda` is already in the dependency tree through gpui-component. The icon lives on the gpui main thread, which runs the Windows message loop. Menu events arrive on a channel, and the popup's 8 ms tick reads them.

Menu:

| Item | Action |
| --- | --- |
| New snippet | Opens the editor with an empty match. |
| Edit snippets | Opens the editor with the list. A double click on the icon does the same. |
| Pause expansion | A check item. While it is on, the engine ignores keys and the popup does not open. |
| Reload | Sends `ToEngine::Reload`. |
| Open configuration folder | Opens the espanso base folder in Explorer. |
| Open log | Opens the log file in the default text editor. |
| Start at sign-in | A check item. It adds or removes `snip.exe` under the `Run` key of `HKCU`. |
| Restart as administrator | It runs `scripts/win/snip.ps1 -Action start`. The menu hides this item while `snip` has administrator rights. |
| Quit | Ends the process. |

The tooltip shows the number of snippets, or "paused", or the last load error.

Limit: the `Run` key starts `snip` without administrator rights. At sign-in, Windows gives administrator rights to a program only through a scheduled task. A later step can add this task.

## Step 3: editor window

One editor window at a time. If it is open, a new request brings it to the front and loads the requested snippet.

Layout:

- Left: a search box and the snippet list, from `snip_core::Workspace::list_snippets`. A file filter sits above the list.
- Right: the selected match, with two tabs, "Form" and "YAML".
- Bottom: the target file (drop-down list), Save (Ctrl+S), Test, Delete, and a status line.

Form view fields:

| Group | Fields |
| --- | --- |
| Triggers | A list of trigger boxes with add and remove buttons (`trigger` or `triggers`), or one `regex` box. |
| Search | `label`, `search_terms` |
| Output | `replace` (multiline), or `form` with `form_fields`, or `image_path` |
| Options | `word`, `left_word`, `right_word`, `propagate_case`, `uppercase_style`, `force_mode` |
| Variables | A list of variables. Each has a `name`, a `type`, and the boxes for the parameters of that type. |
| Comments | The comment lines directly above the match. |

Parameters per variable type:

| Type | Parameters |
| --- | --- |
| `date` | `format`, `offset`, `locale` |
| `echo` | `echo` |
| `random` | `choices` (one per line) |
| `shell` | `cmd` (multiline), `shell`, `trim`, `debug` |
| `script` | `args` (one per line) |
| `clipboard` | none |
| `choice` | `values` (one per line, or `label` and `id` pairs) |
| `form` | `layout`, `fields` |
| `match` | `trigger` |
| `global` | none |

Every variable also has `depends_on` and `inject_vars`. A key that the form does not know stays in the YAML. The form shows a note: "Other keys: edit them in the YAML tab."

How the two views stay in sync:

- A new module `snip_core::matchedit` reads one match into a form model and writes form changes back as text. It changes only the keys that the form changed. It has unit tests for each field group.
- When the Form tab opens, the editor parses the YAML text. If the text is not valid, the form shows the parse error in place of the fields.

Save, test and delete:

- Save calls `create_snippet` or `update_snippet` of `snip-core`. These functions make sure that espanso's own loader accepts the new file. They also reject a write after a change on disk. After a save, the editor sends `ToEngine::Reload`.
- Test renders the match with the engine and shows the text. It does not insert anything. A `shell` or `script` variable runs its command during the test, so the Test button says so.
- Delete asks you first, and deletes only after you say yes.
- Files under `match/packages/` open read-only.
- The last target file is kept in `%LOCALAPPDATA%\snip\last-file.txt`.

## Step 4: popup shortcuts

While the popup is open, the hook passes two more keys to it:

| Key | Action |
| --- | --- |
| Ctrl+E | Edit the selected snippet. |
| Ctrl+N | New snippet. The first trigger is the popup trigger and the keyword, for example `;;sig`. |

Before the editor opens, the engine thread erases the popup trigger and the keyword with Backspaces, as it does before an insert. The status line of the popup shows both keys. When no snippet matches, it shows "Ctrl+N new snippet".

## Risks

- The writer keeps the text of untouched keys, but a changed block string can come back in another style, such as `|` in place of quotes. The unit tests cover multiline `replace` and `cmd` values.
- The editor window takes the focus. The hook ignores keys while it has the focus, so an expansion does not fire inside the editor. This also means that you cannot test a trigger inside the editor by typing it.
- A save reloads the whole engine. With 1999 matches, a load takes about 60 ms, so this is not a problem today.
