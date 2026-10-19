# Plenora Interoperability Vectors 1.0

Status: normative

Contract identifier: `plenora-interop-vectors-v1`

## 1. Scope

This contract fixes what a pipeline of public operations must deliver between
components that claim
[Arrow Geometry Semantics 1.0](../data/ARROW-GEOMETRY-SEMANTICS-1.0.md). It is
opt-in, like that contract: a component claims it in its adoption manifest,
and an interoperability suite holds to it only the components that claim
it. It adds no key, no value and no operation, and changes neither
[Composition 1.0](COMPOSITION-1.0.md), whose matrix the chains follow, nor
any catalog or binding.

## 2. Vectors

The vectors in [`vectors/interop-v1`](../../vectors/interop-v1/) fix what a
pipeline of public operations must deliver between components that claim
[Arrow Geometry Semantics 1.0](../data/ARROW-GEOMETRY-SEMANTICS-1.0.md), so
that a suite running the same chain through every surface (Rust, Python,
CLI, runtime) compares each result with one expectation instead of with
another implementation. Their shape is
[`interop-vector-v1.schema.json`](../../schemas/interop-vector-v1.schema.json).

**INTEROP-001** — A vector is one of three kinds:

- `handoff`: an input table (a vector of
  [`vectors/arrow-data-v1`](../../vectors/arrow-data-v1/)), a chain of steps
  and the exact table the chain delivers;
- `rejection`: an input table, one step and the error axes that step reports
  (REJ-001 to REJ-003);
- `source`: a source document, the reading step and the coordinates and axis
  order the step must declare (GEO-002, GEO-003): a GeoJSON document, whose
  format fixes longitude first, or a CSV document with a `wkt` column, whose
  order and CRS the read request states. The read request names the format
  of the document (`geojson` or `wkt_csv`); a request that names another
  format, or none, contradicts the document and the vector is refused.

**INTEROP-002** — Consecutive steps of a chain are a `direct` edge of the
matrix, or a write followed by a read of the same component on the same
target (`via: target`). The first step reads the input table as Arrow IPC;
the output of a chain is the table its last step delivers or writes as Arrow
IPC. Both ends are checked, not only the consecutive pairs, from what the
catalog declares for each step and never from the name of its operation: a
step reads the input table as Arrow IPC when its catalog input carries the
Arrow interchange contract with an Arrow IPC media type, or when its catalog
input contract selects a source format, its catalog output carries that
contract and its `format` parameter is `arrow_ipc_file` or
`arrow_ipc_stream`; a step delivers a table when its catalog output carries
that contract, and writes it as Arrow IPC when its catalog input contract
selects a sink format, its catalog input carries that contract and its
`format` is one of those two. The input contracts that select a format are
`plenora-io-read-input-v1` (source) and `plenora-io-write-input-v1` (sink);
a step whose input contract selects none, such as `database.read` or
`database.write`, whose catalog sides equal those of `io.read` and
`io.write`, names no `format`, and a vector in which it does is refused. A
step whose input contract selects a format names one of the closed list
`arrow_ipc_file`, `arrow_ipc_stream`, `gpkg`, `geojson` and `wkt_csv`. Both
lists are closed: a new input contract that selects a format, or a new
format, is added to them explicitly, in this specification and in the
validator, never inferred. In a `via: target` pair the first step's catalog
input and the second step's catalog output carry the Arrow interchange
contract, and when either step's input contract selects a format the read
names the format the write wrote. A `rejection`
step reads the input table in the same way; a `source` step reads its
source document instead. Step parameters such as a plan, a sink format or a read request are
abstract: the harness maps them to each surface's spelling. A `data.run`
step names the plan `identity` or a registered kernel. A step MAY name, as
a list, the limits its component declares (`one_geometry_field`, GEO-012;
`one_geometry_type`, GEO-010). The schema leaves `params` open, so every
parameter these rules read is checked for its type and value before any
step of the vector is evaluated, and a vector with an invalid one is
refused without evaluating its steps.

**INTEROP-003** — A step changes the table only through the transformations it
declares, from this closed list, applied in the order of the table; only the
operation the table names may declare one. Everything else, data and
metadata, is compared exactly and by type (`1`, `1.0` and `true` differ),
byte for byte for WKB, except the keys under the prefixes that
`expected_output` lists in `delegated_metadata`. Their values are the
provider's and its own vectors verify them; the harness records the keys
under those prefixes that the step declaring `provider_metadata` delivers,
and requires every later step to deliver them unchanged, so the comparison
removes from the final table only those recorded keys, after checking that
each is present with its recorded value. That observation needs the chain to
run: it belongs to the interoperability suite and to the provider's own
vectors, not to the validator of this repository, which checks that the
prefixes are declared.

| transformation | effect | owner | basis |
|---|---|---|---|
| `complete_missing_geometry_keys` | a geometry field receives the keys it omits, read without asserting more than it carries; `axis_order` becomes `unknown` when a CRS is declared without it | `data.run` | DT-ARROW-003, GEO-002 |
| `assign_field_ids` | a field without `plenora.field_id` receives the smallest free identifier, in field order | any step | GEO-013 |
| `large_to_standard` | `large_utf8` becomes `utf8` and `large_binary` becomes `binary` | `data.run` | the data-tools output contract (ARROW-010) |
| `srid_from_epsg_identifier` | a geometry field with a resolved `EPSG:<n>` and no `srid`, `n` within 32 bits, receives `srid=<n>` | `data.run` | the data-tools output contract (ARROW-010) |
| `ewkb_with_field_srid` | geometry values become EWKB with the extended type code (an ISO dimension code becomes the Z and M flags) and the field's `srid` on the outermost geometry, in the same byte order; `encoding` becomes `ewkb` | `database.read` | GEO-009 |
| `axis_order_unknown` | `axis_order` becomes `unknown` on a geometry field with a declared CRS: the step cannot establish the stored order | `database.read` | GEO-002 |
| `several_types_to_mixed` | a geometry field declared `exact` with several types becomes `mixed` without a list: the target keeps them in an unconstrained column | `database.read` | GEO-010 |
| `provider_metadata` | keys under the step's reviewed provider prefix are added and listed in `delegated_metadata` | `database.read` | ARROW-009 |

A loss that is not in the list is a defect of the step, never an expected
difference.

**INTEROP-004** — Every step is evaluated with the single order of REJ-002, the
operation included: the input's contract version, vocabulary and CRS, after
the acceptances of the step's profile (DT-ARROW-003 for `data.run`); then,
for a step that computes (a `data.run` whose plan is a registered `geo.`
kernel), the CRS it must use (GEO-015, GEO-006, DT-ARROW-004 for a
north-first order) and then its support (DT-ARROW-004: `geography` semantics
or non-planar edges are `unsupported`); then the limits the step names; then, for a step that decodes values (one that
computes, `database.write`, or `io.write` to a format other than Arrow IPC),
the values. A step that only carries or records the CRS is never expected to
refuse it (GEO-004), nor to check values it does not decode (GEO-009). A
`rejection` vector states the first rejection of that order: its class
(`input`, `support` or `crs`), category and rule. The schema classes expect
phase `validate` and remote effect `none`; a value class expects `read` for
a reading step, or `write` with `none` or `rolled_back` for a writing step
(GEO-018). Every rejection expects `retry.kind: never` and the CLI exit code
3. Every step of a `handoff` accepts its input. The operation-schema class
of REJ-001 depends on a target or a plan that the vectors of this version do
not describe; the components' own vectors exercise it.

**INTEROP-005** — The validator evaluates every step with that order,
recomputes every expected table from the input and the declared
transformations, checks every chain against the matrix and at both ends
(INTEROP-002) and reads every
source document itself. Its messages name rules and structural paths, never
a value or a data key. The vectors illustrate the rules and do not cover them
all: the phases `connect` and `probe`, an `unknown` remote effect with its
recovery (REJ-003) and the operation-schema class are not represented, and
their number does not attest full coverage.
