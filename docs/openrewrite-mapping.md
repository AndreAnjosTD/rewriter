# OpenRewrite recipes, mapped to Rust

An audit of the OpenRewrite recipe catalog against what a Rust rewrite engine can express, and what this tool ships as a result.

Source: the catalog at `codegenomeproject.org/recipes` and its machine-readable index, `artifacts.codegenomeproject.org/recipes.csv`, read on 2026-10-29.

**8,226 recipe entries across 94 artifacts and five ecosystems.**

## The short answer

No, all of them cannot be taken to Rust, and it is not close.

| Ecosystem | Recipes | Rust relevance |
| --- | ---: | --- |
| `maven` (Java/JVM) | 6,817 | Some primitives transfer; almost all concrete recipes are Java-platform-specific |
| `nuget` (C#/.NET) | 737 | None |
| `go` | 290 | None directly, but the closest structural precedent |
| `npm` (JS/TS) | 280 | None |
| `pip` (Python) | 102 | None |

Of the 6,817 Java recipes, 5,466 live inside framework packs whose subject matter has no Rust equivalent:

| Pack | Recipes | Why it does not transfer |
| --- | ---: | --- |
| `java.migrate` | 520 | Java version and JDK API migrations |
| `java.spring` | 321 | Spring framework |
| `java.testing` | 312 | JUnit, Mockito, AssertJ, Hamcrest |
| `java.logging` | 129 | Log4j, SLF4J, Logback |
| `java.security` | 122 | Java crypto and XML security APIs |
| `java.jackson` | 61 | Jackson annotations |
| `java.micronaut`, `java.struts`, `java.jspecify`, `java.joda`, `java.netty`, `java.springdoc`, `java.pmd`, `java.liberty`, `java.flyway` | 135 | Frameworks with no Rust counterpart |

What does transfer is the **vocabulary**: a small core of primitives, plus a code-quality pack whose Rust equivalent is largely clippy.

## Core primitives: `org.openrewrite.java.*` (45 recipes)

These are the reusable building blocks — the ones a declarative recipe composes.

| Verdict | Count | Recipes |
| --- | ---: | --- |
| **Shipped as Rust recipes** | 16 | `ReplaceConstant`, `RemoveMethodInvocations`, `AddImport`, `AddLicenseHeader`, `AddCommentToImport`, `AddCommentToMethod`, `ReplaceStringLiteralValue`, `ReplaceStringLiteralWithConstant`, plus the eight type-attribution primitives listed below |
| **Covered by another mechanism** | 4 | `OrderImports` → `rustfmt` (on by default); `RemoveUnusedImports` → `cargo fix`; `InlineMethodCalls` and `ShortenFullyQualifiedTypeReferences` → `rust.migrate.assist`, which runs the rust-analyzer assist for each |
| **Expressible, needs engine work** | 10 | `ChangePackage`, `ChangePackageInStringLiteral`, `ChangeTypeInStringLiteral`, `ChangeMethodAccessLevel`, `NoStaticImport`, `UseStaticImport`, `RemoveAnnotation`, `RemoveAnnotationAttribute`, `ReplaceAnnotation`, `AddCommentToMethodInvocations` |
| **Still open, needs type attribution** | 8 | `ChangeMethodTargetToStatic`, `ChangeMethodTargetToVariable`, `ChangeStaticFieldToMethod`, `ChangeAnnotationAttributeName`, `AddOrUpdateAnnotationAttribute`, `AddNullMethodArgument`, `SimplifyMethodChain`, `ReplaceMethodInvocationWithConstant` |
| **Not applicable** | 7 | `CreateEmptyJavaClass`, `RandomizeId`, `RecipeMarkupDemonstration`, `RemoveMethodThrows`, `RemoveObjectsIsNull`, `UpdateSourcePositions`, `SimplifySingleElementAnnotation` |

Ten of the original 18 type-attribution primitives have shipped. Eight are driven by `textDocument/rename` and `textDocument/references`:

| OpenRewrite | Rust recipe |
| --- | --- |
| `ChangeType` | `rust.migrate.change-type` |
| `ChangeMethodName` | `rust.migrate.change-method-name` |
| `ChangeMethodInvocationReturnType` | `rust.migrate.change-return-type` |
| `ReplaceConstantWithAnotherConstant` | `rust.migrate.rename-symbol` (the rename form) |
| `DeleteMethodArgument` | `rust.migrate.delete-argument` |
| `AddMethodParameter`, `AddLiteralMethodArgument` | `rust.migrate.add-argument` — one recipe, because Rust has no optional parameters |
| `ReorderMethodArguments` | `rust.migrate.reorder-arguments` |

The other two, `InlineMethodCalls` and `ShortenFullyQualifiedTypeReferences`, are reachable through `rust.migrate.assist`, which runs any rust-analyzer assist by title. See [`type-attribution-scope.md`](type-attribution-scope.md).

Type attribution is what makes the shipped recipes safe: `ChangeType` needs to know that an identifier resolves to the old fully-qualified type before renaming it.

Rust has the information — `rust-analyzer` computes it — but a tree-sitter engine does not, and renaming every occurrence of a name without type information is how a refactoring tool deletes a customer's data.

The compile gate catches breakage, not silent misbinding. The verdicts below record why each of the eight remaining primitives is either Rust-inapplicable or not worth a semantic action.

### Per-primitive verdicts for the 8 still open

| OpenRewrite primitive | Rust analogue | Verdict |
| --- | --- | --- |
| `ChangeMethodTargetToStatic` | none | **No.** Rust has no instance-to-static rewiring: an associated function has no receiver, so there is no `receiver.assoc()` call to rewrite. Turning a method into an associated function is a signature change plus `Type::f(recv)` at every call site, which is a different refactor, not this primitive. |
| `ChangeMethodTargetToVariable` | replace the receiver expression at resolved call sites | **Not worth building yet.** It is expressible: ask for references, then rewrite each receiver span. But the edit is per-migration (which receiver, which variable) and nothing in the current catalog needs it. A structural `$RECEIVER.method()` pattern is wrong whenever another type shares the method name, so the honest form would be a new semantic action; there is no demand for one yet. |
| `ChangeStaticFieldToMethod` | associated const → associated function | **No.** The replacement function has to exist and mean the same thing, and Rust code would keep the const. Rewriting `Type::CONST` to `Type::get()` at every resolved use is mechanically possible with the references layer, but it is an API redesign, not a rewrite. |
| `ChangeAnnotationAttributeName` | `#[old]` → `#[new]` | **Structural, not semantic.** Attributes are syntax, so an ast-grep rule on `attribute_item` can rename one; no type attribution is required. Worth writing only for a named macro's vocabulary (`serde`, `clap`), and as a specific recipe rather than a general primitive. |
| `AddOrUpdateAnnotationAttribute` | add or update an attribute argument | **Structural, and macro-specific.** `#[serde(rename = "...")]` is a text edit inside an attribute. OpenRewrite's version is typed for Java annotations; Rust attributes are arbitrary token trees, so a general primitive has nothing to resolve. |
| `AddNullMethodArgument` | `None` for `Option<T>` parameters | **Subsumed.** Rust has no null. Where the parameter is `Option<T>`, `rust.migrate.add-argument --set argument=None` already writes it at every call site; a separate null primitive would be a worse-named duplicate. |
| `SimplifyMethodChain` | a named chain → a combined call | **No general primitive.** Collapsing `a.b().c()` into `a.bc()` requires knowing the combined method means the same thing; type attribution proves it exists, not that it is equivalent. In Rust the honest form is a specific structural recipe for a named chain (`.iter().cloned().collect()` and friends), which the pattern engine already handles. |
| `ReplaceMethodInvocationWithConstant` | a resolved call → a constant expression | **Feasible, not built.** Resolve the function, ask for its references, and replace each call expression with a constant path; the references layer and the AST call-site detection already exist. It is a new action rather than a new capability, and no migration here has needed it yet. |

## Static analysis and code quality

`org.openrewrite.staticanalysis` carries 194 recipes. The Rust equivalent of most of that pack is **clippy**, and this tool already runs it behind the correctness gate as `rust.fix.clippy`.

| Bucket | Approx. | Handling in Rust |
| --- | ---: | --- |
| Covered by clippy / `cargo fix` | ~110 | `rust.fix.clippy`, `rust.fix.cargo-fix` |
| Shipped as Rust recipes | 9 | see below |
| Java-specific (collections, streams, annotations, serialization) | ~50 | none |
| Expressible, not yet written | ~25 | candidate backlog |

Shipped because clippy does not cover them, or because the result reads better than the lint fix:

| OpenRewrite | Rust recipe |
| --- | --- |
| `SimplifyBooleanReturn` | `rust.idiom.bool-return` |
| `SimplifyBooleanExpression` | `rust.idiom.bool-compare` |
| `SimplifyDoubleNegation` (Go pack) | `rust.idiom.double-negation` |
| `RemoveSelfAssignment` | `rust.idiom.self-assignment` |
| `ReplaceLambdaWithMethodReference` | `rust.idiom.lambda-to-fn` |
| `RemoveSystemOutPrintln` | `rust.idiom.debug-macro-removal` |
| `IsEmptyCallOnCollections` | `rust.idiom.string-len-zero` |
| `FindSystemAndRuntimeExitCalls` | `rust.scan.process-exit` |
| `AvoidHardcodedCredentials` (Go pack) | `rust.scan.hardcoded-secret` |
| `AvoidUnsafePackage` (Go pack) | `rust.scan.transmute` |

## What was written this round

| Recipe | Kind | OpenRewrite origin |
| --- | --- | --- |
| `rust.idiom.bool-return` | fix | `SimplifyBooleanReturn` |
| `rust.idiom.double-negation` | fix | `SimplifyDoubleNegation` |
| `rust.idiom.self-assignment` | fix | `RemoveSelfAssignment` |
| `rust.idiom.lambda-to-fn` | fix | `ReplaceLambdaWithMethodReference` |
| `rust.idiom.redundant-field-shorthand` | fix | none — Rust-specific idiom |
| `rust.idiom.debug-macro-removal` | fix | `RemoveSystemOutPrintln` |
| `rust.idiom.explicit-unit-return` | fix | none — Rust-specific idiom |
| `rust.modernize.legacy-numeric-constants` | fix | `ReplaceConstant` |
| `rust.migrate.replace-string-literal` | fix | `ReplaceStringLiteralValue` |
| `rust.migrate.string-literal-to-constant` | fix | `ReplaceStringLiteralWithConstant` |
| `rust.migrate.change-return-type` | semantic | `ChangeMethodInvocationReturnType` |
| `rust.migrate.rename-symbol` | semantic | `ReplaceConstantWithAnotherConstant` (the rename form) |
| `rust.scan.transmute` | scan | `AvoidUnsafePackage` |
| `rust.scan.process-exit` | scan | `FindSystemAndRuntimeExitCalls` |
| `rust.scan.hardcoded-secret` | scan | `AvoidHardcodedCredentials` |
| `rust.modernize` | composite | declarative recipe composition |
| `rust.migrate.add-license-header` | insert | `AddLicenseHeader` |
| `rust.migrate.add-import` | insert | `AddImport` |
| `rust.migrate.add-comment-to-import` | insert | `AddCommentToImport` |
| `rust.migrate.add-comment-to-method` | insert | `AddCommentToMethod` |

## Recipes that were attempted and rejected

Honesty about the failures is more useful than a longer catalog.

| Attempt | Why it was dropped |
| --- | --- |
| `&Vec<T>` → `&[T]` in parameters | Type position. `&Vec<$T>` is not a valid expression, so ast-grep's pattern parser rejects it, and `kind` + `regex` cannot capture the type parameter for the replacement. |
| `try!(expr)` → `(expr)?` | `try` is a reserved keyword in the current grammar, so no pattern parses. Only a regex over the macro's raw text would match, and a regex cannot capture the inner expression to rewrite it. |
| `ChangeMethodName` | Without type attribution it renames every same-named method in the codebase, including ones on unrelated types. That is a footgun, not a recipe. (It shipped as `rust.migrate.change-method-name` once the type-attribution layer existed.) |

## What would close the remaining gap

The type-attribution layer and recipe options are shipped; the per-primitive verdicts above record why the last eight type-attribution primitives are not next. What is left:

1. **The remaining comment recipe.** `AddCommentToImport` and `AddCommentToMethod` shipped on the `before-match` and `after-match` placements of the `insert` step, which write text above or below every node an ast-grep rule selects and keep the matched line's indentation. `AddCommentToMethodInvocations` is now expressible on the same placement; it is unshipped only because no invocation-selecting recipe has been written yet.
2. **The remaining string-literal migrations.** `ReplaceStringLiteralValue` and `ReplaceStringLiteralWithConstant` shipped as structural recipes. `ChangePackageInStringLiteral` and `ChangeTypeInStringLiteral` rewrite Java package and type names inside strings; neither has a Rust equivalent, so they stay unbuilt.
