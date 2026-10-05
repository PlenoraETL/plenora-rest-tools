# Plenora Runtime Conformance Vectors 1.0

Status: normative

Contract identifier: `plenora-runtime-vector-v1`

The fixtures in [`vectors/runtime-v1`](../../vectors/runtime-v1/) make the
Runtime Binding 1.0 metadata and result rules directly reusable by
`runtime-tools` and each domain component.

## 1. Request vector

A request vector contains the exact routing identity, operation version, input
contract and content type. A component test MUST reject any mutation that makes
these values disagree with its capability descriptor before invoking domain
functionality.

Every vector models the runtime envelope identities in metadata. It carries
`plenora.message.id` and `plenora.trace.correlation_id` as canonical lowercase
hyphenated UUIDs, and may carry `plenora.message.causation_id` in the same
form. Request metadata additionally carries the runtime capability identity;
success and error metadata carry the output contract.

The payload is illustrative operation data. It deliberately uses protected
references and MUST NOT be replaced with real credentials in committed test
evidence.

## 2. Success vector

A success vector preserves operation version, output contract and correlation
identity. Returning only an acknowledgement for a result-producing operation
does not satisfy the fixture.

## 3. Error vector

An error vector uses `application/vnd.plenora.error+json`, identifies
`plenora-error-v1` and carries all common error axes. Unknown remote effect is
paired with `never`, `quarantine` or `requires_recovery` (ERR-006), never
automatic retry.

## 4. Use by adopters

`runtime-tools` tests envelope transport and metadata preservation. A domain
component tests selector validation, invocation and semantic result/error
mapping through its public runtime adapter. Neither test needs access to the
other component's private types.

`runtime-tools` adopts these obligations through the separate
[runtime-tools transport profile](../../profiles/runtime-tools.md). That profile
does not make it a sixth domain library.

The structural shape is defined by
[`runtime-vector-v1.schema.json`](../../schemas/runtime-vector-v1.schema.json).
The repository semantic validator also checks the vectors against the public
catalogs and typed error schema.

## 5. Required conformance matrix

The runtime-v1 fixture set is a transport matrix, not a definition of any
component-owned payload. It includes:

- requests for database, data, IO, REST and storage selectors;
- JSON and Arrow success envelopes;
- deadline and causation metadata;
- opaque secret, endpoint and artifact references, including storage source
  and sink roles with bounded metadata, explicit overwrite and publication
  policy;
- typed failures with `none` and `unknown` remote effects;
- safe retry and recovery-required dispositions.

The storage subset includes requests for all seven selected operations, list
and get/put success envelopes, and partial/unknown transfer errors. Its payload
counterexamples are exercised as mutations by
[`test_storage_contracts.py`](../../tools/test_storage_contracts.py); they cover
explicit policies, source/sink roles, bounded artifact references, secret/path
rejection, integrity, byte counts, cursors and unsafe retry. These tests validate
the fixture semantics, not a provider implementation.

An adopter MUST exercise every fixture whose operation it advertises. The
runtime transport consumer MUST exercise the complete set. For every request
fixture it MUST also demonstrate fail-closed rejection when capability,
operation, operation version or input contract routing is missing or invalid.
Those negative probes are mutations of the valid fixtures and are not stored
as `plenora-runtime-vector-v1` documents.

Fixture payload objects remain illustrative. A component validates them using
its own immutable payload schema; runtime-tools preserves their serialized
bytes and does not infer domain semantics from their fields.
