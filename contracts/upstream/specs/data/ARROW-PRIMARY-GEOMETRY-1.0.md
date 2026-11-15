# Plenora Arrow Primary Geometry 1.0

Status: normative

Contract identifier: `plenora-arrow-primary-geometry-v1`

## 1. Scope

[Arrow Geometry Semantics 1.0](ARROW-GEOMETRY-SEMANTICS-1.0.md) admits several
`geoarrow.wkb` fields in one schema, each with its own metadata (GEO-012), but
no key says which of them is the table's primary geometry, the one an
operation takes when its request names none and the one a format that has a
primary column, such as GeoParquet, declares. Arrow has no standard way to
say it. This contract adds one schema metadata key that says it, and the
rules that keep its meaning across operations and components.

It is opt-in. A component claims it in its adoption manifest, next to Arrow
Geometry Semantics 1.0, which it requires; from then on it follows every rule
of this document on every surface that exchanges Arrow. It changes neither
`plenora.contract.version`, nor any key or value of
[Arrow Vocabulary 1.0](ARROW-VOCABULARY-1.0.md), nor any rule of Arrow
Geometry Semantics 1.0. A component that does not claim it reads the key as
unknown schema metadata (ARROW-010). A pipeline relies on these rules only
between components that claim them (section 9).

## 2. The key

**PRIM-001** — The primary geometry is declared in schema metadata, under
`plenora.primary_geometry`, with one of two values:

- the `plenora.field_id` of a `geoarrow.wkb` field of the schema, written as
  that field writes it: a non-negative decimal integer without leading zeros
  that matches the whole value (Arrow Vocabulary 1.0 section 2, GEO-016);
- `none`: the table has no primary geometry.

The reference is the field's identity, never its name or its position, so
that a rename or a reordering does not change it (ARROW-003, ARROW-004). The
key is a schema key: it never appears in field metadata.

**PRIM-002** — The primary state of a schema follows from its geometry fields
and the key:

| geometry fields | key absent | `none` | a field identity |
|---|---|---|---|
| none | no geometry | not conforming | not conforming |
| one | that field is the primary | no primary | that field |
| two or more | no primary | no primary | that field |

A schema with one geometry field and no key reads as it did before this
contract: that field is the primary. A schema with several geometry fields
and no primary is conforming and may cross every operation that does not
need a primary; GEO-012 is unchanged, and every geometry field keeps its own
metadata, CRS included, whatever the primary.

**PRIM-003** — A schema is not conforming when the key is outside the grammar
of PRIM-001, stands in field metadata, stands on a schema without any
geometry field, or names an identity that no field carries or that a field
without `geoarrow.wkb` carries. The key is resolved against the schema as the
producer sent it, before any completion a profile admits (DT-ARROW-003): an
identity the consumer would assign is not one the producer named.

**PRIM-004** — A component writes the key exactly when its absence would mean
another state: `none` for one geometry field without a primary, the field
identity for two or more with a primary, and no key otherwise. A consumer
accepts the other forms PRIM-002 gives a meaning (`none` with several
geometry fields, an identity with one) with that meaning, and emits them in
this form; the rewriting changes no state and is the normalization this
contract states (ARROW-010). A schema with one geometry field and a primary
is therefore unchanged on the wire.

## 3. Operations

**PRIM-005** — The primary state belongs to the table, and an operation keeps
it:

- the primary is the field with its identity: an operation that renames,
  reorders, filters, sorts or carries it keeps the primary on that field;
- an operation that removes the primary field leaves the table without a
  primary. It never promotes another field, by position, name, type, CRS, or
  because only one is left: one geometry field left is emitted with `none`;
- an operation that adds geometry fields keeps the primary, and writes the
  key when the table passes from one geometry field to two;
- a field an operation produces becomes the primary only when the input had
  no geometry field, when the request names it, or when the operation's
  published output contract declares that it replaces the primary, such as
  a transformation of the primary in place.

Changing the primary of a table is an explicit request, whose spelling is
owned by the component and its catalog.

**PRIM-006** — An operation that combines several inputs never elects a
primary. When it places their fields side by side, such as a join, and every
geometry field of its output comes from one input, the output keeps that
input's primary state by field provenance: the primary survives only if
its field survives, and an input without a primary remains without one.
If output identities change, the primary reference follows the input-to-output
field mapping; it is never copied as a bare numeric ID. When the output
carries geometry fields of two or more inputs, it has no primary unless the request names one, whatever the
inputs declared, their order, their names or their CRS. When it feeds the
fields of several inputs into the same output fields, such as an append or
a union, the output field that the primary of every input feeds is the
primary; if an input has no primary, or the primaries of the inputs feed
different output fields, the output has no primary. This correspondence is
established by field provenance, not by numeric equality of identities across
inputs: equal IDs can feed different output fields, and different IDs can
feed the same output field.

**PRIM-007** — An operation that computes with one geometry of an input takes
the field its request selects by identity and, when the request selects
none, the primary of that input. Without a selection and without a primary,
or with a selection that names no geometry field of the input, the operation
rejects the input in the operation-schema class of REJ-001 (`schema`),
before any effect, with phase `validate` (REJ-003); it never takes the first
or the only geometry field. The selection names a field of the input; the
spelling of the request parameter is the component's. An explicit selector
is resolved against the schema as received, before any profile completion
(DT-ARROW-003). An identity absent there is rejected in the operation-schema
class (`schema`), even if completion would assign that identity to a field.
After resolution, the operation uses the completed metadata of that same
field. This rejection follows REJ-002, including preceding support limits.
In the order of REJ-002 the CRS the operation must use (GEO-006, GEO-015) and its support of
the field (such as `geography` semantics) are those of the operand only; the
missing operand is reported after the support limits; the operation decodes
the values of the operand and carries the other geometry fields without
decoding them (GEO-009). Every geometry field remains subject to input
conformance (Arrow Vocabulary 1.0 section 4, GEO-005, GEO-016), whatever the
operand. Computing with a field that is not the primary leaves the primary
unchanged.

## 4. GeoParquet

**PRIM-008** — GeoParquet metadata (`geo`) and the key correspond exactly:

- a reader delivers the field that `primary_column` names as the primary,
  in the form of PRIM-004, and every column of `columns` as a geometry field
  with its own metadata. A native key it records, such as
  `plenora.geometry.native.geoparquet.primary_column`, is provenance of the
  file (ARROW-009): no component reads it to decide the primary, and it
  never overrides the key;
- a writer names the primary in `primary_column` and lists every geometry
  field in `columns`. A table without a primary is rejected with
  `unsupported`, the support class of REJ-001, before any effect: GeoParquet
  requires `primary_column`, and the writer never chooses one;
- a sink that stores one geometry field per table, such as GeoPackage, keeps
  GEO-012: it rejects several geometry fields with `unsupported` and never
  writes the primary alone.

A write followed by a read of the same file therefore delivers the same
primary, on the field of the same name.

## 5. Rejection categories

The defects of PRIM-001 and PRIM-003 belong to the vocabulary class of
REJ-001 (`schema`). In the order of REJ-002 they come after the checks of
every field of that class, so that a repeated field identity is reported
first, and before the CRS class. The missing operand of PRIM-007 belongs to
the operation-schema class (`schema`), and the writer's rejection of
PRIM-008 to the support class (`unsupported`). None of them is reported as
`invalid_plan` or `invalid_configuration`. REJ-003 gives the other axes:
phase `validate`, `remote_effect: none`, `retry.kind: never`, CLI exit code 3.

## 6. Vectors

**PRIM-009** — The vectors of this contract are:

- [`vectors/arrow-primary-v1`](../../vectors/arrow-primary-v1/): schema
  fixtures in the shape of
  [`arrow-data-vector-v1.schema.json`](../../schemas/arrow-data-vector-v1.schema.json),
  whose `expect` is the input-conformance verdict of a component that claims
  this contract and Arrow Geometry Semantics 1.0;
- [`vectors/interop-primary-v1`](../../vectors/interop-primary-v1/): chains
  in the shape of
  [`interop-primary-vector-v1.schema.json`](../../schemas/interop-primary-vector-v1.schema.json),
  evaluated with the rules of
  [Interoperability Vectors 1.0](../composition/INTEROP-VECTORS-1.0.md) and of
  this contract: every step judges the key, emits it in the form of PRIM-004,
  and a `data.run` step whose plan is a `geo.` kernel takes its operand by
  PRIM-007 from the field identity its `geometry` parameter names, if any. A
  `geometry` parameter on a step that does not compute, or that is not a
  field identity, is refused, never ignored;
- [`vectors/primary-composition-v1`](../../vectors/primary-composition-v1/):
  provenance fixtures for PRIM-006 in the shape of
  [`primary-composition-vector-v1.schema.json`](../../schemas/primary-composition-vector-v1.schema.json).
  Each input lists its geometry identities and semantic primary (`null` means
  no primary, not an omitted wire key). `mapping` lists surviving geometry
  fields by zero-based input index, input field identity and output identity.
  It defines the input-to-output correspondence; omitted fields are dropped.
  `expected_primary` is the output state and `expected_key` the canonical
  PRIM-004 wire value (`null` means omission). These fixtures describe field
  provenance, not a component's request syntax or row computation;
- [`vectors/geoparquet-primary-v1`](../../vectors/geoparquet-primary-v1/): the
  correspondence of PRIM-008 in the shape of
  [`geoparquet-primary-vector-v1.schema.json`](../../schemas/geoparquet-primary-vector-v1.schema.json),
  for a writer and for a reader.

The validator derives every verdict from the rules, and checks that every
vector of `vectors/arrow-data-v1` keeps its verdict under them. The vectors
illustrate the rules and do not cover them all: the operations of PRIM-005
other than carrying a table, row computation for PRIM-006, the CRS and the
other members of the GeoParquet `columns`, and GeoParquet files themselves are left to the
components' own vectors.

## 7. Informative: what this contract does not fix

- The spelling of the selection and of the change of primary in a
  component's requests and catalogs.
- How a component represents the primary internally.
- Which operations accept several geometry fields: GEO-012 lets a component
  refuse them with `unsupported`, and its discovery results say so.

## 8. Informative: compatibility

A schema with at most one geometry field and no key, every schema published
before this contract, keeps its meaning and its verdict. A schema with
several geometry fields and no key keeps the meaning it had under Arrow
Geometry Semantics 1.0, which named no primary.

## 9. Informative: components that do not claim the contract

A component that claims only Arrow Geometry Semantics 1.0 carries the key as
unknown metadata. Two hazards follow, which is why a pipeline relies on the
key only between components that claim the contract:

- an operation of such a component that removes or renumbers fields can leave
  a key that names no field, which a claiming consumer rejects (PRIM-003), or
  one that names another geometry field, which no consumer can detect;
- such a component reads one geometry field declared `none` as the primary,
  as Arrow Geometry Semantics 1.0 alone allows.
