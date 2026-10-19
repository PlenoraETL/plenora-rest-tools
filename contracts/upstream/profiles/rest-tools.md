# Public profile: rest-tools

Profile identifier: `plenora-rest-tools-profile-v1`

Normative target catalog: [`rest-tools-v1.json`](../catalogs/rest-tools-v1.json)

## Applicable contracts

- [Public Surfaces 1.0](../specs/surfaces/PUBLIC-SURFACES-1.0.md)
- [Capability Discovery 2.0](../specs/capabilities/CAPABILITY-DISCOVERY-2.0.md)
- [Typed Errors 1.0](../specs/errors/ERRORS-1.0.md)
- [Public Security 1.0](../specs/security/PUBLIC-SECURITY-1.0.md)
- [Python SDK 1.0](../specs/sdk/PYTHON-SDK-1.0.md)
- [CLI 2.0](../specs/cli/CLI-2.0.md), when exposed
- [Runtime Binding 1.0](../specs/runtime/RUNTIME-BINDING-1.0.md)
- [Arrow Interchange 1.0](../specs/data/ARROW-INTERCHANGE-1.0.md), when advertised
- [Surface Bindings 1.0](../specs/surfaces/SURFACE-BINDINGS-1.0.md)
- [Composition 1.0](../specs/composition/COMPOSITION-1.0.md)

## Public purpose

The component exposes a stable black-box HTTP execution surface. HTTP client,
pool, authentication flow and retry implementation remain private.

## Required operation families

The stable public catalog includes:

- `rest.test`;
- `rest.generate`;
- `rest.enrich`;
- `rest.download`;
- `rest.upload`.

Each operation descriptor declares input and output contracts, side effects,
execution controls and externally relevant limits. Supported HTTP methods,
authentication modes, response formats, pagination, polling and transfer
behavior are structured capability attributes rather than hidden inference.

## Component-owned contracts

The component owns and versions:

- `plenora-rest-execution-request-v1`;
- `plenora-rest-execution-result-v1`;
- `plenora-rest-file-transfer-input-v1`;
- `plenora-rest-file-transfer-result-v1`;
- `plenora-rest-capability-attributes-v1`.

Their normative schemas and operation specifications live in the adopting
`rest-tools` repository. The common catalog references their immutable
identifiers without centralizing provider or implementation details.

## Public surfaces

- Rust API: required.
- CLI: optional (`conditional` in the catalog). An artifact MAY ship the
  `plenora-rest` command; when it does, the command is governed by CLI 2.0,
  exposes the five operations with the spellings of
  [`bindings/cli-v1.json`](../bindings/cli-v1.json) and lists `cli` among the
  surfaces of each operation in its capability document. An artifact without
  the command omits `cli` and remains conforming.
- Python SDK: required and governed by Python SDK 1.0.
- Runtime: required for every REST operation selected for orchestration.

The v1 catalog selects all five REST operations for the runtime surface; the
runtime target is therefore required, not conditional.

Equivalent Rust, Python and runtime operations preserve operation status,
output meaning and the common typed error axes.

## Artifact semantics

Rust and in-process Python callers may provide an explicitly authorized local
path as an input convenience. Serialized runtime requests use opaque artifact
references.

Public results identify an artifact through bounded metadata and an opaque
reference. They never echo private local paths.

The file-transfer input carries the REST connection configuration, artifact
source or sink, content type, size limits and optional integrity constraints.
Raw file bytes are not embedded in persistable JSON requests.

## Idempotency

All five operations accept an opaque idempotency key. Rust and Python carry it
as an execution control; serialized runtime requests carry it only as
`plenora.execution.idempotency_key` metadata.

The component MUST validate and bound the key, reject reuse with a different
contract input while that key remains in its local execution scope, and deliver
a stable key to the explicitly configured remote header, query field or body
field. Multi-request operations derive stable child keys so different remote
requests never share one provider key. Durable deduplication across process
restarts remains a property of the target service or the owning runtime; the
component MUST NOT claim that forwarding a key proves remote deduplication.

## Asynchronous HTTP jobs

Polling remains a behavior of the five stable operations, not a provider or
queue-specific operation. A polling configuration MAY start a remote job or
resume a previously observed job. Resume MUST skip the original submission and
continue from the configured status endpoint.

When an unfinished remote job has a bounded public identifier, an ambiguous or
interrupted result exposes the component-owned
`plenora-rest-async-job-recovery-v1` contract. The handle contains no
credential, authorization material, private path or signed polling URL. The
caller retains the original connection and credential reference and supplies
the handle's job identifier to a later resume invocation.

A polling configuration MAY declare a bounded best-effort remote cancellation
request for cooperative cancellation, deadline expiry or polling timeout.
Acceptance of that request is observable, but never proves rollback; the
failure keeps a conservative remote effect and the recovery handle remains
usable.

Message brokers, worker leases, acknowledgement, visibility timeout,
backpressure, durable scheduling and dead-letter queues remain owned by the
runtime or remote service and are outside this component.

## Interchange

JSON is the default structured representation. Binary and file transfer
operations declare their content type and artifact contract explicitly.

Arrow is optional and applies only if an operation advertises Arrow input or
output. REST responses MUST NOT be represented as Arrow merely to satisfy this
profile.

## External security

TLS verification is secure by default. Insecure networking, private-network
access, redirects, custom methods and local file transfer are explicit,
discoverable opt-ins.

Credentials, tokens, PEM material, authorization headers, body previews and
local paths MUST NOT appear in capability documents, public errors or persisted
diagnostics.

Conformance note (informative): this repository's gate checks the REST runtime
examples positively for artifact references (an opaque `scheme:` or `scheme://`
reference, nothing else) and heuristically for private paths and inline
credentials elsewhere: a string that is a valid opaque reference (the
`reference` grammar of `data-execution-input-v3.schema.json`) is never judged
as a path; any other string is a path when it has a drive, root, UNC, home,
`.`/`..`, `file:`, environment-variable or backslash spelling; a member name is
a credential when, after NFKC and case folding and without `_`, `-`, `.` and
spaces, it contains a secret word; authorization and PEM values are credentials
under any name. A relative path with forward slashes or none (`dir/report.csv`,
`report.csv`) in a free field, or a secret spelled otherwise, can pass that
heuristic; the property itself is shown by the adopter's boundary tests, not by
the gate.

## Not specified here

This profile does not prescribe the HTTP client, connection pool, DNS resolver,
retry loop, Python binding technology or internal JSON representation.
