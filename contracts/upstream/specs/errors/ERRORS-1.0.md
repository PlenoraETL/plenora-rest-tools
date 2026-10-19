# Plenora Typed Error Contract 1.0

Status: normative

Contract identifier: `plenora-error-v1`

The machine shape is defined by the
[error v1 schema](../../schemas/error-v1.schema.json).

## 1. Applicability

Every failure crossing a public Plenora boundary MUST expose these four
independent axes:

- `category`: what failed;
- `phase`: when it failed;
- `remote_effect`: what is known about externally visible effects;
- `retry`: which retry behavior is permitted.

Consumers MUST make decisions from typed fields, never by parsing `message`.

## 2. Category

**ERR-001** — `category` MUST use the most precise value admitted by the
schema. Provider or dependency exception names are not public categories.

**ERR-002** — Unsupported operations, versions or providers MUST fail with
`unsupported` or a more precise validation category. They MUST NOT be silently
ignored or emulated with different semantics.

For an invalid Arrow input, the components that claim Arrow Geometry
Semantics 1.0 report the category of each class in the order it fixes
(REJ-001 to REJ-004); this is informative here.

## 3. Phase

**ERR-003** — `phase` identifies the last externally meaningful phase known to
have started. It does not expose private call stacks.

## 4. Remote effect

**ERR-004** — `remote_effect` MUST be conservative. When the component cannot
prove whether a remote mutation occurred, it MUST report `unknown`.

**ERR-005** — Timeout and cancellation MUST NOT be treated as proof of rollback.

## 5. Retry

**ERR-006** — `remote_effect: unknown` MUST NOT permit automatic safe retry.
It requires `never`, `quarantine` or `requires_recovery`.

**ERR-007** — `retry.kind: after` MUST include `delay_ms`. Other retry kinds
MUST NOT include it.

**ERR-008** — `requires_idempotency_key` is valid only when the public
operation advertises idempotency-key support.

**ERR-014** — When a remote mutation may have started and its outcome is not
proven, the error reports `remote_effect: unknown` (ERR-004), even when the
failure that ends the operation is local, such as writing a downloaded body,
and whatever the request method: a method's conventional safety is not proof.
An idempotency key does not change this (ERR-006). The retry disposition
SHOULD be `requires_recovery`, unless the component profile states another
of those ERR-006 admits.

**ERR-015** — When a publication or commit is proven and a later cleanup
fails, the error reports `phase: cleanup` and `remote_effect: committed`: the
result was published, so it is neither a success (SURF-014) nor `partial`.
`retry.kind` is `never` when the residue is local only, because a retry would
publish again, and `requires_recovery` when a remote residue remains. When the
remote outcome of the cleanup itself is unknown, ERR-014 applies.

**ERR-016** — An unproven outcome does not decide the category: `category`
names what failed (ERR-001), and the uncertainty is carried by
`remote_effect: unknown` (ERR-004) with a retry that ERR-006 admits,
normally `requires_recovery`. A commit whose confirmation was lost because
the connection or the stream failed is `io`, phase `commit`; one whose
deadline elapsed is `timeout`; `internal` is reserved for a defect of the
component, such as a panic or an unhandled exception (CLI 2.0 section 8:
exit codes 5, 5 and 70).

## 6. Message and details

**ERR-009** — `message` is diagnostic text for people. It MUST be bounded and
redacted.

**ERR-010** — Public error data MUST NOT contain credentials, authorization
headers, connection strings, bound SQL, source rows, unbounded payloads or local
secret paths.

**ERR-011** — The compact UTF-8 JSON encoding of a public error MUST NOT exceed
524,288 bytes. The compact UTF-8 JSON encoding of `details` MUST NOT exceed
262,144 bytes.

**ERR-012** — Within `details`, counting the `details` object as depth 1:

- nesting depth MUST NOT exceed 8;
- an object MUST NOT contain more than 128 properties;
- an array MUST NOT contain more than 128 items;
- a string value MUST NOT exceed 4,096 UTF-8 bytes;
- the complete value MUST NOT contain more than 2,048 JSON nodes, including
  containers and scalar values.

These are semantic byte and aggregate limits. Producers and conformance tests
MUST enforce them in addition to JSON Schema validation.

**ERR-013** — When a component profile requires typed error details, `details`
MUST contain exactly the component-owned, versioned shape declared by that
profile. Contract identifiers are immutable. Evolving the shape requires a new
identifier; consumers MUST NOT infer the shape from `code` or `message`.

`code`, `provider`, `execution_id` and `details` MAY provide structured
context. Their absence MUST NOT change the meaning of the four common axes.

## 7. Surface projection

The CLI projects categories to exit codes as defined by CLI 2.0. Python exposes
the axes on `PlenoraError`. Rust and runtime bindings MAY use native tagged
types or serialized objects, but MUST preserve all four axes and their meaning.
