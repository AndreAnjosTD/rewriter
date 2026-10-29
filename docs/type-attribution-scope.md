# Scope: a type-attribution layer for rustrewrite

Status: phase 1 implemented; phases 2-5 not started.

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
| 2 | `kind: semantic` step, `--diff`/`--check` support, two flagship recipes (`rust.migrate.change-method-name`, `rust.migrate.change-type`) | `check` reports semantic findings without writing; `run --apply` goes through the gate | 2–3 d |
| 3 | References-driven call-site rewriting for the argument primitives | `DeleteMethodArgument` equivalent works on a multi-site fixture | 4–6 d |
| 4 | Assists as recipe sources: enumerate `codeAction` results and expose them by id | One assist runs end-to-end through the recipe model | 3–5 d |
| 5 | Performance and lifecycle: cache the loaded workspace, `prefill_caches`, skip the server when no semantic step is present | Cold run on NodeHugDesktop under 90 s, warm semantic run under 10 s | 2–3 d |

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

## Risks

| Risk | Severity | Mitigation |
| --- | --- | --- |
| 78 s / 2 GB cold load on a real workspace | High — makes semantic recipes feel unlike the rest of the tool | Only start the server when a semantic step exists; cache the loaded workspace; document the cost per recipe |
| Toolchain drift in `ra_ap_*` | Removed by choosing Option A | The LSP protocol is stable; the binary ships with the toolchain |
| Server hangs or crashes mid-run | Medium | Timeouts on every request, kill the server, treat as a failed step and roll back |
| Rename refuses valid-looking targets (trait impls, generated code) | Medium — already observed | Report `0 edits` explicitly as "the server declined", never as success |
| Windows file URIs and path separators | Low, known | Covered in phase 1 tests |
| Unsaved-buffer semantics: LSP edits buffers, not files | Low | We apply edits ourselves; `didOpen` with current disk text keeps them in sync |

## Deliberately out of scope

- Compiling or type-checking: rust-analyzer does that for its own analysis; the correctness gate keeps using `cargo`, which is the authority.
- Macro expansion semantics beyond what the server reports.
- Non-Cargo projects beyond a clear error message.
- A persistent daemon, warm-start server, or background indexing — phase 5 at the earliest.

## Recommendation

Build phases 1 and 2, in that order, against a fixture crate that already contains the ambiguous cases from this spike: two types with the same method name, a trait impl that must refuse to rename, and a method with three call sites.

The gate stays the last line of defence: semantic edits are still just edits, and a rewrite that does not compile still rolls back.
