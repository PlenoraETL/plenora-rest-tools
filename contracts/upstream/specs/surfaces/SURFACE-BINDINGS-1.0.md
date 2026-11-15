# Plenora Surface Bindings Contract 1.0

Status: normative

Contract identifier: `plenora-surface-bindings-v1`

The exact CLI, Python and runtime bindings are machine-readable in
[`bindings`](../../bindings/) and validate against
[`surface-bindings-v1.schema.json`](../../schemas/surface-bindings-v1.schema.json).
Rust operation-to-API mappings are component-owned as defined below.

## 1. One operation, multiple spellings

CLI commands, Python methods and runtime selectors are spellings of a catalog
operation, not independent semantics. Each binding identifies exactly one
`(operation, version)` pair from the component catalog.

A surface MAY wrap inputs and results in idiomatic types. It MUST preserve the
catalog contracts, defaults, error axes, side effects and execution controls;
a surface that materializes a result declares the effect it adds (SB-001).

When a component publishes several catalog versions, an artifact implements
exactly one of them, the one its profile and capability document declare. A
spelling MAY then be listed for several versions of the same operation that no
single catalog version selects together; on a given artifact it identifies the
version that artifact's catalog selects. An artifact exposing two versions of
one operation on the same surface needs a distinct spelling for each.

### Materializing a result that the surface cannot return in process

The catalog describes the **operation**, not the mechanics a particular surface
needs in order to deliver its result. A surface that cannot return an output in
process — a process-level CLI whose machine stream already carries exactly one
JSON document, per CLI 2.0 §4 — must place that output somewhere the caller can
reach it.

Writing it to a location the caller named is a property of the **binding**, not
a change to the operation. Such a surface:

- MUST accept the destination as a declared input of its binding, never infer
  it, and never write to a location the caller did not name;
- MUST declare the resulting effect in the capability document of the artifact
  (SB-001), so that discovery describes the artifact that is running rather
  than the abstract operation;
- MUST NOT change the operation identifier, version, contracts, error axes or
  execution controls.

The operation's declared `side_effect` continues to describe the operation, in
the catalog and in the capability document. A binding that materializes a
result declares its own effect next to it, and the two are not in conflict:
they describe different things, and a consumer reads the one that belongs to
the surface it is calling.

Without this distinction an operation whose output is a dataset could not be
bound to a process-level CLI at all: the catalog would say the operation has no
side effect, the surface would have to write a file, and no truthful
declaration would exist.

**SB-001** — The effect of a surface is declared in the operation's capability
`attributes` under the shared key `plenora.surface_side_effects` (contract
`plenora-surface-side-effects-v1`). Its value is an object whose members are
surface names listed in that operation's `surfaces` and whose values are
`local` or `remote`; each value MUST be stricter than the operation's
`side_effect` in the order `none`, `local`, `remote`. A surface that is not a
member has exactly the operation's `side_effect`. A consumer that calls an
operation through a surface reads the stricter of the two as the effect of
that call. The key is optional: its absence preserves the meaning of Capability
Discovery 2.0, in which every surface has the operation's `side_effect`.

## 2. Rust binding

The common catalog selects the Rust surface and its operation semantics, but
this repository does not prescribe Rust module, trait, method or type names.
Each component MUST publish a versioned operation-to-public-export mapping for
its released crate and verify it through a consumer crate that imports only
those documented exports.

The component-owned mapping identifies each catalog `(operation, version)` and
the corresponding public Rust entry point. Its adoption manifest identifies
the exact crate artifact by version and digest. Private modules, test-only
features and implementation traits are not valid mappings.

## 3. CLI binding

[`cli-v1.json`](../../bindings/cli-v1.json) defines canonical commands. Tokens
written in uppercase denote caller-provided values; they are grammar
placeholders, not literal arguments.

Every listed CLI artifact also implements `--help`, `--version --format json`
and `capabilities --format json` according to CLI 2.0. Diagnostic, benchmark,
fuzz and self-test commands are not public domain bindings.

Deprecated aliases MAY remain callable only when listed under
`deprecated_aliases`. They MUST resolve to the canonical operation and emit the
same machine result. New integrations MUST use the canonical entrypoint.

## 4. Python binding

[`python-sdk-v1.json`](../../bindings/python-sdk-v1.json) defines distribution,
import and symbol spellings. The target packages, required or selected by
their profile, are:

| Component | Distribution | Import |
|---|---|---|
| database-tools | `plenora-database` | `plenora_database` |
| data-tools | `plenora-data` | `plenora_data` |
| io-tools | `plenora-io` | `plenora_io` |
| rest-tools | `plenora-rest` | `plenora_rest` |
| storage-tools | `plenora-storage` | `plenora_storage` |

Sync and async symbols listed for the same operation are semantically
equivalent. Lifecycle helpers and query builders may expose several idiomatic
methods that all bind to one operation identity.

## 5. Runtime binding

[`runtime-v1.json`](../../bindings/runtime-v1.json) uses the canonical selector
form `<capability>#<operation>@<operation-version>`. The capability name is
`plenora.<domain>-tools`; the runtime capability version remains `1` as defined
by Runtime Binding 1.0.

The selector is review notation. Serialized requests still carry the reserved
metadata keys from Runtime Binding 1.0 and MUST NOT derive routing by parsing a
CLI command or Python symbol.

## 6. Adoption rule

Target bindings describe what a conforming public artifact exposes. Existing
legacy spellings do not become normative merely because they are implemented.
During migration an adopter records the missing canonical binding and any
temporary alias in its adoption manifest.

This contract names public entrypoints only. It does not prescribe the internal
function, module, native binding technology or command dispatcher used to
implement them.
