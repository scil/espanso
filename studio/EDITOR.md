# Tray icon and snippet editor design

Status: design agreed on 2026-11-16. All four steps built on 2026-11-16. On 2026-11-17 the editor window was removed: the popup edit session is the only editor, also from the tray. See "Build notes for one editor".

Build notes for steps 1 and 2:

- While expansion is paused, the engine gets a reset for each key, so a trigger typed half before the pause does not expand after it.
- "Open configuration folder" and "Open log" call `ShellExecuteW` on a thread of their own. On the test machine, the folder window took about 2.4 s with `explorer.exe <folder>` and about 0.25 s with `ShellExecuteW`, with or without administrator rights.
- If the first load fails, the engine waits for "Reload snippets". If a later load fails, the old snippets stay, and the tooltip shows the error.

Build notes for step 3:

- The code is in `popup-gpui/src/bin/snip/editor.rs`. The form model is `snip_core::matchedit`. It has its own YAML writer, because `serde_norway` writes some multiline strings wrongly. After each change, the writer parses its output again and refuses a change that does not read back the same.
- `snip-core/examples/form_roundtrip.rs` takes every match of the real configuration through the form and back. On the test machine, all 2000 matches came back unchanged.
- The YAML text is the source of truth. When the Form tab opens, the form reads it. When the YAML tab opens, or when you click Save, the form writes the changed keys back. The two views do not sync while you type.
- Delete asks with a second click on the button, not with a dialog.
- The last target file is kept in `%LOCALAPPDATA%\snip\last-file.txt`.
- Keys: Ctrl+S saves, Ctrl+Enter saves and closes, Ctrl+N starts a new snippet, Ctrl+F goes to the popup list, and so does Left with the cursor at the start of a box. Esc closes the form. With unsaved edits, the first Esc shows a warning, and a second one closes without saving.
- When the editor opens a snippet, the cursor goes to the replacement text.
- Tab and Shift+Tab move between the text boxes of the open form tab. This includes the replacement text and the other multi-line boxes: Tab there moves on and does not indent. In the YAML box, Tab indents.
- Layout after the first test: the form has two tabs, "Snippet" and "More options". The YAML text is always visible below the tabs. The form and the YAML text stay in sync while you type. If the YAML text is not valid, the form keeps its last state and does not write until the text is valid again.

Build notes for the usability round:

- Unsaved edits: a click on another snippet, New, Ctrl+N, or a request from the popup or the tray shows a bar with Save, Discard and Keep editing. Nothing changes until you pick one.
- A dot on the Save button means unsaved edits.
- Under the triggers, a warning names another snippet that has the same trigger. Prefix triggers are not a conflict, because the snip engine waits for the longer trigger.
- A variable bar is the top line of the Variables row. It shows the snippet's variables and the global variables as chips. A chip is a small button that inserts `{{name}}` at the caret, or over the selected text. A chip of a variable that the text does not use yet is dimmed, and its tooltip gives the type and what the type does. With more than six variables, one "Variables" menu replaces the chips, in two groups: this snippet and global. The New menu lists every type with one line of help. A pick adds a variable with a free name, such as `date1`, and inserts it. A new date variable gets the format `%Y-%m-%d`. Form and image outputs take no `{{name}}`, so their bar has only New, and New only adds the variable.
- The global variables come from espanso's own loader. It follows `imports`, `includes` and `extra_includes`, so a file outside `match/` counts too, for example `_global_vars.yml` that `config/default.yml` includes. The list joins the global variables of every configuration.
- Typing `{{` in the replacement text opens a picker under the text box. It lists the variables whose name contains the typed name. Names that start with it come first. Then it offers a new variable of each type that starts with it. Up and Down move, Enter or Tab inserts, and Esc closes it until the caret leaves that `{{`. A `}}` right after the caret is replaced too.
- Under the variables, a warning names two problems. One is a `{{name}}` in the text with no variable in the snippet and no global variable. The other is a variable that the text and the other variables do not use. Form snippets are not checked.
- Test renders the YAML as it is now, without a save and without an insert. It writes a temporary folder with a copy of `config/default.yml` and one match file. That file imports every match file, so global variables work, and it holds the snippet with a test label. The engine loads the folder on a background thread. Shell and script variables run. Forms and choice lists are not shown, so such a snippet reports that. The name of the temporary match file must not start with `_`, because espanso skips such files. Each test leaks one loaded configuration, because the engine keeps it for the life of the process.
- Open file opens the match file at the line of the snippet. If `code` is on the PATH, VS Code opens it. Otherwise the default program for `.yml` files opens it. snip runs as administrator, so the editor that it starts also runs as administrator.

Build notes for step 4:

- The hook classifies the Ctrl key alone as `Key::Ctrl`, which changes nothing. Ctrl with a letter is `Key::CtrlLetter`. Before this step, the Ctrl key alone closed the popup.
- Right also edits the selected snippet, like Ctrl+E. Both need at least one result. With no result, Right closes the popup and goes to the application. Ctrl with another letter closes the popup, and the key goes to the application.
- The engine thread erases the text with Backspaces first, and then it asks the UI to open the editor. So the Backspaces go to the target application, not to the editor.
- From the popup, the popup window itself becomes the editor. The popup keeps its list, with the same rows and style, where it was. The window grows to the right by 700 logical pixels and to 760 logical pixels high, and the form fills the new space. At the edge of the work area, the window moves left or up. The popup then takes focus: the hook ignores input while a `snip` window has focus, so keys and clicks go to the form. A click on a row, or Alt+Up and Alt+Down, opens that row's snippet in the form. A right click on a row also opens it. The form has no list, no search box and no "Show list" button of its own: the popup list does that work. The list keeps its top-left corner, so it does not move when the form opens. The form fades in and slides 16 pixels in from the list over 120 ms. The window size does not animate, because a stepped resize of a gpui window flickers.
- The popup calls this state the edit session (a name of this project). The popup list is part of it. Ctrl+F in the form, Left with the cursor at the start of a box, or a click beside the rows, gives the list the focus, and its right edge turns to the accent color. With the focus in the list, the keys work as in the popup: type to search, Backspace, Up and Down, Tab as the `tab` setting says, Ctrl+1 to Ctrl+9 to insert, Alt+1 to Alt+9 to select, Ctrl+P, Caps Lock and Ctrl+Caps Lock. Up and Down also show the row in the form, but the list keeps the focus. Right, Ctrl+E or Enter then puts the cursor in the form. The insert keys work also with the form open, unless the form has unsaved edits. The hook handles Caps Lock and Ctrl+Caps Lock there, so the capitals mode does not change.
- Tab and Shift+Tab move between the boxes of the form. A key binding cannot do this. A multi-line text box binds Tab to indent, and that binding is the deepest one. The popup window also has the gpui-kit `Root`, which binds Tab to its own focus order. So `editor::init` registers a keystroke interceptor, which gpui runs before all key bindings. It acts only inside the `SnipEditor` key context and only when the focus is in a box of the Tab order. Elsewhere, such as the YAML box, Tab works as before.
- Esc in the form, or in the list while the form shows, closes the form. With unsaved edits, the first Esc only warns. The popup then shows only its list, where it was, and keeps the focus. In this list-only mode, Enter, a click or Ctrl+1 to Ctrl+9 inserts a snippet: the window that had focus before gets it back, and the engine types the text. Ctrl+E, Right or a right click opens the form again, and Ctrl+N opens it with a new snippet. Esc in the list-only mode ends the edit session.
- Ctrl+Enter after a save ends the edit session. The popup hides, takes no focus again, and gives focus back to the window that had it before. A click into another window also ends the edit session, unless the form holds unsaved edits: then the form stays, like a normal window. While the form holds unsaved edits, a new `,,` session shows no popup, and a pick list gets no answer.
- In the edit session, a header row sits above the list. It has the same height as the tab row of the form, so the two halves line up. The header holds the file button. A click on it, or Ctrl+O anywhere in the session, shows the match files in the list, each with its number of snippets. Type to search the file names, then pick a file with Enter or a click. "All files" removes the filter. Esc or Ctrl+O again goes back to the snippets and keeps the filter. A popup list is used here in place of a drop-down menu, because a key cannot open the gpui-kit menu.
- The file filter ends with the edit session. The next `,,` popup searches all files again. The normal popup has no header, because it never has focus.
- In the edit session, the status line always shows the search on the left and the next Esc step on the right: "Esc: back to snippets", "Esc: list only" or "Esc: close".
- The engine does not know the file of a match. After each load, the engine thread reads the match files with `snip-core` and gives each engine entry the file of the first unused snippet with the same triggers and label (`popup_core::data::assign_files`). An entry with no such snippet has no file, so it shows only under "All files".
- The popup knows a snippet only by its triggers and label, not by its file. The editor opens the first snippet in the files with the same triggers. If a snippet also has the same label, the editor opens that one. The popup joins several triggers with two spaces, and the editor compares them the same way. If no snippet matches, the form shows an error: reload, then pick the snippet again.

Build notes for one editor (2026-11-17):

- The editor window is gone. The popup edit session is the only editor. `editor.rs` keeps only the form: no list, no search box, no file menu, no window title and no saved window rectangle. The file `%LOCALAPPDATA%\snip\editor-window.txt` is no longer read or written.
- New snippet, Edit snippets and a double click on the tray icon open the edit session in the middle of the monitor with the mouse, with the list on the left and the form on the right. New snippet puts the cursor in the form. Edit snippets shows the first snippet of the list and gives the list the focus, so you can type to search at once.
- A tray request ends an open `,,` popup or pick list first. If the edit session is open already, the request shows in it, and the window comes to the front.
- A session from the tray has no window that waits for text. So in the list-only mode, Enter, a click or Ctrl+1 to Ctrl+9 opens the snippet in the form instead of inserting it.

`snip` replaces the espanso daemon for daily work (see [EXPANDER.md](EXPANDER.md)). This design adds three parts to it:

1. A tray icon with a menu of common actions.
2. An editor that creates and edits snippets. It covers every part of an espanso match: several triggers, variables of each type, shell commands, forms, and options.
3. Shortcuts in the `,,` popup that open the editor for the selected snippet or for a new one.

All three parts run in the `snip` process. The editor is a gpui view in the popup window. So the editor can reload the engine after a save, and the keyboard hook already ignores keys while a window of `snip` has the focus.

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
| 3 | Editor: list, form view, YAML view, save, delete. Since 2026-11-17 it lives only in the popup. |
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
| New snippet | Opens the popup edit session with an empty match. |
| Edit snippets | Opens the popup edit session with the list and the first snippet. A double click on the icon does the same. |
| Pause expansion | A check item. While it is on, the engine ignores keys and the popup does not open. |
| Reload | Sends `ToEngine::Reload`. |
| Open configuration folder | Opens the espanso base folder in Explorer. |
| Open log | Opens the log file in the default text editor. |
| Start at sign-in | A check item. It adds or removes `snip.exe` under the `Run` key of `HKCU`, and an "enabled" entry for it under `Explorer\StartupApproved\Run`. Recent Windows 11 builds skip a `Run` value that has no such entry. If both exist and the entry says enabled, the item is checked. Otherwise it is clear, for example after a switch-off in Task Manager. |
| Settings... | Opens the edit session with the settings panel in the right half. See [Settings panel](POPUP.md#settings-panel). |
| Restart as administrator | It runs `scripts/win/snip.ps1 -Action start`. The menu hides this item while `snip` has administrator rights. |
| Quit | Ends the process. |

The tooltip shows the number of snippets, or "paused", or the last load error.

Limit: the `Run` key starts `snip` without administrator rights. At sign-in, Windows gives administrator rights to a program only through a scheduled task. A later step can add this task.

## Step 3: editor window

This section is the first design. Since 2026-11-17 the editor has no window of its own: the popup list replaces the left side. See "Build notes for one editor".

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
| Right click on a row | Edit that snippet. A pick list ignores it. |
| Ctrl+N | New snippet. The first trigger is the popup trigger and the keyword, for example `,,sig`. |
| Alt+Up, Alt+Down | In the edit session: select the row above or below. An open form shows it. |
| Ctrl+F (in the form) | Give the popup list the focus. Type to search there. |
| Left at the start of a box (in the form) | Give the popup list the focus, as Ctrl+F does. |
| Esc (in the form) | Close the form. The popup shows only its list, with focus. Esc again ends the edit session. |
| Ctrl+O | In the edit session: show the match files in the list, to filter by one file. Ctrl+O or Esc goes back. |

Before the editor opens, the engine thread erases the popup trigger and the keyword with Backspaces, as it does before an insert. The normal popup shows no key help. Its bottom line only shows a note, "no match", or the filter of a pick list. In the edit session, the bottom line also shows what Esc does next.

## Risks

- The writer keeps the text of untouched keys. A changed block string can come back in another style, for example `|` in place of quotes. The unit tests cover multiline `replace` and `cmd` values.
- The editor window takes the focus. The hook ignores keys while it has the focus, so an expansion does not fire inside the editor. This also means that you cannot test a trigger inside the editor by typing it.
- A save reloads the whole engine. With 1999 matches, a load takes about 60 ms, so this is not a problem today.
