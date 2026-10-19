# Plenora Runtime Binding Contract 2.0

Status: normative

Contract identifier: `plenora-runtime-binding-v2`

This contract maps public operations to the transport-neutral capability
boundary exposed by runtime-tools. It governs serialized requests and results,
not adapters, handlers, queues or worker implementation.

Version 2.0 keeps every rule of
[Runtime Binding 1.0](RUNTIME-BINDING-1.0.md) and the wire format of its
requests and results, and adds RT-024 (section 14) and the negotiation
between the two versions (section 15). Section 16 lists what differs; the transition is in
[COMPATIBILITY.md](../../COMPATIBILITY.md#public-catalogs-and-runtime-binding-10-to-20).

## 1. Applicability

An operation follows this contract only when its capability descriptor lists the
`runtime` surface.

## 2. Capability identity

**RT-001** — The runtime capability name for component
`plenora-<domain>-tools` is `plenora.<domain>-tools`.

RT-002 of 1.0, which made the capability version the contract version,
does not apply to this version; RT-025 replaces it, and an identifier is
never reused with another meaning.

**RT-025** — Capability version `1` identifies the wire format of runtime
requests and results, which this version keeps from 1.0: a request of 2.0
carries `plenora.capability.version` `1`. The contract version an artifact
follows is the one its capability document states for the `runtime`
interface (CAP-003): `plenora-runtime-binding-v2` with version `2`. Both are
independent from component release and operation contract versions.

**RT-003** — The runtime operation selector carries the complete stable public
operation identifier, for example `database.read` or `geo.buffer`.

## 3. Request metadata

A runtime request carries these reserved metadata values:

| Key | Meaning |
|---|---|
| `plenora.capability.name` | runtime capability identity |
| `plenora.capability.version` | wire format version, `1` in this version (RT-025); never the contract version |
| `plenora.capability.operation` | stable public operation identifier |
| `plenora.operation.version` | operation contract version |
| `plenora.input.contract` | versioned input contract identifier |
| `plenora.trace.correlation_id` | originating correlation identity |

**RT-004** — Routing and contract metadata MUST agree with one available
operation advertised by Capability Discovery 2.0. Mismatch fails before
invocation with `protocol` or `unsupported`.

**RT-005** — The serialized payload content type MUST be one of the input
content types advertised for that operation version.

The runtime message envelope carries these identity keys:

| Key | Requirement |
|---|---|
| `plenora.message.id` | required unique message identity |
| `plenora.trace.correlation_id` | required originating correlation identity |
| `plenora.message.causation_id` | optional direct-cause message identity |

**RT-012** — Envelope identities MUST use the canonical lowercase hyphenated
UUID representation. Alternate key spellings and non-canonical UUID text are
not compatible aliases. Message and optional causation identities remain
observable to the caller; results preserve the originating correlation UUID.

## 4. Execution controls

When advertised by the operation, the request MAY also carry:

- `plenora.execution.deadline`: an absolute RFC 3339 UTC timestamp;
- `plenora.execution.idempotency_key`: an opaque bounded key.

Cancellation is propagated by the runtime invocation context.

**RT-006** — A control MUST NOT be accepted silently when the operation
descriptor declares it unsupported.

**RT-007** — Deadline, cancellation and idempotency behavior MUST preserve the
semantics of the same operation version on its other public surfaces.

## 5. Artifact-bearing requests and results

**RT-013** — A persistable serialized input that identifies a file or other
artifact MUST carry an opaque artifact reference. Private local paths MUST NOT
cross the runtime boundary. The reference is resolved only through an
authorized runtime resource. When the component contract declares artifact
metadata, it is bounded and keeps content type, byte size and a genuinely
calculated SHA-256 separate from provider-specific identifiers.

**RT-014** — An artifact result MUST preserve its output contract, content
type, originating correlation identity, byte count and checksum algorithm and
value. The concrete transport or storage mechanism for the artifact is
implementation-specific.

**RT-015** — Artifact source and sink resolution belongs to the final
application boundary. A component may expose transport-neutral resolver traits
but MUST NOT require a core runtime-tools crate to depend on the component.

## 6. Success result

A successful runtime invocation returns serialized output with:

- content type advertised by the operation;
- `plenora.operation.version`;
- `plenora.output.contract`;
- the originating correlation identity.

**RT-008** — A result-producing operation MUST return its public result. A
successful acknowledgement with no result is conforming only when the declared
output contract explicitly represents an empty acknowledgement.

**RT-009** — Streaming and artifact-reference outputs MAY use transport-specific
delivery, but every delivered result MUST preserve the advertised content type,
output contract and correlation identity.

## 7. Failure result

**RT-010** — A public runtime failure MUST preserve the common error axes from
`plenora-error-v1`. An adapter-specific retry class is not a substitute for
the public category, phase, remote effect and retry disposition.

A serialized error result uses content type
`application/vnd.plenora.error+json` and output contract
`plenora-error-v1`.

**RT-011** — Unknown operation, version, input contract or content type fails
before invocation with `remote_effect: none`.

## 8. Security

Runtime requests follow
[Public Security 1.0](../security/PUBLIC-SECURITY-1.0.md). Persistable messages carry
connection or secret references and MUST NOT contain raw credentials, tokens,
authorization headers or private key material.

## 9. Compatibility

Adding optional metadata is compatible when its absence preserves previous
behavior. Renaming reserved keys, changing their meaning, changing routing
identity or weakening result/error semantics requires a new runtime binding
version. A new wire format also requires a new capability version (RT-025).

## 10. Canonical selectors and vectors

The exact operation selectors for the five component profiles are registered
in [`bindings/runtime-v1.json`](../../bindings/runtime-v1.json). Reusable
request, success and error fixtures are defined by
[Runtime Conformance Vectors 1.0](RUNTIME-VECTORS-1.0.md), with the shape
and the rules of its section 5. The fixtures that illustrate a rule of this
version only are in [`vectors/runtime-v2`](../../vectors/runtime-v2/): an
adopter of 2.0 exercises them in addition, under the same rules.

## 11. Rejection before invocation

These rules clarify sections 3, 4 and 7. Each one chooses, among the values
those sections already admit, the one a conforming consumer can rely on; none
admits a request or a result that 1.0 rejected. The reasons and the
compatibility of each are recorded in
[decision 0010](../../decisions/0010-runtime-rejection-and-identity.md).
[`vectors/runtime-probes-v1`](../../vectors/runtime-probes-v1/) states the
expected rejection of each probe as data ([RUNTIME-VECTORS-1.0
§6](RUNTIME-VECTORS-1.0.md#6-rejection-probes)).

**RT-016** — A request that fails RT-004, RT-005, RT-006, RT-011, RT-012 or a
rule of this section is rejected before invocation: no domain functionality
runs. The error has `phase: validate` (ERR-003: no later phase started),
`remote_effect: none` (ERR-004: the absence of an effect is proven) and
`retry.kind: never` (the same message fails the same way again).

**RT-017** — A reserved request key that is required and absent, is not a
JSON string, or does not match its grammar fails with `protocol`. A value
outside the grammar is never normalized into a value inside it: `"01"`,
`" 1"`, `1` and uppercase or braced UUID text are malformed, not aliases.

| Key | Grammar |
|---|---|
| `plenora.capability.name` | `^plenora\.[a-z][a-z0-9-]*-tools$` |
| `plenora.capability.version` | `^[1-9][0-9]*$` |
| `plenora.capability.operation` | `^[a-z][a-z0-9_-]*(\.[a-z][a-z0-9_-]*)+$` |
| `plenora.operation.version` | `^[1-9][0-9]*$` |
| `plenora.input.contract` | `^plenora-[a-z0-9-]+-v[1-9][0-9]*$` |
| `plenora.message.id`, `plenora.trace.correlation_id`, `plenora.message.causation_id` | canonical lowercase hyphenated UUID (RT-012) |
| `plenora.execution.deadline` | RT-021 |
| `plenora.execution.idempotency_key` | RT-022 |

**RT-018** — The category of a rejection is the first that applies:

1. `protocol`, by RT-017;
2. `unsupported`, when every reserved value is well-formed but the capability
   name, the wire version, operation, operation version or input contract
   does not agree with one available operation advertised on the runtime
   surface (RT-004, RT-011), the content type is not advertised (RT-005), or
   a control is present that the operation declares unsupported (RT-006);
3. `timeout`, when the deadline has elapsed at entry (RT-021).

The wire version is `plenora.capability.version`, compared with the wire
version this binding defines, `1` (RT-025): a value such as `"2"` is
well-formed and unsupported; `"01"` is malformed. The contract version is
not a request value and is never compared with one: a destination that
advertises `plenora-runtime-binding-v2` accepts a well-formed request with
wire version `"1"`, as one that advertises `-v1` does. Which contract
governs an exchange is decided before sending, by the selection of the
destination (RT-026).

**RT-019** — The result of a rejection carries the content type and output
contract of section 7 and a new `plenora.message.id` (RT-020). It reflects
`plenora.capability.operation`, `plenora.operation.version` and
`plenora.trace.correlation_id` only when the request carried them
well-formed, byte for byte; otherwise it omits them. It never normalizes a
received value and never supplies one the request did not carry. A request
without a canonical correlation has no originating correlation to preserve
(RT-012), so its rejection carries none. The keys the vector schema requires in
a stored error vector describe valid fixtures, not rejections of malformed
requests.

## 12. Result identity

**RT-020** — Every result, success or error, carries a new
`plenora.message.id`, distinct from the request's. A result SHOULD carry
`plenora.message.causation_id`; when present it MUST equal the request's
`plenora.message.id`, and only when that identity is canonical. A result
never copies the request's own `plenora.message.causation_id`, which names
an earlier cause.

## 13. Execution controls on the runtime

**RT-021** — `plenora.execution.deadline` is UTC: a value with a non-zero
offset, or with `-00:00` (an unknown local offset in RFC 3339), fails with
`protocol`. Senders SHOULD write `YYYY-MM-DDTHH:MM:SSZ`, with an optional
fraction of one to nine digits before `Z`, the spelling of every vector;
whether a receiver accepts the other RFC 3339 spellings of UTC is not decided
by this version. A deadline that has elapsed at entry, `deadline <= now`, fails with
`timeout` under RT-016. A deadline on an operation that does not advertise it
fails with `unsupported` (RT-006).

**RT-022** — `plenora.execution.idempotency_key`, when present, is a non-empty
string within the bound the operation declares; a JSON `null`, an empty string
or a value over the bound fails with `protocol`. On an operation that does not
advertise the control it fails with `unsupported` (RT-006). Its absence is not
an error: the operation runs without deduplication. Reuse of a key with
different input (SURF-012) fails with `conflict`, `phase: validate`,
`remote_effect: none`, `retry.kind: never`.

**RT-023** — On the runtime surface the deadline travels only as metadata. A
component input contract that also carries a deadline field MUST reject a
runtime request that carries both, even with equal values, with
`invalid_configuration` under RT-016.

## 14. Results beyond a bound

**RT-024** — A result that a bound cannot carry whole, a payload limit of the
transport or a materialization limit of the component, fails with a typed
error of category `resource_limit`. A component or transport MUST NOT
truncate a result and report success (SURF-014): a success result is
complete. RT-024 fixes no phase: the phase is the last externally
meaningful phase started (ERR-003), such as `read` while a result is read,
`finalize` while it is built, or `write`, `commit` or `cleanup` after a
publication. The remote effect follows ERR-004: `none` when nothing has been
published or committed; after a publication or a commit, as `data.run`
version 3 makes before it returns its manifest, the one proven, `committed`
or `partial`, or `unknown` when it is not proven (ERR-014, ERR-015,
DT-RUN-006). The retry is `never` when the effect is `none`, and one that
ERR-006 admits otherwise.

The component declares its limits where its profile says; the transport
declares its own in its adoption manifest.
[`database-query-result-limit-error.json`](../../vectors/runtime-v2/database-query-result-limit-error.json)
and
[`database-query-transport-limit-error.json`](../../vectors/runtime-v2/database-query-transport-limit-error.json)
are illustrative error results: they carry neither the request, nor the
bound, nor the size, and the validator does not derive RT-024 from them.

## 15. Negotiation between 1.0 and 2.0

A request is byte for byte the same under both versions (RT-025), so no
destination can tell from a request which contract its sender requires.
The contract is negotiated where the sender, or the transport that routes
on its behalf, chooses the destination.

**RT-026** — Before sending, the sender reads the `runtime` interface of the
destination's capability document (CAP-003) and decides:

1. a sender that requires 1.0 sends to a destination that advertises
   `plenora-runtime-binding-v1` or `plenora-runtime-binding-v2`: every result
   of 2.0 is one that 1.0 admits, RT-024's error being a typed error of
   1.0;
2. a sender that requires 2.0 sends to a destination that advertises
   `plenora-runtime-binding-v2`, under 2.0. To a destination that advertises
   only `plenora-runtime-binding-v1` it sends only when it has declared that
   it accepts the downgrade, and the exchange is then governed by 1.0,
   without RT-024; without that declaration it refuses before sending. The
   downgrade is never implicit;
3. a request meant for 2.0 that reaches a destination of 1.0 is detectable
   only by this selection, never by the destination: a transport that
   routes requests meant for 2.0 applies 2. to each destination it selects,
   with the negotiation context of RT-027, and refuses when it cannot read
   the destination's capability document;
4. a destination whose runtime interface names another contract, or a
   version that is not that of its contract (`1` for `-v1`, `2` for `-v2`),
   is refused before sending.

**RT-027** — The **negotiation context** of a request is the contract its
sender requires and whether the sender accepts the downgrade to 1.0. It
reaches the hop that selects the effective destination unchanged, through
every forwarding:

1. *Where it is.* Outside the runtime envelope, in the transport metadata
   of each hop (message attributes or headers), as two values:
   `plenora.negotiation.requires` (`plenora-runtime-binding-v1` or
   `plenora-runtime-binding-v2`) and `plenora.negotiation.accepts_downgrade`
   (`true` or `false`). It is never written into the reserved metadata or
   the payload of the request, so the wire format of RT-025 does not change,
   and the destination neither reads nor needs it.
2. *The acceptance record.* The transport keeps, for every request it has
   accepted, a durable record bound to its `plenora.message.id` that holds
   the context fixed at its first acceptance:
   - it is created atomically at the first acceptance: two concurrent
     receptions of the same `plenora.message.id` create one record, and the
     one that does not create it is a forwarding;
   - it is kept, readable by every hop of the transport, for the whole
     window in which the transport may still deliver, retry, redrive or
     resend the request, a window the transport declares in its adoption
     manifest;
   - only an authoritative read counts: a read that may miss a record
     already created, such as one from a lagging replica, is an unreadable
     record (item 3);
   - after the window, a reception with the same `plenora.message.id` is
     refused, never taken for a new request;
   - two senders that use the same `plenora.message.id` for different
     requests already violate the uniqueness of that identity (section 3);
     the record does not repair it: the second request is a forwarding of
     the first and is refused unless it states the same context.
3. *Ingress or forwarding, per request.* Whether a hop acts as ingress is
   a property of the request, not of the service it receives it from. A
   hop is the ingress of a request only when it accepts it from outside the
   transport and no acceptance record exists for its `plenora.message.id`:
   a new request. Every other reception, a retry, a redrive from a
   dead-letter queue, a resend by the sender with the same
   `plenora.message.id`, or a delivery from another hop, is a forwarding.
   A hop that cannot read the acceptance record cannot tell the two cases
   apart and refuses.
4. *The ingress.* The sender states the context to the ingress, which
   writes it into the acceptance record. For a sender that states none, the
   ingress attaches `plenora-runtime-binding-v1` without downgrade, unless
   it is configured with a minimum requirement of
   `plenora-runtime-binding-v2`: then a request that states no requirement,
   or states `plenora-runtime-binding-v1`, is refused. Only the ingress of a
   new request may supply the default.
5. *A forwarding.* Every hop that forwards the request, a queue, a router or
   a relay, carries the context unchanged to the next hop. A hop that
   receives a forwarding compares the context it receives with the
   acceptance record: a missing context is a loss, a different one an
   alteration, and both are refused before sending; the request would
   otherwise be routed with a context its sender did not state. A hop that
   cannot carry the context refuses the request when it receives it.
6. *The selection.* The hop that selects the effective destination applies
   RT-026 with the context.

**Limit.** Without a minimum requirement at the ingress, a sender of 2.0
that states no requirement is indistinguishable from a sender of 1.0: its
requirement is taken as 1.0 and no error reports it. The selection then
does not guarantee RT-024, because a destination of 1.0 may be chosen; a
destination of 2.0, if chosen, still applies it. The
guarantee that a downgrade is never implicit (RT-026 item 2) covers the
requirements a sender states, and the requests an ingress with a minimum
requirement of 2.0 accepts; a sender of 2.0 states its requirement, or uses
an ingress so configured.

A refusal before sending is a typed error of the sender: `unsupported`,
`phase: validate`, `remote_effect: none`, `retry.kind: never`. A sender
written for 1.0 that requires exactly `plenora-runtime-binding-v1` refuses a
destination of 2.0: a refusal, never a misread result. The vectors in
[`vectors/runtime-negotiation-v1`](../../vectors/runtime-negotiation-v1/)
(schema
[`runtime-negotiation-vector-v1.schema.json`](../../schemas/runtime-negotiation-vector-v1.schema.json))
are sequences of hops: the context the sender states, the ingress
configuration, and for each hop where the request comes from, the context it
receives and forwards and whether it can read the acceptance record. The
validator derives the decision of RT-026 and RT-027 from the sequence; the
vectors illustrate the rules and do not attest that every sequence is
covered.

## 16. Changes from version 1.0

- RT-024 is new: under 1.0 a result beyond a bound had no stated outcome,
  and a truncated success was excluded only by SURF-014.
- RT-025 replaces RT-002: it separates the wire format, still capability
  version `1`, from the contract version, which the capability document
  states; under 1.0 the capability version named the contract version. A
  request and a result of 2.0 are byte for byte those of 1.0, so a consumer
  of 1.0 reads them. RT-018 compares the wire version with the wire version
  and never the contract version, which RT-026 negotiates before sending.
- RT-026 is new: the selection of the destination decides which contract
  governs an exchange, and a downgrade from 2.0 to 1.0 is explicit.
- RT-027 is new: the negotiation context travels outside the envelope from
  the ingress of a new request to the hop that selects the destination;
  an acceptance record per `plenora.message.id` tells a new request from a
  retry, a redrive or a resend, and a hop refuses a lost or altered context
  or one it cannot check. Unless the ingress requires 2.0, the requirement
  of a silent sender is taken as 1.0 and the selection does not guarantee
  RT-024; a destination of 2.0, if chosen, still applies it.
- Every other rule, key, grammar and vector of 1.0 applies unchanged.
