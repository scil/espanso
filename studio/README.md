# Snippet Studio

Snippet Studio is a set of companion tools that edit an espanso configuration folder. The tools never change espanso itself. The espanso daemon watches the folder and reloads changed `.yml` files, so a saved edit is live at once.

This folder is a separate Cargo workspace. It is not a member of the root espanso workspace, so merges from upstream do not touch it. It depends on `../espanso-config` by path, so it parses files exactly the way espanso does.

## Crates

| Crate | Purpose |
| --- | --- |
| `snip-core` | Library: list, read, edit and validate snippets. Rust frontends link it. |
| `snipctl` | Command line tool over `snip-core`. `snipctl serve` speaks JSON-RPC for Go and web frontends. |
| `popup-core`, `popup-slint`, `popup-gpui` | Caret popup prototypes: a snippet search window that opens next to the text caret. `popup-gpui` also builds `snip`, a resident popup that opens on a typed trigger, like an input method. See [POPUP.md](POPUP.md). |

## Guarantees

- An edit changes only the bytes of the edited match. Comments, blank lines, quoting, `CRLF` line ends and a byte order mark elsewhere in the file stay as they were.
- Comment lines directly above a match belong to that match. The editor receives them with the match, and a delete removes them with it.
- Before every write, espanso's own loader reads the new text. If the new text adds an espanso error or warning, nothing is written and the error lists the problems.
- Every update and delete carries the hash of the match text as read. If the file changed on disk since then, the write fails with a conflict error.
- Writes go through a temporary `.snipcore.tmp` file and a rename. espanso's watcher ignores that name.
- Files under `match/packages/` are read-only, because the espanso package manager overwrites them.

## Limits

- `matches:` must be a block list (`- trigger: ...`). A flow list (`matches: [...]`) is reported as unsupported.
- When a match is edited, its whole text is re-indented. A comment line inside the match that sat left of the match's indent moves to the indent. Saving a match without changes writes nothing.
- Configuration files (`config/*.yml`) are written as whole files. Only their YAML syntax is tested.

## Base folder

The tools look for the espanso folder in this order:

1. The `--base <dir>` flag.
2. The `SNIP_BASE` environment variable.
3. The standard folder: `%APPDATA%\espanso` on Windows, `~/.config/espanso` on Linux, `~/Library/Application Support/espanso` on macOS.

## Command line

Every command prints JSON. Errors go to standard error with a non-zero exit code.

```bash
cargo build --release -p snipctl
snipctl info
snipctl files
snipctl list --query sig
snipctl get match/base.yml 3
printf 'trigger: ":hi"\nreplace: "hello"\n' | snipctl create match/base.yml
printf 'trigger: ":hi"\nreplace: "hello!"\n' | snipctl update match/base.yml 3 <hash>
snipctl delete match/base.yml 3 <hash>
snipctl validate
snipctl conflicts
snipctl overlaps --own
```

## JSON-RPC

`snipctl serve` reads one JSON-RPC 2.0 request per line on standard input and writes one response per line on standard output.

| Method | Parameters | Result |
| --- | --- | --- |
| `info` | none | `{base, version}` |
| `files.list` | none | list of `{path, kind, read_only, hash, size, matches, error}` |
| `files.read` | `path` | `{path, text, hash, read_only}` |
| `files.write` | `path`, `text`, `hash` (omit for a new file) | same as `files.read` |
| `snippets.list` | `query`, `file` (both optional) | `{snippets, file_errors}` |
| `snippets.get` | `file`, `index` | snippet with `yaml` |
| `snippets.create` | `file`, `yaml` | the new snippet with `yaml` |
| `snippets.update` | `file`, `index`, `hash`, `yaml` | the updated snippet with `yaml` |
| `snippets.delete` | `file`, `index`, `hash` | `{deleted: true}` |
| `validate` | none | `{problems}` from espanso's loader, plus bad `snip.priority` values, match-level priorities and shared triggers |
| `conflicts` | none | `{conflicts}`: each trigger that several active matches share, winner first, with file, index and priority |
| `overlaps` | `own` (optional, default false) | `{overlaps}`: each pair of triggers where the `short` one is the start of the `long` one, with the matches of each. With `own`, pairs that only involve files under `match/packages/` are left out |

Paths are relative to the base folder, use `/`, and must be `.yml` or `.yaml` files. A file must be under `match/` or `config/`, or espanso must load it through `imports`, `includes` or `extra_includes`. An example is a `_global_vars.yml` in the base folder that `config/default.yml` includes.

Error codes:

| Code | Meaning |
| --- | --- |
| -32001 | Not found |
| -32002 | Conflict: the match changed on disk. `data` holds both hashes. |
| -32003 | Invalid YAML |
| -32004 | Invalid input, such as a path outside the folder |
| -32005 | Unsupported file layout |
| -32006 | Validation failed. `data.problems` lists the new espanso errors. |
| -32007 | Read-only file |
| -32010, -32011 | I/O or other error |

Example:

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"snippets.list","params":{"query":"sig"}}\n' | snipctl serve
```

## Portability

Each snippet carries a `portability` report for export to other programs, such as input methods:

- `static`: plain text, which works anywhere.
- `dynamic`: uses `date` or `random` variables, or a variable from another file. The target needs its own renderer, such as a Rime Lua script.
- `espanso_only`: uses a form, shell, script or clipboard variable, an image, rich text, a regex trigger, or the `$|$` cursor marker.

## Tests

```bash
cargo test
cargo run -p snip-core --example roundtrip -- <espanso base folder>
```

The `roundtrip` example writes nothing. It tests every match in every file. A save without changes must give back the exact file. A delete and an append must give a file that parses.
