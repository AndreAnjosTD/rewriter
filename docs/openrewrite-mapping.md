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
| **Shipped as Rust recipes** | 2 | `ReplaceConstant` → `rust.modernize.legacy-numeric-constants`; `RemoveMethodInvocations` → `rust.idiom.debug-macro-removal` |
| **Covered by another mechanism** | 2 | `OrderImports` → `rustfmt` (on by default); `RemoveUnusedImports` → `cargo fix` |
| **Expressible, needs engine work** | 16 | `AddImport`, `AddLicenseHeader`, `ChangePackage`, `ChangePackageInStringLiteral`, `ChangeTypeInStringLiteral`, `ReplaceStringLiteralValue`, `ReplaceStringLiteralWithConstant`, `ChangeMethodAccessLevel`, `NoStaticImport`, `UseStaticImport`, `RemoveAnnotation`, `RemoveAnnotationAttribute`, `ReplaceAnnotation`, `AddCommentToImport`, `AddCommentToMethod`, `AddCommentToMethodInvocations` |
| **Needs type attribution** | 18 | `ChangeType`, `ChangeMethodName`, `ChangeMethodTargetToStatic`, `ChangeMethodTargetToVariable`, `ChangeMethodInvocationReturnType`, `ChangeStaticFieldToMethod`, `ChangeAnnotationAttributeName`, `AddOrUpdateAnnotationAttribute`, `AddMethodParameter`, `AddLiteralMethodArgument`, `AddNullMethodArgument`, `DeleteMethodArgument`, `ReorderMethodArguments`, `InlineMethodCalls`, `ShortenFullyQualifiedTypeReferences`, `SimplifyMethodChain`, `ReplaceMethodInvocationWithConstant`, `ReplaceConstantWithAnotherConstant` |

Six of those 18 have since shipped, driven by the language server rather than by syntax: `rust.migrate.change-type`, `rust.migrate.change-method-name`, and the argument family (`delete-argument`, `add-argument`, `reorder-arguments`), which together cover `DeleteMethodArgument`, `AddMethodParameter`, `AddLiteralMethodArgument` and `ReorderMethodArguments`. See [`type-attribution-scope.md`](type-attribution-scope.md).
| **Not applicable** | 7 | `CreateEmptyJavaClass`, `RandomizeId`, `RecipeMarkupDemonstration`, `RemoveMethodThrows`, `RemoveObjectsIsNull`, `UpdateSourcePositions`, `SimplifySingleElementAnnotation` |

The 18 in the type-attribution bucket are the heart of OpenRewrite, and they are exactly what a syntax-only engine cannot do.

`ChangeType` needs to know that an identifier resolves to the old fully-qualified type before renaming it.

Rust has the information — `rust-analyzer` computes it — but a tree-sitter engine does not, and renaming every occurrence of a name without type information is how a refactoring tool deletes a customer's data.

The compile gate catches breakage, not silent misbinding. So these stay out until there is a semantic layer.

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
| `rust.scan.transmute` | scan | `AvoidUnsafePackage` |
| `rust.scan.process-exit` | scan | `FindSystemAndRuntimeExitCalls` |
| `rust.scan.hardcoded-secret` | scan | `AvoidHardcodedCredentials` |
| `rust.modernize` | composite | declarative recipe composition |

## Recipes that were attempted and rejected

Honesty about the failures is more useful than a longer catalog.

| Attempt | Why it was dropped |
| --- | --- |
| `&Vec<T>` → `&[T]` in parameters | Type position. `&Vec<$T>` is not a valid expression, so ast-grep's pattern parser rejects it, and `kind` + `regex` cannot capture the type parameter for the replacement. |
| `try!(expr)` → `(expr)?` | `try` is a reserved keyword in the current grammar, so no pattern parses. Only a regex over the macro's raw text would match, and a regex cannot capture the inner expression to rewrite it. |
| `ChangeMethodName` | Without type attribution it renames every same-named method in the codebase, including ones on unrelated types. That is a footgun, not a recipe. |

## What would close the gap

In rough order of value per unit of work:

1. **A type-attribution layer.** `rust-analyzer` exposes diagnostics, symbol resolution and assists over LSP. Driving it would unlock the whole `ChangeType` / `ChangeMethodName` / `ReorderMethodArguments` family, which is 18 of the 45 primitives and most of the catalog's conceptual weight. Scoped, with measurements, in [`type-attribution-scope.md`](type-attribution-scope.md).
2. **Import and header edits.** `AddImport`, `AddLicenseHeader` and the comment recipes need a step kind that inserts text at a file position rather than replacing a match. Small engine addition, unlocks 6 recipes.
3. **String-literal rewrites.** `ChangePackageInStringLiteral`, `ReplaceStringLiteralValue` and `ChangeTypeInStringLiteral` need a rule that matches inside a literal; the engine supports this today but the recipes are not written.
4. **Recipe options.** OpenRewrite parameterizes `ChangeType` with `oldFullyQualifiedTypeName` / `newFullyQualifiedTypeName`. This tool hard-codes each migration as its own recipe. A `${option}` substitution with `--set` would make the catalog generative instead of enumerative.
