# 0016: The IO-tools Python SDK as a declared surface

Status: accepted

Date: 2026-10-10; revised 2026-10-18

## Context

Decision 0015 introduced Public Catalogs 2.0 and the current IO-tools catalog
and profile v3. IO-tools PR #60 adopts those documents. The original draft
of this decision added Python to v2; that addition has not been published
on plenora-contracts main.

IO-tools PRs #44 and #50 are merged. The SDK was inspected with `git show
origin/main:sdk/python/...` at IO-tools revision
`80f2557c9214cadcf5c772acd0d383f91a834eb9`: `_client.py` supplies the method
map, discovery and backend selection; `__init__.py` exports package version
and errors; `_process.py` implements the runners. The SDK ships both a pure
Python wheel using a CLI process and a native abi3 wheel using `NativeRunner`.
Both execute the CLI protocol. Describing it as only a process wrapper would
exclude the native implementation without a semantic reason.

## Decision

- Catalog `io-tools-v3.json` selects `python_sdk` as `conditional` and adds it
  to all six operations. These additions are permitted by COMPATIBILITY.md;
  operation versions, payloads, controls and catalog side effects stay fixed.
- Catalogs and profiles v1 and v2 retain their published content. In
  particular, the unmerged Python additions to v2 are withdrawn from this
  draft, not from a published contract. Decision 0015 makes v3 the current
  target while preserving earlier pins. The global exit condition for 1.0
  need not be assumed satisfied: coexistence permits the older catalogs,
  but does not require backporting new surfaces to them.
- `bindings/python-sdk-v1.json` names `plenora-io / plenora_io`, discovery
  `plenora_io.version` and `Client.capabilities`, and the methods in the real
  `OPERAZIONI` map: `Client.catalog` (io.catalog@2), `Client.inspect`
  (io.inspect@1), `Client.layers` (io.layers@1), `Client.read`,
  `Client.read_table`, `Client.validate` (io.read@2), `Client.write`
  (io.write@2), and `Client.convert` (io.convert@1).
- Profile v3 makes Python optional and adds **IO-PY-001**: `api_modes:
  ["sync"]`, CLI protocol semantics through either backend, structured
  discovery, Arrow adaptation and fidelity, and truthful effects and
  cancellation/timeout outcomes. `Client.backend` distinguishes execution;
  it is not a domain operation and needs no binding-map entry.
- `plenora_io.version()` reads installed distribution metadata;
  `Client.version()` describes the backend and is not a substitute.
  `Client.capabilities()` already adds the Python interface and surface to
  the backend document. Consumers select from that document, never from
  the target catalog (CAT-001).
- `io_python_errors` checks the artifact, discovery and method bindings.
- The immutability gate recognizes Public Catalogs 2.0 in its byte-level
  dispatch as well as its document comparator. The missing dispatch entry
  rejected compatible surface additions; the fix applies the existing
  compatibility rules, with tests that still reject removals and mutations.

## Remaining IO adoption work

The declaration is a target, not a claim that an existing wheel conforms.
The merge condition on PR #44 is satisfied; PR #50 is also present. Before
IO-tools claims the Python contract at the new contracts pin it must:

- complete the operation-version migration in IO PR #60: main's backend
  capability document still advertises `io.catalog`, `io.read` and `io.write`
  at version 1 with v1 result identifiers. The SDK forwards those identities;
  its method map alone does not establish v3 conformance. The local #60 tree
  advertises version 2 for those three operations. Keep the v2 bindings
  required by catalog v3; do not downgrade them to match the old backend;
- complete SB-001 in the backend and `Client.capabilities()`:
  `crates/plenora-io-tools/src/capability.rs` currently gives `io.read` a
  blanket `side_effect: local`, both on main and in the inspected #60 tree.
  The catalog requires `none`, with the local materialization effect declared
  per surface. Set the operation effect to `none` and add `cli: local` under
  `plenora.surface_side_effects`; `con_superficie_python` must preserve it
  and add `python_sdk: local`. Today that function adds only interface and
  surface, not this attribute. Adding the attribute while leaving the blanket
  effect `local` would also violate SB-001's strictly stronger override rule.
  Keep this requirement; fix IO rather than change the catalog's meaning;
- update the adoption manifest and copied catalog/profile at the new pin,
  declare `plenora-python-sdk-v1` for Python artifacts with `api_modes:
  ["sync"]`, and replace the old rationale excluding Python from the profile;
- verify installed pure and native wheels, including capability effects,
  package-version discovery, cooperative native timeout, SIGINT cancellation
  and conservative error axes after a writing command starts.

The current native timeout requests cancellation and waits, then raises
`ProtocolError`; it is not a hard return-time limit or necessarily the
operation's `TimeoutError`. A handled SIGINT delivers the operation's
cancellation envelope as `CancelledError`. The process backend retains its
platform/thread limitations. Neither behavior permits assuming rollback.
These distinctions are recorded in IO-PY-001 without prescribing native
implementation details or requiring a nominal asynchronous API.

## Change statement

- **Consumers affected:** Python callers of IO-tools adopting profile v3.
- **Before:** no shared Python surface for IO-tools.
- **After:** an optional SDK with canonical spellings and IO-PY-001.
- **Compatible:** an undecided/not-applicable target becomes selected and
  operations gain a surface; an artifact without Python stays conforming
  and omits it from discovery without a deviation (CAT-002).
- **Documents:** catalog and profile v3, Python binding map, Surface Bindings
  distribution table, validator and regression tests; no schema or vector
  changes and no change to the published v2 catalog/profile.
