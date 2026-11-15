# Plenora File Interchange Contract 1.0

Status: normative

Contract identifiers: `plenora-file-interchange-v1`,
`plenora-file-interop-vector-v1`, `plenora-file-composition-v1`.

## 1. Scope

This opt-in contract governs a file passed between public operations.
It does not assert that a component release implements the edge. Adoption
manifests and component interoperability suites provide that evidence.

**FILE-001** — A descriptor MUST validate against
[`file-interchange-v1.schema.json`](../../schemas/file-interchange-v1.schema.json).
It declares the exact media type (lowercase, without parameters), byte count,
lowercase hexadecimal SHA-256 of the complete file, producing component,
operation and version, format family and reference. The producing operation
is the operation handing off the file, including a retrieval operation;
it need not be the original author of the bytes.

## 2. References and integrity

**FILE-002** — Delivery MUST be by reference to the exact bytes. A consumer
MUST NOT re-encode, normalize line endings, infer a replacement format, or
silently transcode a file to satisfy an edge. Descriptor metadata describes
the file, not the JSON envelope that reports it. Format families are explicit
capability identifiers, such as `geojson` or `arrow_ipc_file`; no wildcard or
implicit acceptance follows from a matching media type.

**FILE-003** — On CLI, `local_path` identifies a local file; a relative path
is resolved against the invocation's working directory. The harness MUST
materialize a fixture there or replace the fixture reference with its actual
path. On runtime, `authorized_reference` carries only an opaque token in the
`artifact:` namespace. Its suffix has no path semantics. Runtime MUST NOT
accept private local paths, file URLs, embedded bytes or caller-supplied
filesystem locations in place of this token. The resolver MUST authorize
access for the current caller and consumer before returning bytes; a token's
syntactic validity is not authorization. No `authorized: true` assertion
from the producer grants access.

**FILE-004** — Before invoking the consuming operation or publishing bytes,
the handoff layer MUST check the consumer's explicit format-family and
media-type declarations, the resolver's media type, the complete byte count,
and SHA-256. It MUST verify and consume the same immutable snapshot, or hold
access that prevents replacement between verification and use. It MUST reject
missing authorization or an unreadable reference without invoking the consumer.
The consumer still applies its operation-specific format and fidelity checks;
a digest proves integrity, not format validity.

**FILE-005** — For the vector cases, rejection precedence is:
undeclared format family, content-type mismatch, short payload, excess payload,
then digest mismatch. Their causes are respectively
`undeclared_format_family`, `content_type_mismatch`, `truncated_payload`,
`size_mismatch`, `digest_mismatch`. The expected axes are category `schema`,
phase `validate`, remote effect `none`, retry `never`. These describe the
handoff before consumer invocation, not effects of the already completed
producer. A full typed error maps retry to `{"kind": "never"}`.
The message MUST be the fixed text `File handoff rejected.` and MUST NOT
include bytes, reference tokens, paths, media types, digests or document values.
Authorization and resolver failures follow the shared
[typed error contract](../errors/ERRORS-1.0.md).

## 3. Additive composition

**FILE-006** — The opt-in file matrix is
[`file-pipelines-v1.json`](../../composition/file-pipelines-v1.json), shaped by
[`file-composition-v1.schema.json`](../../schemas/file-composition-v1.schema.json).
Each endpoint explicitly declares `artifact_form: file_reference` separately
from its catalog envelope contract. Both endpoints MUST exist at the exact
catalog operation version, declare the surface and match that envelope.
A direct edge MUST use this file interchange contract on both artifact sides
with identical format family and content type. Implementations MUST advertise
and negotiate those artifact capabilities before using the edge. Merely
supporting the catalog's JSON envelope does not constitute adoption.

The declared initial edges are `storage.get@1 -> io.read@2` and
`io.write@2 -> storage.put@1`, each for GeoJSON on CLI and runtime.
CLI and runtime keep these exact versions; their entrypoints are mapped by
the existing [CLI](../../bindings/cli-v1.json) and
[runtime](../../bindings/runtime-v1.json) bindings. Runtime qualification still
requires execution through the authorized artifact resolver.

Rationale: composition-v1 closes its endpoint and edge shapes and cannot
express per-side artifact forms. Its direct-edge checks concern catalog
envelopes, which are not file metadata. A separate file composition identifier
therefore adds an opt-in matrix without reinterpreting
[Composition 1.0](COMPOSITION-1.0.md), modifying `pipelines-v1.json`, or
adding interchange claims to published catalogs.

## 4. Normative vectors

**FILE-007** — Every JSON file in
[`vectors/file-interop-v1`](../../vectors/file-interop-v1/) is normative and
validates against
[`file-interop-vector-v1.schema.json`](../../schemas/file-interop-vector-v1.schema.json).
Its edge MUST be present in the file matrix. Its descriptor MUST agree with
the edge's producer, surface, family and type. `consumer` is the capability
advertisement under test; refusal vectors intentionally omit a capability.
`resolved.bytes_hex` encodes exact fixture bytes for the test harness only,
never the production descriptor or wire handoff. `resolved.content_type`
is the resolver's independent metadata.

The reference evaluator checks the bytes and computes the expectation;
it MUST NOT open paths or resolve tokens taken from vectors. A component
suite MUST materialize these bytes, execute the producer and consumer through
the chosen surface, and compare the actual handoff with the expectation.
The positive case MUST preserve every byte. Each edge and surface has a
positive case and the four required rejection cases.
Repository validation alone is not execution of storage or IO implementations.
Validator diagnostics MUST contain only fixed rule text and structural
identifiers, following [decision 0021](../../decisions/0021-messages-without-data.md).

## 5. Qualification scope and change statement

This increment ratifies only the two file edges above. Other candidates in
the [full matrix proposal](../../proposals/full-interop-matrix.md) remain
unratified; phase 1 of that proposal is not completed by this contract.
In particular, storage/data and storage/database Arrow IPC file edges,
REST download/upload edges, and data/database-to-storage edges need separate
artifact declarations and vectors before use.

`database.query` remains an Arrow producer outside this file matrix;
`data.describe` remains outside its file consumers. `io.convert` is excluded
from these initial edges because conversion is an operation with distinct
input and output artifacts, not an identity file handoff. `rest.generate`
produces structured results and needs a declared serialization before a file
edge can apply. `data.run@1` and `@2` are outside this increment; `@2` has no
runtime binding and MUST NOT be substituted by `@3`. Runtime-only `@3` needs
its own artifact mapping and cannot be promised on CLI.

Affected consumers are composing applications, IO and storage adopters.
Previously these file edges had no shared integrity and reference contract;
adopters now have exact, testable artifact semantics. This is an additive
change with new schema and contract identifiers. Published catalogs,
bindings, profiles, examples and Arrow vectors are unchanged. IO and storage
must explicitly adopt the new contracts and prove their respective sides;
runtime adopters additionally provide authorized resolution. Data, database
and REST gain no conformance claim from these vectors.
