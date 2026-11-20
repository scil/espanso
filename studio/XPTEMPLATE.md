# XPTemplate features as an upgrade direction

Status: research note, written on 2026-11-16. Step 1 of the order of work (priority and conflict report) was built on 2026-11-16. See [Priority](#priority).

[XPTemplate](https://github.com/drmingdrmer/xptemplate) is a snippet engine for Vim. It has a full snippet model: placeholders, nested snippets, repeated parts, priorities and inheritance. This note lists its features, compares each one with espanso and `snip`, and proposes an order of work for `snip`.

Sources: the XPTemplate README and the files `doc/xptemplate.txt`, `doc/xpt.snippet.syn.txt`, `doc/xpt.snippet.tutor.txt`, `doc/xpt.api.txt`, `doc/xpt-function.txt` and `doc/xpt/*.txt`. The help file says "Last change: 2009 Nov 08", so some details can differ from the current code.

## The main difference

XPTemplate lives inside the editor. It inserts the snippet text first, then keeps a session open on the live text. Tab moves between placeholders in the buffer, and the engine changes the buffer as you type.

espanso and `snip` work from outside the target program. They send keys or paste text into another window, and they cannot read or change that text after the insert. So an XPTemplate feature that edits the buffer after the insert cannot move over as it is.

The form window of `snip` is the place for these features. All editing happens in the form window before the insert. The target program receives the finished text once.

This note uses one coined term:

- inline form (coined in this note, no industry term): a form window that shows the whole snippet text, with an input box in place of each placeholder. It is the form-window version of an XPTemplate placeholder session. The `snip` form already shows a preview under the fields. An inline form merges the preview and the fields into one view.

## Feature catalog and comparison

Each row gives the XPTemplate feature, what espanso and `snip` have today, and the proposed direction.

### Placeholders

| XPTemplate | Today in espanso and snip | Direction |
| --- | --- | --- |
| Placeholder `` `name^ `` with a default value | A form field `[[name]]` with `default` | Same model. Keep. |
| Mirrored placeholders: the same name twice updates both | A form field is used once in the layout. The snippet text can use `{{form1.name}}` many times. | Allow `{{form1.name}}` many times and show each copy live in the inline form. |
| Leading placeholder (``` ``i^ ``` picks the copy that you edit) | None | Not needed. In a form, the input box is the only copy that you edit. |
| Edges `` `(`xpt`)^ ``: text kept around a placeholder, removed with it | None | Add optional edges, so an empty field removes its comma, space or line break. |
| Optional placeholder `` `name?^ ``: Tab on an unchanged value clears it | None | Add an `optional` field flag. An unchanged optional field becomes empty, and its edges go with it. |
| `` `cursor^ ``: the end point of the snippet | `$|$` cursor hint | Same. Keep. |
| Instant value `` `strftime(...)^ ``: computed once, not editable | `date`, `shell`, `script` and other variables | Same. Keep. |
| Preset value `XSET name|pre=` and default from an expression `XSET name|def=` | `default` is a fixed string | Let a field default come from another variable, for example from `clipboard` or `date`. |
| Ontime filter: computes a value on each key press | The form preview updates on each key press | Add live transforms, for example a second field that shows the first one in upper case. |
| Post filter `` `text^UpperCase(V())^^ ``: runs when you leave the field | `shell` and `script` can transform values after submit | Add a small set of built-in transforms (upper, lower, title, snake, camel, trim) that need no shell. |

### Navigation

| XPTemplate | Today | Direction |
| --- | --- | --- |
| Tab and Shift+Tab move between placeholders | Tab and Shift+Tab move between form fields | Same. Keep. |
| Enter clears the field and moves on | Enter submits the form | Keep Enter as submit. Add a key that clears the field and moves on. |
| `XSET ComeFirst=` and `XSET ComeLast=` change the jump order | Fields follow layout order | Add a field `order`, so the jump order can differ from the text order. |
| Highlights for the current and next placeholders | The focused field has a frame | Enough for now. |

### Nested snippets

| XPTemplate | Today | Direction |
| --- | --- | --- |
| Trigger a snippet inside a placeholder. Tab returns to the outer snippet. | The keyboard hook ignores keys while a `snip` window has the focus, so a trigger typed in a form field does nothing. A `match` variable inserts a fixed inner snippet. | Let a form field expand triggers. If the inner snippet has a form, push it on a stack: its fields join the inline form, and Tab moves on to the outer fields after the inner ones. |
| `XPTinfo()` shows the stack of active snippets | None | Show the stack in the form title, for example `switch > if`. |

### Repetition

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `` `...^ `` repeated part: Tab adds one more copy, Enter stops | None | Add a repeated group in a form, for example one `case` block or one function argument. A key adds a copy, and an empty last copy ends the group. |
| `` `else...{{^ ... `}}^ ``: an optional block that you add on demand | None | Same mechanism as a repeated group with at most one copy. |

### Wrapping

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `wrap=name`: the selected text goes into one placeholder | espanso has `{{clipboard}}`, but you copy the text first | Add a wrap trigger in `snip`: it copies the selection, opens the popup, and puts the text into the field named by `wrap`. |
| `wraponly`: the snippet shows only in wrap mode | None | Hide such snippets from the normal popup. |

### Names, matching and the popup

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `hint=`: a description in the popup | `label` | Same. Keep. |
| `synonym=a|b`: more names for one snippet | `triggers:` list and `search_terms` | Same. Keep. |
| `alias=`: a copy of a snippet with other keys | A `match` variable can reuse a snippet | Add a short form: a match that names a base match and changes some fields. |
| `hidden`, and names that start with `_`: snippets used only by other snippets | None. Every match with a trigger can expand. | Add a `hidden` flag. A hidden match has no trigger in the popup, but other snippets can use it. |
| `abbr`: expand on Space instead of on the trigger key | `word: true` | Same. Keep. |
| Popup of all snippets for a prefix | The `,,` popup with a search box | `snip` is stronger here: it has substring search. Keep. |
| `g:xptemplate_minimal_prefix` | None | Not needed. |
| `syn=comment`: the snippet works only in one syntax group | `filter_exec` and `filter_title` per program | A text expander cannot see the syntax under the caret. Program and window title filters are the right level. Keep. |

### Priority

XPTemplate gives each snippet file a priority: `personal` 0, `sub` 8, `lang` 16, `like` 32, `spec` 48, `all` 64. A lower number wins. A file can add an offset, for example `like+4`. When two snippets have the same name, the one with the lower number wins. Variables follow the same rule.

espanso has no priority setting. If two active matches share a trigger, espanso shows a list and asks which one to use.

Built on 2026-11-16 (`snip-core/src/priority.rs`):

- A match file sets a priority with a top-level `snip:` key. All matches of the file share it, as in XPTemplate. To give one match another priority, move it to another file. A `snip.priority` on a single match has no effect, and `snipctl validate` shows a `warning` for it.
- A value is a number, or `personal` (0), `normal` (16) or `package` (64), with an optional `+n` or `-n`. A lower number wins.
- Without a value, files under `match/packages/` get `package` and all other files get `normal`.
- When active matches share a trigger, the `snip` engine takes the one with the lowest number. This also applies after the wait for a longer trigger.
- If two or more matches share the lowest number (a tie), `snip` shows a list of them near the caret, as espanso does. The list opens in the popup, the same window as `,,` and `choice` variables. Each line is the match `label`, or else the first line of its text. A repeated line gets ` (2)`, ` (3)`. Weaker matches are not in the list. Esc cancels the list and leaves the trigger in the text.
- `snipctl conflicts` lists each shared trigger with the winner first. `snipctl validate` adds one `info` line for each. A bad priority value is a `warning`.
- espanso ignores the `snip:` keys. A test makes sure that espanso's loader reports no problem for them.

```yaml
snip:
  priority: personal
matches:
  - trigger: ",,sig"
    replace: "Best regards"
```

How a match finds its file: espanso does not keep the source file of a match. A query on one file also returns the matches of the files that it imports. So a match belongs to the smallest file query that holds it. If two files have the same query, the file whose items hold the match's triggers wins. The position in the file comes from the order of the file's own matches, checked against each item's triggers.

Not built:

- Extra priority for the files of a program-specific configuration. Its match files can set a priority themselves.
- A popup that shows the losers of a conflict.

Result on your folder on 2026-11-16: 10 shared triggers, all ties. Three are in `match/packages/actually-all-emojis`. Seven are in your own files under `_prompts/` and `_cli/`: `;;exp`, `exp;;`, `;;analogy`, `;;doc`, `;;prompts`, `;;sed` and `;;sysproxy`. In espanso and in `snip` these show a list.

### Inheritance and inclusion

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `XPTinclude`: load other snippet files into this file | `imports:` in a match file, `extra_includes` in a configuration | Same. Keep. |
| `XPTembed`: snippets of one language inside another, for example JavaScript in HTML | Per-program configurations | Not needed outside an editor. |
| `` `:name:^ `` and `` `Include:name^ ``: insert another snippet in a placeholder, with its fields | A `match` variable inserts the rendered text of another match. Its form fields do not join the outer form. | Let an included snippet bring its fields into the outer inline form. |
| `` `:name({'ph':'newph'}):^ ``: pass values to an included snippet | None | Let a `match` variable set field defaults of the inner snippet. |
| An included snippet brings its filters and defaults, unless the outer snippet changes them | None | Follows from the two rows above. |

### Variables and style

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `XPTvar $author`, `g:xptemplate_vars` for personal values | `global_vars` | Same. Keep. |
| `$BR*` brace style and `$SP*` space style | None | Not needed for general text. Global variables can hold such values. |
| `$DATE_FMT` and other format variables | `format` per `date` variable | Same. Keep. |
| Variable priority, so a personal file can change one value for one language | One `global_vars` list per configuration | Follows from the priority work. |

### Functions

XPTemplate has built-in functions that a snippet calls in a placeholder: `Echo`, `Build`, `BuildSnippet`, `S()` (substitute), `V()` (the typed value), `R()` (another field by name), `N()`, `C()` and `E()`. You can also add your own functions in Vim script.

espanso has `shell` and `script` variables for the same jobs. They start a program, so they are slow for a transform on each key press.

Direction: add a small set of built-in transforms that run in-process, and keep `shell` and `script` for heavy work. A transform can read another field (as `R()` does) and the current value (as `V()` does).

### Tools

| XPTemplate | Today | Direction |
| --- | --- | --- |
| `:XPTreload` after a change | Automatic reload in `snip` | `snip` is stronger. Keep. |
| Debug log `g:xptemplate_debug_log` | `snip` has a log file | Same. Keep. |
| `g:xptemplate_cwd_snippet`: snippets from the current folder | None | Not needed. |

## Proposed order of work

The order puts the features that change daily typing first, and the features that need a new form model later.

1. Priority and conflict report. A match file and a match get a priority. The engine picks the strongest match. `snipctl validate` and the popup show conflicts. This change needs no new form model.
2. Built-in transforms. Upper, lower, title, snake, camel and trim, used on a field or a variable. They run in-process.
3. Inline form. One view that shows the snippet text with input boxes in place. Fields used many times update live. An empty optional field removes its text and its edges.
4. Field order and computed defaults. `order` changes the Tab order. A default can come from another variable, for example the clipboard.
5. Wrap trigger. A key copies the selection and opens the popup. The chosen snippet receives the text in its `wrap` field.
6. Nested snippets. A trigger typed in a form field expands. The inner form joins the outer inline form as a stack.
7. Included snippets with fields, and `hidden` base snippets with parameters.
8. Repeated groups. A group of fields that you can add again and again.

## Where the new keys go

espanso reads each match with a fixed list of keys. The match struct in `espanso-config/src/matches/group/loader/yaml/parse.rs` has no `deny_unknown_fields`, so espanso ignores an unknown key. Before you rely on this, make sure that the `snip-core` validation shows no new espanso warning for such a key.

Proposal: put all new keys under one key, `snip:`, in a match. espanso ignores it, and the same files still work in espanso. Example:

```yaml
- trigger: ",,fn"
  replace: "{{form1.name}}({{form1.args}})"
  snip:
    priority: personal
    fields:
      args: { optional: true, edges: ["", ""] }
      name: { transform: snake }
```

A snippet that uses a `snip:` feature still expands in espanso, but without that feature.

## Open questions

- Is the inline form a new window type, or a change of the current form window?
- Must snippets with `snip:` keys stay readable by the espanso daemon, or can `snip` replace it fully?
