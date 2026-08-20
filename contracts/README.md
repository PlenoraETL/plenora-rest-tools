# plenora-rest component contracts

This directory is the normative, component-owned wire contract for
plenora-rest-tools. The common Plenora catalog owns the operation identities;
this repository owns the payload shapes referenced by that catalog.

Contract identifiers:

- plenora-rest-execution-request-v1;
- plenora-rest-execution-result-v1;
- plenora-rest-file-transfer-input-v1;
- plenora-rest-file-transfer-result-v1;
- plenora-rest-async-job-recovery-v1;
- plenora-rest-capability-attributes-v1.

The five stable operations are rest.test, rest.generate, rest.enrich,
rest.download and rest.upload. They are provider-neutral. Provider presets,
service-specific adapters and HTTP implementation details are not part of
these contracts.

Runtime messages carry JSON envelopes. Raw file bytes, private local paths and
inline credentials are forbidden on the runtime boundary. File transfers use
an authorized opaque artifact_source or artifact_sink; credentials use
credential_ref. Rust and in-process Python callers may use an explicitly
authorized local path as an input convenience, but results never echo it.

Unknown fields are rejected by the Rust DTOs. Public failures preserve
category, phase, remote_effect and retry from plenora-error-v1.

An asynchronous HTTP job may return a bounded recovery handle containing only
its public job identifier and best-effort remote cancellation outcome. Resume
uses that identifier with the original connection and skips job submission.
Idempotency keys are execution controls and never appear in public results.
