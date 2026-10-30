# rustrewrite

Recipe-driven, verified refactoring for Rust: the OpenRewrite idea, built for a language that has no LST.

You describe a rewrite once as a recipe. The tool finds every match across the codebase, applies the rewrite, then compiles the result and puts everything back if the build breaks.
A rewrite that does not compile is not a rewrite, it is a broken build, and `rustrewrite` refuses to leave one behind.

```
rustrewrite check --recipe rust.idioms          # what would change, and where
rustrewrite run   --recipe rust.idioms --apply  # change it, then prove it still builds
rustrewrite run   --recipe rust.idioms --apply --test   # ...and run the tests too
```

## Install

```
cargo install --path .
```

The binary is self-contained: the recipe catalog is compiled into it.

## How it maps onto OpenRewrite

| OpenRewrite | rustrewrite |
| --- | --- |
| Recipe | a YAML file with an id, tags, risk class and steps |
| Declarative recipe | `steps:` of structural rules, no code required |
| Composite recipe | `recipes:` naming other recipe ids |
| `RecipeRun` / dry run | `check` |
| `RewriteRun` | `run --apply` |
| Scan recipe | `mode: scan`, reports without writing |
| LST | tree-sitter parse of the file, edited in place so formatting and comments survive |
| Recipe tests | `examples:` per recipe, executed by `rustrewrite test` |

The one deliberate difference: OpenRewrite assumes type attribution everywhere. Rust recipes here are either syntax-scoped, where tree-sitter proves the match, or analyzer-backed, where rust-analyzer resolves the symbol. The compile gate catches what neither proves.
That trade is why the gate is mandatory rather than optional.

## Commands

| Command | What it does |
| --- | --- |
| `list [--tag T]` | The catalog. |
| `explain <recipe>` | One recipe, with its examples executed as a live proof. |
| `test` | Run every recipe example. Use this before trusting a new recipe. |
| `check [--recipe R] [--diff] [--json]` | Report findings. Exits 1 if there are any, so CI can hold the line. Never writes. |
| `run [--recipe R] [--apply] [--test] [--diff] [--json]` | Apply a recipe, then gate it. Without `--apply` it is a dry run. |
| `rename --at file:line:col --to NAME [--apply] [--json]` | Rename one resolved symbol using rust-analyzer's type information. |
| `serve [--status] [--stop] [--idle SECS]` | Keep a warm analyzer for this workspace, inspect it, or stop it. |
| `verify [--test]` | The gate on its own: format, compile, optionally test. |

Recipes that take parameters accept `--set key=value`, repeatable, on `check` and `run`.

`--recipe` accepts an id, a namespace prefix (`rust.idiom` selects the whole pack), or `all`.

## The desktop app

`rustrewrite-gui` is a desktop front end built on egui/eframe. It is a view over the same CLI: every check and apply runs `rustrewrite` as a subprocess and reads its JSON, so the gate, the recipes and the rewrite engine stay in one place.

```
cargo run -p rustrewrite-gui
```

The app needs a `rustrewrite` binary. It looks on `PATH` first; if you run it from a checkout without installing, open **Settings** and point it at `target/debug/rustrewrite` (build it with `cargo build`).

The window has five working areas:

- **Top bar** — the workspace root, with **Browse…** for a folder picker and **Load catalog** to fetch the recipe list. The search box filters the list, **Select all shown** checks every recipe the filter currently shows, and **Clear** unchecks them all. **Settings** holds the binary path.
- **Recipe list** — every catalog entry with its id, kind and risk. The search matches the id, name, tags and description. Each row carries a checkbox and a label: the checkbox decides what the actions run, the label opens that recipe in the detail pane, and neither moves the other. A recipe that declares a required option is tagged `needs --set`, and its checkbox is disabled.
- **Detail** — the inspected recipe's name, description, tags and options. Required options are marked `*`, and declared defaults are filled in. What you type becomes `--set key=value`.
- **Actions** — **Check** is the dry run and **Apply** writes and then runs the correctness gate. With several recipes checked the buttons read **Check 3 recipes** / **Apply 3 recipes**, and the run covers all of them. The toggles map to `--test`, `--no-verify`, `--allow-dirty`, and the warm analyzer (off passes `--cold`). One job runs at a time; **Cancel** kills it.
- **Results** — a findings table (`file:line`, recipe, `before → after`), the unified diff for the selected file with added and removed lines coloured, the stderr log, and a summary naming the selectors that ran, the counts, whether changes were kept, and every gate result. A failure or an `error` record appears as a red banner.

### Selecting several recipes

Checking more than one recipe turns the actions into a batch: the app builds **one** command with one `--recipe` per checked recipe, in catalog order, and the CLI scans once and gates once at the end.

```
rustrewrite --path <root> run --apply --json --diff --recipe A --recipe B --recipe C
```

With nothing checked, **Check** and **Apply** act on the inspected recipe exactly as before. With exactly one checked, they act on that recipe. Options belong to a single recipe, so while a batch is checked the option fields are replaced with a note saying so; uncheck back to one recipe and its options return.

A recipe that declares a **required** option cannot join a batch, because every recipe in one invocation shares a single `--set` map and their required option names would collide. Such a recipe is tagged `needs --set` in the list, **Select all shown** skips it, and if one ends up checked anyway the app refuses the batch with a message naming the recipe rather than silently dropping it. Run it on its own instead.

A `check` that reports findings exits non-zero by design; the app treats its summary as a completed check rather than a failure.

The app does not write files itself. A rewrite happens only through `run --apply` in the CLI, so a failed gate still rolls the tree back.

## Machine-readable output

`check`, `run` and `rename` accept `--json`. In that mode stdout carries newline-delimited JSON and nothing else — diffs, notes, progress and gate output all move to stderr — so the stream can be piped straight into `jq`.

Every line is one object, and `type` says which kind it is. Readers can skip a type they do not know, so new ones are additive.

```json
{"type":"finding","file":"src/main.rs","line":10,"column":8,"recipe":"rust.idiom.string-len-zero","before":"items.len() == 0","after":"items.is_empty()"}
{"type":"summary","command":"check","recipe":"rust.idioms","findings":6,"files":1,"changed":true,"applied":false}
{"type":"error","message":"unknown recipe `rust.nope` ..."}
```

`finding` is one reported change or hazard:

| Field | Meaning |
| --- | --- |
| `file` | Path relative to `--path`, with forward slashes on every platform. |
| `line`, `column` | 1-based position of the match. |
| `recipe` | The recipe id, or `null` for `rename`, which is positional rather than recipe-driven. |
| `before` | The text that matched. Empty for an `insert` step, which replaces nothing. |
| `after` | The replacement or the inserted text, `null` for a scan finding that only reports, and `""` for a deletion. |

`summary` closes a successful command:

| Field | Meaning |
| --- | --- |
| `command` | `check`, `run` or `rename`. |
| `recipe` | The selector as written (id, prefix or `all`), or `null` for `rename`. |
| `findings` | Number of `finding` lines in this stream. |
| `files` | Number of distinct files those findings touch. |
| `changed` | Whether anything would change (`check`, a dry run) or did change (`--apply`). |
| `applied` | Whether the changes were written and kept. A dry run, and an apply that the gate rolled back, are both `false`. |
| `gates` | Present only when the correctness gate actually ran: `[{"name":"format","ok":true},{"name":"compile","ok":true}]`. An absent key means no gate ran, which is different from a gate that passed. |

A successful command ends with its summary. A failed one ends with `error` as the last line and exits non-zero; `check` also exits 1 when it reports findings, and still ends with its summary.

```console
$ rustrewrite check --recipe rust.idioms --json \
  | jq -r 'select(.type == "finding") | "\(.file):\(.line):\(.column) \(.recipe)"'
src/main.rs:10:8 rust.idiom.string-len-zero
src/main.rs:18:8 rust.idiom.bool-compare
src/main.rs:21:8 rust.idiom.bool-compare
...

$ rustrewrite run --recipe rust.idioms --apply --json \
  | jq 'select(.type == "summary") | {changed, applied, gates}'
{
  "changed": true,
  "applied": true,
  "gates": [
    { "name": "format", "ok": true },
    { "name": "compile", "ok": true }
  ]
}
```

## Type-aware renames

Recipes match syntax, and syntax cannot tell a `Vec::len` from a `MyBag::len`. When the change has to hit exactly one of them, ask the compiler-backed server instead:

```
rustrewrite rename --at src/server.rs:42:8 --to spawn          # report
rustrewrite rename --at src/server.rs:42:8 --to spawn --apply  # rename, then gate it
```

It starts `rust-analyzer`, waits for the workspace to load, and asks for a rename at that position. The server returns the edits, this tool applies them, and the compile gate still decides whether they stay. A dry run shows the diff and writes nothing.

What it buys you, demonstrated in `tests/semantic.rs`: with `Vec::len` and a custom `Bag::len` in the same file, renaming `Bag::len` touches the declaration and its one call and leaves `Vec::len` untouched.

Requirements and cost:

- `rust-analyzer` on `PATH`. Install with `rustup component add rust-analyzer` — and for a project that pins a toolchain in `rust-toolchain.toml`, add it for that toolchain too.
- The first request pays for loading the workspace: about 2 s on a small crate, about 17 s on a 35k-line workspace. `--wait` sets the budget (default 180 s).
- The server may refuse a rename. A trait implementation member cannot be renamed, because the trait requires the name. That is reported as a refusal and nothing is written.

### The warm server

Loading a workspace is the whole cost, and it is paid once per server rather than once per command. The first semantic command on a workspace starts a server in the background; later commands attach to it and skip the load:

```
rustrewrite rename --at cli/src/exit.rs:112:12 --to status_code   # 17.1 s, starts the server
rustrewrite rename --at cli/src/exit.rs:112:12 --to status_code   #  0.26 s, same process
rustrewrite serve --status                                        # who is serving, and how many requests
rustrewrite serve --stop
```

Those two numbers are the same rename on a 35k-line workspace. A recipe run goes from tens of seconds to about one.

- Semantic commands start one automatically. Structural-only commands never start an analyzer at all.
- The server is keyed by workspace root, so two projects get two servers.
- It exits after 15 minutes without a request; `serve --idle SECS` changes that when started by hand, and `0` keeps it alive.
- `--cold` on `check`, `run` and `rename` bypasses it and starts a one-shot analyzer, which is what you want in CI or when comparing timings.
- `RUSTREWRITE_STATE_DIR` relocates the small state file that names the server's port. `RUSTREWRITE_TRACE=1` makes a foreground `serve` log every request it answers.
- One client at a time is served; a second command waits its turn instead of starting a second analyzer.

### As recipes

The same capability is available as parameterized recipes, which is what makes it composable with the rest of the catalog:

```
rustrewrite check --recipe rust.migrate.change-method-name \
  --set file=src/server.rs \
  --set 'anchor=fn start(&self)' \
  --set from=start --set to=spawn

rustrewrite run --recipe rust.migrate.change-type \
  --set file=src/graph.rs --set 'anchor=struct Bag' \
  --set from=Bag --set to=Sack --apply
```

`anchor` is text on the declaration line. It replaces a line number because any edit above the declaration moves a line number, and an anchor does not.

These are the Rust forms of OpenRewrite's `ChangeMethodName` and `ChangeType`: one reusable primitive, configured per call, rather than a hand-written recipe per migration.

### Renaming any resolved symbol

`rust.migrate.rename-symbol` is the recipe form of `rename --at`, so a resolved rename can compose with other steps:

```
rustrewrite run --recipe rust.migrate.rename-symbol \
  --set file=src/limits.rs --set 'anchor=pub const MAX: u32 = 100' \
  --set from=MAX --set to=CEILING --apply
```

It works for constants, statics, fields and functions. Because the analyzer resolves the symbol, a `Config::MAX` on another type is a different symbol and keeps its name, which no text replacement can promise. It is the Rust form of `ReplaceConstantWithAnotherConstant` when the migration is a rename: the old name becomes the new one at its declaration and everywhere it is used.

### Changing a return type

```
rustrewrite run --recipe rust.migrate.change-return-type \
  --set file=src/version.rs --set 'anchor=fn major(&self) -> u8' \
  --set from=major --set to=u64 --apply
```

It edits the declaration exactly: an existing `-> Old` is replaced in place, and a signature with no arrow gets one inserted before the body or the `where` clause. Multi-line return types, generics and `where` clauses keep their place.

Call sites are deliberately not rewritten. Widening a return type is only safe where the compiler says it is, and the way to find that out is to compile: if any caller relied on the old type, the correctness gate rolls the whole run back and the recipe reports a failed run. That is the correct outcome, not a hidden failure. A recipe that guessed at caller fixes would be a recipe that silently changes behaviour.

### Changing a function's visibility

```
rustrewrite run --recipe rust.migrate.change-visibility \
  --set file=src/lib.rs --set 'anchor=fn helper()' \
  --set from=helper --set to=pub(crate) --apply
```

The four accepted forms are `pub`, `pub(crate)`, `pub(super)` and `pub(in crate::module)`. An existing modifier is replaced in place. A private declaration gets one inserted before `async`, `const`, `unsafe`, `extern` and `fn`, which is the order Rust accepts. `--set to=` with an empty value removes the modifier and makes the function private.

`to` is required, so leaving it out is an error; an empty value is a deliberate request for privacy. The declaration is the only edit, and the correctness gate decides whether the result still compiles.

### Rewriting call sites

Changing an argument list is not one symbol, it is every call plus the declaration, and getting one wrong stops the crate compiling. Three recipes handle that family:

```
rustrewrite run --recipe rust.migrate.delete-argument \
  --set file=src/main.rs --set 'anchor=fn record(scope: &str' \
  --set from=record --set index=0 --apply
```

| Recipe | `--set` | Effect |
| --- | --- | --- |
| `delete-argument` | `index` | Removes that argument from every call and the matching parameter from the declaration |
| `add-argument` | `index`, `argument`, `parameter` | Inserts the argument at every call and the parameter in the declaration |
| `reorder-arguments` | `order` | Permutes both sides; `1,0` swaps the first two |

A `self` receiver occupies parameter slot zero but no argument slot, so the index shift is handled for you. References that are not call sites — a function used as a value, for example — are reported and left alone rather than rewritten into nonsense.

These are OpenRewrite's `DeleteMethodArgument`, `AddLiteralMethodArgument` and `ReorderMethodArguments`, with the matching `AddMethodParameter` and `RemoveMethodParameter` folded in, because Rust has no optional parameters.

### Running the analyzer's own assists

rust-analyzer ships roughly 134 assists — inline a variable, promote a local to a constant, shorten a fully qualified path, add a missing match arm. They are reachable through the same model:

```
rustrewrite run --recipe rust.migrate.assist \
  --set file=src/main.rs --set 'anchor=let sum' \
  --set from=sum --set 'title=Inline variable' --apply
```

The assist is requested over the token at `from`, matched by title, resolved into concrete edits, applied, and then compiled like any other change. When the title does not match, the error lists every assist the analyzer offers at that position, which is the practical way to discover them.

## String literals

Two structural primitives cover OpenRewrite's string-literal recipes. Both match a literal by its exact source spelling, so a comment, an identifier, or a longer string that merely contains the text is untouched, and both cover plain and raw literals. A raw literal keeps its delimiter and hash count.

```
rustrewrite run --recipe rust.migrate.replace-string-literal \
  --set from=api.internal --set to=api.example.com --apply

rustrewrite run --recipe rust.migrate.string-literal-to-constant \
  --set from=api.internal --set to=crate::PROD_HOST --apply
```

`from`, and the replacement value for `replace-string-literal`, are written as they appear between the quotes, so an escaped quote is `\"`, and a raw literal's backslashes are literal characters. Byte (`b"..."`) and C (`c"..."`) string literals are not rewritten.

`replace-string-literal` changes the value and leaves a literal. `string-literal-to-constant` replaces the whole literal with an expression, and it does not create or import anything: the constant has to exist and be in scope where the literal was. The compile gate is what proves it, and a missing constant fails the run and rolls the change back.

## Attributes

Two structural primitives cover OpenRewrite's annotation recipes:

```
rustrewrite run --recipe rust.migrate.remove-attribute \
  --set attribute=deprecated --apply

rustrewrite run --recipe rust.migrate.replace-attribute \
  --set from=deprecated --set to=obsolete --apply
```

`attribute`, `from` and `to` are names, not patterns. `remove-attribute` removes both `#[deprecated]` and `#[deprecated(...)]` from every item that carries it, and takes the whole line when the attribute is alone on it. `replace-attribute` renames the attribute in place and keeps its arguments, so `#[deprecated(note = "old")]` becomes `#[obsolete(note = "old")]`.

Both match the attribute's own name exactly. An attribute whose name merely starts with the same text (`#[deprecated_since]`) is left alone, a comment that mentions the attribute is not an attribute node at all, and an identifier inside the arguments is not the name. Inner attributes (`#![...]`) are outside the two recipes, and `replace-attribute` leaves a scoped path alone rather than renaming one segment of it.

## What the gate does

With `--apply`, in order:

1. Refuses to run on a dirty git tree unless you pass `--allow-dirty`.
2. Snapshots every `.rs` file.
3. Applies the structural steps in memory, writes only files that actually changed.
4. Runs cargo-backed steps (compiler and clippy fixes) on the real tree.
5. Runs `cargo fmt --all`, then `cargo check --all-targets`, then `cargo test --all-targets` if you asked for it.
6. On any failure: restores every snapshot, removes any file the run created, and exits non-zero with the failing output.

A failed gate means the recipe is wrong, not that the code is wrong.

## Writing a recipe

A recipe is a YAML file. The catalog lives in `recipes/`, and a project can add its own under `.rustrewrite/recipes/` (loaded automatically) or any directory passed with `--recipes`.

```yaml
id: rust.idiom.string-len-zero
name: Use is_empty() instead of comparing len() to zero
description: >-
  `len() == 0` predates the is_empty convention.
tags: [idiom, std]
risk: low

steps:
  - rule:
      pattern: $X.len() == 0
    fix: $X.is_empty()

examples:
  - before: if items.len() == 0 { return; }
    after: if items.is_empty() { return; }
    why: The empty case becomes the method that names it.
```

The `rule` object is an [ast-grep rule](https://ast-grep.github.io/reference/rule.html), passed through verbatim: `pattern`, `kind`, `all`/`any`/`not`, `inside`, `has`, `constraints` and `utils` all work.
`constraints` and `utils` are written as siblings of `rule`, mirroring ast-grep's own YAML.
`fix` is a template where `$VAR` and `$$$` metavariables bound by the rule are substituted.

A constraint is what keeps a broad pattern honest. This renames any numeric constant path, but only for the primitive type names, in one rule instead of eighteen:

```yaml
- rule:
    pattern: "std::$T::MAX"
  constraints:
    T:
      regex: "^(i8|i16|i32|i64|i128|isize|u8|u16|u32|u64|u128|usize|f32|f64)$"
  fix: "$T::MAX"
```

Step options:

```yaml
- rule: { pattern: extern crate $NAME; }
  delete: true            # remove the whole line when the match is alone on it
- rule: { pattern: $X.unwrap() }
  message: unwrap() panics on None/Err   # shown by scan recipes
  exclude: ["**/tests/**"]               # path globs, relative to the run root
```

A composite recipe names other recipes and runs them in order:

```yaml
id: rust.idioms
recipes:
  - rust.idiom.string-len-zero
  - rust.idiom.bool-compare
```

A `cargo` step puts a compiler or clippy fixer behind the same gate:

```yaml
steps:
  - kind: cargo
    command: ["cargo", "clippy", "--fix", "--all-targets", "--allow-dirty", "--allow-staged"]
```

A `semantic` step asks rust-analyzer to rename a resolved symbol. It needs `file`, `from` and `to`, plus either `anchor` (text on the declaration line) or `line`:

```yaml
steps:
  - kind: semantic
    action: rename
    file: ${file}
    anchor: ${anchor}
    from: ${from}
    to: ${to}
```

An `insert` step writes text at a file position instead of replacing a node. It needs `where` (`top` or `after-uses`) and `text`; `unless` is the marker that makes it idempotent and defaults to `text`. `include`, `exclude` and `message` work as they do on a structural step:

```yaml
steps:
  - kind: insert
    where: after-uses        # top puts the text on line one
    text: "use ${path};"
    unless: "BTreeMap"       # optional; defaults to `text`
    include: ["src/**"]      # optional path globs, as on any step
    message: adds the import # shown in reports
```

A file that already contains the `unless` marker gets nothing, and reports nothing, so running the same recipe twice is a no-op.
An inserted `use` declaration is also skipped when an existing `use` already binds the same last path segment, so `use std::collections::{BTreeMap, HashMap};` counts as having `BTreeMap`.
The step runs after structural steps and before the gate.

A recipe may declare parameters, the equivalent of OpenRewrite's `@Option` fields, and they are substituted into `${name}`.
`default` makes an option optional and supplies the value when it is not given:

```yaml
options:
  file:
    description: Path to the file that declares the method.
    required: true
  header:
    description: The comment block to insert.
    default: "// SPDX-License-Identifier: MIT"
```

`rustrewrite list` shows which recipes need `--set`, and `explain` describes each option.

A recipe marked `opt-in: true` is only run when it is named; `all` skips it. Use that for a recipe whose default behavior is a project policy choice rather than a mechanical fix, such as which license header to stamp.

An option marked `literal: true` is substituted into an ast-grep pattern or template as text, so it cannot contain an ast-grep metavariable (`$NAME`, `$_`, `$5`, or `$$$`) or a recipe placeholder (`${...}`). The substitution is refused with that explanation rather than matching or compiling something unintended; a literal `$name` is fine.

A recipe may not mix text-editing (`structural` or `insert`) and `semantic` steps. The text pass happens before the rename is resolved, so the analyzer would be pointed at a file that has already moved. Split such a change into two recipes and run them in order.

Every recipe that rewrites something should carry at least one `example`.
`rustrewrite test` runs them all, so a recipe that silently stops matching fails the build rather than quietly doing nothing.

## Inserting text

Some migrations add text where there is no node to replace. An `insert` step covers those, and the same step serves `check` and `run --apply`:

```
rustrewrite check --recipe rust.migrate.add-license-header
rustrewrite run   --recipe rust.migrate.add-license-header --apply

rustrewrite run --recipe rust.migrate.add-import \
  --set path=std::collections::BTreeMap --apply
```

`rust.migrate.add-license-header` writes a `//` comment block at the very top of each file under `src/**`, above `//!` module docs and inner attributes, and skips `vendor/` and `third_party/` directories.
The `header` option replaces the default SPDX line: `--set 'header=// Copyright Acme Ltd'`.
A file that already contains the header text is left alone, so a second run reports nothing.

`rust.migrate.add-import` writes `use <path>;` after the last existing `use` declaration, or above the first item when a file has none.
It does not duplicate an import already present in any form: the duplicate check compares the last segment of `path` against every identifier the file's `use` declarations bind, so `use std::collections::BTreeMap;` and `use std::collections::{BTreeMap, HashMap};` both count.
An alias counts by the name it binds, so `use a::Bar as Baz` covers `Baz` and not `Bar`.
A glob import does not count, because it does not name the item.

An `insert` step has four placements. `top` and `after-uses` are file-level and produce one finding per changed file. `before-match` and `after-match` place the text relative to every node a `rule` selects, and produce one finding per insertion:

```yaml
steps:
  - kind: insert
    where: before-match
    rule:
      pattern: "fn start(&self) { $$$BODY }"
    text: "// TODO: document this method"
```

`before-match` writes on the line directly above the matched node and `after-match` on the line directly below its end.
The text takes the indentation of the line the node starts on, so a comment above a method keeps the `impl` block's depth, and a multi-line `text` is indented line by line.
`before-match` lands directly above the node itself, not above the doc comments or attributes that belong to it.
For both node-relative placements the finding carries the inserted line and column, so a caller sees every comment that was added.

Idempotency is per match for those placements: the step compares the first line of `text` (trimmed), or `unless` when it is given, with the lines directly above or below the node.
A node that already carries the comment is left alone while the other matches still receive theirs, so a second run changes nothing.

`rust.migrate.add-comment-to-method` writes a documentation or TODO comment above every function its `pattern` selects, the Rust form of OpenRewrite's `AddCommentToMethod`.
The value is a function header such as `fn start(&self)` or `fn $NAME(&self)`; the recipe adds the body wildcard `{ $$$BODY }`, so the body is not part of the selection.
`rust.migrate.add-comment-to-import` is the same placement for `use` declarations and the Rust form of `AddCommentToImport`.
A metavariable pattern annotates a group in one run:

```
rustrewrite run --recipe rust.migrate.add-comment-to-method \
  --set 'pattern=fn $NAME(&self)' --set 'comment=// TODO: document' --apply

rustrewrite run --recipe rust.migrate.add-comment-to-import \
  --set 'pattern=use std::$NAME;' --set 'comment=// Legacy std imports.' --apply
```

Since an insertion replaces nothing, its finding's `before` is empty and its `after` is the text written, indentation included.
Neither file-level recipe runs from a bare `all`: the header recipe is marked opt-in, because which license a project carries is a policy decision, and the import recipe needs `--set path`.

An example of a recipe that declares options carries the values it needs, so `test` and `explain` exercise the real substitution instead of a `${name}` placeholder:

```yaml
examples:
  - before: let host = "api.internal";
    after: let host = "api.example.com";
    options:
      from: api.internal
      to: api.example.com
```

## The catalog

| Recipe | Kind | What it does |
| --- | --- | --- |
| `rust.idiom.string-len-zero` | fix | `.len() == 0` becomes `.is_empty()` |
| `rust.idiom.bool-compare` | fix | `x == true` becomes `x` |
| `rust.idiom.bool-return` | fix | `if x { true } else { false }` becomes `x` |
| `rust.idiom.double-negation` | fix | `!!x` becomes `x` |
| `rust.idiom.self-assignment` | fix | removes `x = x;` |
| `rust.idiom.string-from-empty` | fix | `String::from("")` becomes `String::new()` |
| `rust.idiom.iter-nth-zero` | fix | `.nth(0)` becomes `.next()` |
| `rust.idiom.redundant-field-shorthand` | fix | `Point { x: x }` becomes `Point { x }` |
| `rust.idiom.explicit-unit-return` | fix | `return ();` becomes `return;` |
| `rust.idiom.lambda-to-fn` | fix | `|x| f(x)` becomes `f` |
| `rust.idiom.debug-macro-removal` | fix | removes statement-position `dbg!(..)` |
| `rust.idioms` | composite | the nine idiom fixes above, in one pass |
| `rust.modernize.extern-crate` | fix | drops redundant `extern crate` declarations |
| `rust.modernize.legacy-numeric-constants` | fix | `std::i32::MAX` becomes `i32::MAX` |
| `rust.modernize` | composite | edition-2018 mechanical migration |
| `rust.migrate.change-method-name` | semantic | rename a resolved method and its true call sites |
| `rust.migrate.rename-symbol` | semantic | rename a resolved constant, static, field or function |
| `rust.migrate.change-type` | semantic | rename a resolved type and every reference |
| `rust.migrate.change-return-type` | semantic | replace a function's declared return type |
| `rust.migrate.change-visibility` | semantic | replace, add, or remove a function's visibility |
| `rust.migrate.delete-argument` | semantic | drop an argument from a call and a parameter from the declaration |
| `rust.migrate.add-argument` | semantic | insert an argument at every call site and a parameter in the declaration |
| `rust.migrate.reorder-arguments` | semantic | permute arguments at every call site and parameters in the declaration |
| `rust.migrate.assist` | semantic | run one of rust-analyzer's built-in assists, chosen by title |
| `rust.migrate.log-to-tracing` | fix | `log::level!` becomes `tracing::level!` |
| `rust.migrate.add-license-header` | insert | writes a `//` license header above `src/**` files that lack it |
| `rust.migrate.add-import` | insert | adds a missing `use` after the last import, without duplicating one |
| `rust.migrate.add-comment-to-method` | insert | writes a comment above every function a pattern selects |
| `rust.migrate.add-comment-to-import` | insert | writes a comment above every matching `use` declaration |
| `rust.migrate.replace-string-literal` | fix | a string literal's exact value, plain or raw, becomes another value |
| `rust.migrate.string-literal-to-constant` | fix | a string literal becomes an existing constant expression |
| `rust.migrate.remove-attribute` | fix | removes `#[name]` and `#[name(...)]` from every item carrying it |
| `rust.migrate.replace-attribute` | fix | renames an attribute in place, keeping its arguments |
| `rust.fix.cargo-fix` | cargo | applies compiler suggestions |
| `rust.fix.clippy` | cargo | applies clippy machine-applicable fixes |
| `rust.cleanup` | composite | idiom pack, then compiler suggestions |
| `rust.scan.unwrap` | scan | `unwrap()` outside `tests/` |
| `rust.scan.mem-uninitialized` | scan | deprecated `mem::uninitialized` |
| `rust.scan.transmute` | scan | `mem::transmute` |
| `rust.scan.process-exit` | scan | `process::exit` |
| `rust.scan.hardcoded-secret` | scan | a string literal bound to a secret-looking name |

Read-only recipes exist on purpose: `unwrap` is correct in a main, a prototype, or a test.
The decision belongs to the reviewer, so the tool reports it and writes nothing.

[`docs/openrewrite-mapping.md`](docs/openrewrite-mapping.md) audits the full OpenRewrite catalog against Rust and records which recipes transfer, which need type information, and which were attempted and rejected.

[`docs/type-attribution-scope.md`](docs/type-attribution-scope.md) records how the type-aware layer was built and measured, with numbers for each architecture option that was tried.

## Known limits

- **Structural recipes are syntax-only.** A `pattern` in type position does not parse, so a type rewrite needs `kind:` plus `regex:` and cannot capture a metavariable. Use an analyzer-backed recipe when a change must be type-exact.
- **Syntax alone cannot resolve a symbol.** Nothing that needs to know whether an identifier is a trait, a module or a struct can be decided structurally. Use the `rust.migrate.*` recipes that ask rust-analyzer; they are slower because they load the workspace.
- **`change-return-type` does not rewrite call sites.** It edits the declaration only. A caller that relied on the old type makes the gate fail the whole run and roll back, which is the intended outcome rather than a bug. Update the caller by hand and re-run.
- **`change-visibility` is checked against this workspace only.** Making a `pub` item private compiles here even when a downstream crate outside the workspace still calls it. A trait method cannot carry a visibility qualifier, so the gate rejects a change there rather than keeping invalid code.
- **Attribute recipes see outer attributes only.** `#![...]` inner attributes and attributes nested inside `cfg_attr(...)` are not `attribute_item` nodes, so `remove-attribute` and `replace-attribute` leave them alone.
- **Resolved renames do not reach inside macro invocations.** rust-analyzer omits edits for tokens inside a macro call such as `println!`. Keep the renamed use outside the macro or update it by hand; the gate catches the breakage.
- **The gate needs the crate to compile before you start.** A pre-broken tree fails at the gate and rolls back, which is correct but unsurprising.
- **`cargo fmt` rewrites formatting after a fix.** Replacements are syntactically correct but not always layout-correct; the formatter is the last mile.
- **A raw string value containing an unescaped double quote is not matched by `replace-string-literal`.** The recipe shares one `from` value with a plain-literal pattern, and that pattern cannot express a raw quote. A plain literal's escaped quote (`\"`) is supported; a raw value that needs an actual quote is left alone rather than guessed at.

## Development

```
cargo test      # recipe examples plus end-to-end CLI tests against a fixture crate
cargo run -- list
```

The integration tests copy `tests/fixtures/demo-crate` to a temp directory for each case, so nothing is shared and nothing in the repo is touched.
