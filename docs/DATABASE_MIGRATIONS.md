# Database Format and Change Contract

The [active alpha policy](../AGENTS.md#active-alpha-compatibility-policy) is
authoritative. Breaking source, model, schema and database-format changes are
allowed until the user explicitly declares release readiness or a compatibility
commitment. Legacy readers, migrations, historical conversions and tests solely
for earlier alpha formats are not required, including unpublished branch formats.
Fresh or recreated alpha databases are acceptable for development. Make format
rejection and any intentional recreation explicit; do not silently reset user
state or delete/modify user media. This policy update performs no reset.

This replaces this document's earlier mandatory migration, additive repair,
backfill, legacy read-only projection and old-row preservation requirements.
Existing implementations and verification results remain historical facts;
their existence does not impose future compatibility work. Removing unrelated
migration code is not part of this policy change.

## Database Families

- Source databases live in each indexed source folder as `.yield.db`.
  They own source-local metadata, tags, ratings, curation and scan state,
  pending rename and file-operation recovery state, revision-fenced readiness
  targets/completions, job metadata and analysis/cache references.
- The global library database lives under Yield's app config directory as
  `library.db`. It owns configured source references and global analysis/cache
  state that is not source-local.

## Current-Format Correctness

Schema changes must update the current base DDL, affected readers/writers and
meaningful current-format contract tests together. Identify the current format
explicitly. A version stamp alone does not establish structural validity; validate
required structure and report unsupported or malformed state explicitly. There
is no obligation to repair older alpha shapes or keep them readable.

Preserve complete promised save/reopen behavior, current-format recovery and
transaction correctness, ownership boundaries, media references and consistency
between durable state and UI projection. Do not suppress a safety assertion or
accept invalid current state merely to avoid a failing test. Intentional format
breaks do not justify accidental corruption or partial writes.

## Source Database Path and Read-Only Policy

Source-local writes stay inside the configured source/database root. Existing
and newly created database parents must resolve under the canonical root. Opens,
any retained legacy filename handling and SQLite WAL/SHM operations reject linked
database/sidecar paths before SQLite or rename operations.

Read-only UI/background roles must not mutate databases or apply migrations.
They may report an unsupported older format; safe-default projections and legacy
reader fixtures are not required solely for earlier alpha compatibility.

## Verification

Choose focused existing checks for the changed current-format behavior. Relevant
tests currently live in
`crates/yield-library/src/sample_sources/db/source_db_migration_tests/` and
`crates/yield-library/src/sample_sources/library/tests.rs`; historical test/module
names are not compatibility obligations. Adapt affected compatibility-only checks
when a deliberate format break makes their old assumptions obsolete, preserving
current-format safety and correctness coverage.

Use the current [GPUI local validation guidance](GPUI_LOCAL_CI.md) and
[test map](TEST.md#database-format-validation). Native requirements remain scoped
to the behavior changed; this document does not reinstate archived CI lanes or
require a broad native run for a documentation-only policy edit.
