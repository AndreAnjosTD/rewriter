# Scope: a type-attribution layer for rustrewrite

Status: all five phases implemented.

The question this answers: what would it take to make `rustrewrite` do the 18 OpenRewrite primitives that need type information — `ChangeType`, `ChangeMethodName`, `ReorderMethodArguments` and friends — and what does it cost?

Short version: **it is feasible through rust-analyzer's language server, it is a one-week MVP, and it is not feasible by embedding rust-analyzer's own crates.** All three claims below are backed by experiments run on this machine on 2026-10-29, not by reading documentation.

## Why this matters

[`openrewrite-mapping.md`](openrewrite-mapping.md) found that 18 of OpenRewrite's 45 core primitives need type attribution. Those 18 are the conceptual heart of OpenRewrite: renaming a type across a codebase, renaming a method only where it refers to one specific impl, reordering arguments at every call site of a resolved function.

A syntax engine cannot do any of them safely. `rustrewrite` today can rewrite `.len()` to `.is_empty()` but cannot tell a `Vec::len` from a `MyBag::len`, so it would rewrite both and let the compile gate catch the damage. That is fine for idioms and unacceptable for API migration.

## What was measured

| Experiment | Result |
| --- | --- |
| `rust-analyzer search '$a.clone()'` on a file with both `String::clone` and a custom `Clone` impl | **Both matched.** Method patterns match by name, not by resolution. |
| `rust-analyzer search '$a.len()'` with `Vec::len` and a custom `Other::len` | **Both matched.** Same conclusion. |
| `rust-analyzer search 'Option::unwrap($a)'` and `'String::clone($a)'` | **"Failed to resolve path".** Qualified patterns resolve against crate scope and mostly fail to parse. |
| `textDocument/rename` on `Other::len`, with `Vec::len` in the same file | **2 edits: the declaration and its one true call. `Vec::len` untouched.** |
| `rust-analyzer analysis-stats` on this machine's trivial 18-line crate | 909 ms database load, 1.90 s item trees, 1.64 s item collection — **~4.5 s, 287 MB** |
| `rust-analyzer analysis-stats` on NodeHugDesktop (38,410 LoC, 5.3M dependency LoC) | 43.9 s database load, 18.4 s item trees, 15.8 s item collection — **~78 s cold, 1.9 GB peak** |
| `cargo build` with `ra_ap_ide` / `ra_ap_load-cargo` / `ra_ap_project_model` 0.0.331 | **Fails**: a dependency fails const evaluation on rustc 1.94.1 |
| Same with 0.0.358 | **Refuses**: "rustc 1.94.1 is not supported by the following packages" |
| Assists shipped in this rust-analyzer's source tree | **134 handler files** under `crates/ide-assists/src/handlers` |

## Architecture options

### Option A — drive rust-analyzer as a language server (recommended)

Spawn `rust-analyzer`, speak LSP over stdio, use four requests:

| LSP request | What it gives us | OpenRewrite primitives it unlocks |
| --- | --- | --- |
| `textDocument/rename` | Type-exact rename of one resolved symbol, with edits returned as a `WorkspaceEdit` | `ChangeType`, `ChangeMethodName`, `ChangePackage`, `ReplaceConstantWithAnotherConstant` |
| `textDocument/references` | Every true reference to a resolved symbol | `ReorderMethodArguments`, `DeleteMethodArgument`, `AddMethodParameter`, `AddLiteralMethodArgument`, `AddNullMethodArgument` |
| `textDocument/codeAction` | rust-analyzer's own 134 assists, type-aware | `InlineMethodCalls`, `ShortenFullyQualifiedTypeReferences`, plus a long tail of Rust-specific refactors |
| `textDocument/hover` | Inferred type at a position | Preconditioning: only rewrite when the receiver really is a `Vec` |

**Proven**: the rename experiment did exactly what `ChangeMethodName` must do.

**Cost**: the cold load is the whole price. ~4.5 s on a toy crate, ~78 s and 2 GB on a real workspace. A long-lived server amortizes it; a one-shot CLI process pays it on every run.

### Option B — embed the `ra_ap_*` crates (rejected)

The crates exist and expose the same APIs in-process, synchronously, with no protocol. Two versions were tried; both fail to build against the installed stable toolchain, one with a const-evaluation error, the other with an explicit version refusal.

The crates track rustc internals and need a matching toolchain. Adopting them would mean pinning users to one exact Rust version, which is a worse constraint than a 78-second cold start for a tool meant to run on any crates checkout.

### Option C — shell out to `rust-analyzer ssr` (rejected)

Read of `crates/rust-analyzer/src/cli/ssr.rs` plus the experiments above:

- Matching is **by method name**, not resolution, so it cannot distinguish `Vec::len` from `Other::len` — the exact failure we are trying to fix.
- It **writes files in place** with no dry run, no report, and no rollback. That is incompatible with `rustrewrite`'s check/run/gate model.
- Qualified patterns mostly fail to resolve, so the natural phrasing of a migration is unavailable.

## Design: how it fits the existing tool

A new step kind alongside `structural` and `cargo`:

```yaml
id: rust.migrate.rename-method
name: Rename a resolved method
steps:
  - kind: semantic
    action: rename
    # Where to anchor the symbol, resolved by the server rather than by text.
    at:
      file: src/server.rs
      symbol: Server::start
    new_name: spawn
```

The engine phases become:

1. Structural steps, in memory, as today.
2. Flush to disk.
3. **Semantic steps** — start rust-analyzer, open the workspace, run each action, collect `WorkspaceEdit`s, apply them ourselves so they are diffable and reportable.
4. Cargo steps, as today.
5. The correctness gate, unchanged.

Applying the `WorkspaceEdit` ourselves rather than letting the server write is what preserves check/run/gate, `--diff`, and rollback. The gate stays the backstop: a semantic edit that breaks the build still rolls back.

Non-negotiable details:

- **Files must be on disk** before the server loads, so structural changes flush first.
- **A `Cargo.toml` is required**; non-Cargo projects need `rust-project.json`. Detect and report rather than silently doing nothing.
- **Server lifecycle**: one process per `rustrewrite` invocation, killed on exit. A warm-server mode can come later.
- **No-op detection**: `edits == 0` is a normal outcome for a rename the server refuses (a trait impl, for example). It must be reported, not treated as success.

## Phased plan

| Phase | Work | Exit criterion | Estimate |
| --- | --- | --- | --- |
| 0 | This spike | Done — evidence above | 0.5 d |
| 1 | LSP client in Rust: JSON-RPC framing, `initialize`, `didOpen`, `rename`, `WorkspaceEdit` application, server shutdown | **Done** — see results below | 3–5 d |
| 2 | `kind: semantic` step, `--diff`/`--check` support, two flagship recipes (`rust.migrate.change-method-name`, `rust.migrate.change-type`) | **Done** — see results below | 2–3 d |
| 3 | References-driven call-site rewriting for the argument primitives | **Done** — see results below | 4–6 d |
| 4 | Assists as recipe sources: enumerate `codeAction` results and expose them by id | **Done** — see results below | 3–5 d |
| 5 | Performance and lifecycle: warm server per workspace, `prefill_caches`, skip the server when no semantic step is present | **Done** — see results below: 17.1 s cold, 0.26 s warm | 2-3 d |

**MVP is phases 1 and 2: about one week of focused work**, and it delivers the two primitives that carry most of OpenRewrite's value.

## Phase 1 results

Implemented in `src/semantic.rs` and exposed as `rustrewrite rename`. Four integration tests cover it; the whole suite runs in about 15 s.

Exit criterion, measured on 2026-10-29:

| Check | Result |
| --- | --- |
| `Bag::len` renamed with `Vec::len` in the same file | 2 edits: declaration and its one call. `Vec::len` untouched. |
| Method with three call sites | 4 edits: declaration and all three calls. Unrelated sibling method untouched. |
| Trait implementation member | See below — refused or rolled back, never left broken. |
| NodeHugDesktop, test helper `fn graph` | 9 edits across 1 file in 33 s, dry run, nothing written. |

Five things the spike did not predict:

1. **Progress notifications are opt-in.** Without `window.workDoneProgress: true` in the client capabilities, rust-analyzer sends no `$/progress` at all and the readiness wait burns its entire budget. This cost 12 minutes of test time before it was noticed.
2. **A `null` response is not a refusal.** It means "not answerable yet". Conflating the two made every cold workspace look like a server that refuses everything, so `rename` now retries `null` until a deadline and treats only a non-null empty edit set as a refusal.
3. **The server will rename a trait implementation member** rather than refusing it, producing code that no longer implements the trait. The compile gate caught this and rolled it back, which means the gate is load-bearing for semantic edits too, not just structural ones.
4. **`rustup` proxies `rust-analyzer` and honours the project's toolchain pin.** A project pinned to a toolchain without the component fails at `initialize` with "Unknown binary". The error now names the fix.
5. **Windows returns verbatim paths** (`\\?\C:\...`) from `canonicalize`, which broke a workspace-containment check until both sides were normalized.

## Phase 2 results

A `kind: semantic` step now runs a rename from a recipe, and recipes can declare parameters:

```yaml
options:
  file: { required: true }
  anchor: { required: true }
  from: { required: true }
  to: { required: true }
steps:
  - kind: semantic
    action: rename
    file: ${file}
    anchor: ${anchor}
    from: ${from}
    to: ${to}
```

That parameterization is the Rust form of OpenRewrite's `@Option`, and it is what makes `ChangeMethodName` and `ChangeType` reusable primitives instead of one recipe per migration. `rustrewrite list` marks the recipes that need `--set`, and `explain` describes each option.

Four decisions worth recording:

1. **An anchor, not a line number.** A line number is invalidated by any edit above the declaration; text on the declaration line is not.
2. **Structural and semantic steps cannot share a recipe.** The structural pass edits text in memory, so a rename resolved afterwards would be aimed at a file that has already moved. The loader refuses the combination rather than guessing.
3. **Missing options are listed together**, not one per run, and an unknown `--set` key is rejected rather than silently ignored.
4. **`check` runs the server too.** A semantic recipe reports its edits and exits 1, exactly like a structural one, and still writes nothing.

Verified: `check` reports the two edits for `Bag::len` and leaves the file byte-identical; `run --apply` renames the declaration and its call, leaves `Vec::len` alone, and passes the gate; `change-type` renames the declaration, the `impl` block and the use site in one pass. Eight integration tests cover this, and the full suite runs in about 21 seconds.

## Phase 3 results

`textDocument/references` plus a local AST pass now rewrite argument lists:

| Recipe | OpenRewrite origin |
| --- | --- |
| `rust.migrate.delete-argument` | `DeleteMethodArgument` + `RemoveMethodParameter` |
| `rust.migrate.add-argument` | `AddLiteralMethodArgument` + `AddMethodParameter` |
| `rust.migrate.reorder-arguments` | `ReorderMethodArguments` |

The dispatch is: locate the declaration by anchor, ask the server for every reference, and for each one decide whether it is a call site. A reference inside the callee position of a `call_expression` is rewritten; anything else — a function used as a value, an import — is counted and reported, never rewritten into nonsense. The declaration's parameter list moves in the same run, because a call-site-only edit does not compile.

Four things this phase taught:

1. **A `self` receiver breaks index arithmetic.** It occupies parameter slot zero but no argument slot, so every declaration-side index shifts by one. Reordering was worse: prepending the receiver unconditionally produced a three-position permutation for a two-parameter function.
2. **An empty reference list is ambiguous.** On the real workspace the server answered `[]` while still indexing, which read as "this function is never called". Empty results now get a short grace before being believed — the same class of bug as the `null` rename response in phase 1.
3. **References are not call sites.** `textDocument/references` happily returns imports and function-value uses. Filtering them by AST is not optional; rewriting them would produce code that compiles but means something else.
4. **Overlapping edits must be refused, not merged.** Two rewrites that touch the same bytes now fail loudly rather than being applied in an order that happens to work.

Verified: on the fixture, deleting argument 0 rewrites the declaration and all three call sites and the crate still builds; the same works through a `self` receiver; adding and reordering both compile. On NodeHugDesktop, reordering a two-parameter test helper finds the declaration and eight call sites in one dry run. Nineteen integration tests across three files, about 38 seconds.

## Phase 4 results

`rust.migrate.assist` runs one of rust-analyzer's built-in assists by title. The analyzer offers about 134 of them, and they were previously unreachable from the recipe model.

The protocol has three traps, all found by probing rather than reading:

1. **CodeAction literals are opt-in.** Without `textDocument.codeAction.codeActionLiteralSupport` in the client capabilities, rust-analyzer returns no assists at all — every position probes empty. This looked exactly like "there are no assists here" until the capability was added.
2. **Assists arrive without an edit.** They come back with `data` and no `edit`; the edit only exists after a `codeAction/resolve` round trip. Applying what comes back from the first call would silently do nothing.
3. **Titles are the only handle.** There is no stable assist id in the response, so a recipe names the title (`Inline variable`). When it does not match, the error lists every assist available at that position, which turns a failed lookup into discovery.

Verified: `Inline variable` on `let sum = items.iter().sum::<u32>();` rewrites the function to `items.iter().sum::<u32>() * 2`, the gate passes, and `check` reports the same three edits without writing. A wrong title fails with the available list. Twenty-two integration tests across four files, about 34 seconds.

## Phase 5 results

A warm server per workspace. The first semantic command starts it in the background; later commands attach over a loopback socket and skip the load entirely.

Measured on the same 35k-line workspace used in phase 1, one dry-run rename of `cli/src/exit.rs:112:12`:

| Run | Before | Now |
| --- | --- | --- |
| First command on a workspace (starts the server) | 33 s | 17.1 s |
| Every following command | 33 s | 0.26 s |
| A semantic recipe (`change-method-name`, dry run) | 33 s | 1.34 s |

The phase's exit criterion was a cold run under 90 s and a warm run under 10 s; both hold with room.

Four things this phase taught, all of them found by running it:

1. **A Windows child inherits every inheritable handle of its parent.** Starting the server with `Command::spawn` and null stdio left the caller's stdout pipe open inside the server, so `rustrewrite ... | tee log` never saw the pipe close and looked like a hang for as long as the server lived. The stable fix is `CreateProcessW` with `bInheritHandles = FALSE`; std exposes that flag only on nightly. This also explains why the warm tests passed in isolation but behaved strangely under a capturing harness.
2. **An accepted socket inherits the listener's non-blocking flag on Windows.** The listener is non-blocking so the accept loop can check its idle timer; every accepted connection therefore failed its second read with `WouldBlock` and was closed midway. One `set_nonblocking(false)` per connection fixes it. Nothing in the error message pointed at this.
3. **Progress notifications are not a readiness signal.** rust-analyzer emits `$/progress` for unrelated work such as fetching dependencies, so "all progress tokens ended" can be true within milliseconds of startup while the crate graph is still empty. The real signal is the answer to the actual query: `null` means "ask again", a result means ready. Readiness is now advisory and the query loop does the waiting.
4. **A rejection on a cold server is not final.** `No references found at position` arrives both for a genuinely wrong line and for a valid line on a workspace that has not finished loading. Rejections are therefore retried for 20 s while the server has never resolved anything, and reported immediately once it has — a typo on a warm workspace now fails in a second instead of burning the full 180 s budget.

The server also asks for `rust-analyzer.prefillCaches` once it is ready, which is what makes the *next* cold start cheaper. It is a hint: a build that does not know the command answers with an error that is ignored.

Verified: 28 tests across six files, including four new ones — a structural recipe proving it never launches an analyzer, a semantic run naming the missing binary, a second run served by the same process, and `--cold` leaving no server behind.

## Risks

| Risk | Severity | Mitigation |
| --- | --- | --- |
| Cold load on a real workspace | Medium | Measured at 17 s; paid once per workspace, then 0.26 s per command through the warm server |
| A warm server holds ~2 GB until it exits | Medium | 15-minute idle timeout, `serve --stop`, `--cold`, and no server at all for structural recipes |
| Toolchain drift in `ra_ap_*` | Removed by choosing Option A | The LSP protocol is stable; the binary ships with the toolchain |
| Server hangs or crashes mid-run | Medium | Timeouts on every request, kill the server, treat as a failed step and roll back |
| Rename refuses valid-looking targets (trait impls, generated code) | Medium — already observed | Report `0 edits` explicitly as "the server declined", never as success |
| Windows file URIs and path separators | Low, known | Covered in phase 1 tests |
| Unsaved-buffer semantics: LSP edits buffers, not files | Low | We apply edits ourselves; `didOpen` with current disk text keeps them in sync |

## Deliberately out of scope

- Compiling or type-checking: rust-analyzer does that for its own analysis; the correctness gate keeps using `cargo`, which is the authority.
- Macro expansion semantics beyond what the server reports.
- Non-Cargo projects beyond a clear error message.
- Sharing one warm server between different workspaces, or across machines.

## Recommendation

Build phases 1 and 2, in that order, against a fixture crate that already contains the ambiguous cases from this spike: two types with the same method name, a trait impl that must refuse to rename, and a method with three call sites.

The gate stays the last line of defence: semantic edits are still just edits, and a rewrite that does not compile still rolls back.
