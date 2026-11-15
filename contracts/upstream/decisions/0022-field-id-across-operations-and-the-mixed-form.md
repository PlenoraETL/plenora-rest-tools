# 0022: Field identity across operations, and the form of `mixed`

Status: accepted
Date: 2026-10-20

## Context

The interoperability matrix measured on the current family (io 5.0.0,
database 8.0.0, data 3.0.0) found two places where components interpreted
ARROW-VOCABULARY-1.0 §2–§3 in different, individually defensible ways:

- `plenora.field_id`. The vocabulary says «Unique within a schema and
  preserved for an unchanged logical field». database-tools 8.0.0 derives
  the id of every column it reads from its PostgreSQL provenance (a hash of
  `attrelid`+`attnum`), which is stable and name-independent — but in a
  `write` → `read` round trip it **replaces** the id the writer received,
  so a chain loses field identity across the database hop
  (plenora-database-tools#124). The same revision emits the catalog
  metadata on the CLI surface but not on the Rust API surface
  (plenora-database-tools#125).
- `plenora.geometry.types` with `types_declaration=mixed` and no enumerated
  list. database-tools omits the key; plenora-IO-tools serializes the
  contract's empty list as `types=""` (plenora-IO-tools#66). The grammar —
  «comma-separated canonical geometry types» over a closed list — admits no
  empty value, but it never said what a producer that cannot enumerate must
  do with the key.

The published vocabulary keeps the meaning it was published with; this
decision fixes the reading the family commits to, for the vectors and for
the re-entries of the three issues.

## Decision

1. **Field identity crosses an operation.** A field that an operation reads
   and writes back unchanged (same logical field: same data and declared
   semantics, whatever its name or position) keeps the `plenora.field_id` it
   arrived with. A store that keeps its own provenance identity records it
   in its own metadata, not in `plenora.field_id`.
2. **A provenance id is for fields born in the store.** When a field has no
   incoming `plenora.field_id` — it was created in the store, or the writer
   declared none — the reader assigns one; a deterministic provenance-based
   id is a good assignment. Once assigned, rule 1 applies to it.
3. **The same operation gives the same metadata on every surface.** CLI,
   SDK and API readings of the same operation carry the same vocabulary
   keys with the same values (environment-dependent values excepted only
   where the vocabulary itself says so — today nowhere).
4. **`mixed` without an enumerable list omits `plenora.geometry.types`.**
   The key, when present, holds one or more canonical types; the empty
   string is not a value. As a new recommendation of this decision — the
   published §3 only constrains the list's order and uniqueness when the
   key is present — a producer that can enumerate SHOULD enumerate, in
   canonical order without duplicates.

## Consequences

- plenora-database-tools#124 re-enters by storing the incoming id at write
  and re-emitting it at read, with the provenance hash as the assignment
  for fields without one (rule 2). #125 re-enters by moving the catalog
  enrichment into the surface-shared path.
- plenora-IO-tools#66 re-enters by omitting the key for an empty list.
- The conformance vectors gain the three cases: a round trip that must
  preserve the id, an assignment for a store-born field, and a `mixed`
  document without the key that a consumer must accept (and one with
  `types=""` that it must reject).
- A successor vocabulary can state rules 1–4 in its own text; until then
  this decision is the family's committed reading of the published one.
