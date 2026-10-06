//! I comandi: discovery ed esecuzione delle operazioni.

use std::{collections::BTreeMap, io};

use plenora_rest_core::{
    CancellationToken, Engine, EngineConfig, EngineError, ErrorCategory, ErrorPayload, ErrorPhase,
    ExecutionControl, ExecutionError, ExecutionRequest, ExecutionResult, ExecutionStatus,
    RemoteEffect, RetryKind, capabilities,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
};

use crate::{
    args::{Operation, Source},
    envelope::{CLI_ARTIFACT, CLI_CONTRACT, CLI_PROTOCOL_VERSION, CliError, no_details},
    signal,
};

/// Limite della richiesta letta da file o da standard input. Le richieste
/// `enrich` portano i record in linea, quindi il limite è largo; superarlo è
/// un errore `resource_limit`, mai una lettura troncata.
pub(crate) const MAX_REQUEST_BYTES: u64 = 256 * 1024 * 1024;
/// Limite della configurazione del motore, che è un oggetto piccolo.
pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Il documento delle capability di questo binario: quello del core, più
/// l'interfaccia CLI e la superficie `cli` su ogni operazione che la CLI
/// espone (tutte e cinque).
pub(crate) fn capability_document() -> Result<Value, CliError> {
    const SHAPE: CliError = CliError::internal(
        "CAPABILITY_DOCUMENT_INVALID",
        "the capability document has an unexpected shape",
    );
    let mut document = serde_json::to_value(capabilities()).map_err(|_| SHAPE)?;
    let interfaces = document
        .get_mut("interfaces")
        .and_then(Value::as_array_mut)
        .ok_or(SHAPE)?;
    // Dopo l'interfaccia Rust, nell'ordine dell'enum `surface` dello schema.
    let position = interfaces
        .iter()
        .position(|interface| interface.get("kind") == Some(&Value::from("rust")))
        .map_or(0, |index| index.saturating_add(1));
    interfaces.insert(
        position,
        json!({
            "kind": "cli",
            "contract": CLI_CONTRACT,
            "version": CLI_PROTOCOL_VERSION,
            "artifact": CLI_ARTIFACT,
        }),
    );
    let operations = document
        .get_mut("operations")
        .and_then(Value::as_array_mut)
        .ok_or(SHAPE)?;
    let mut exposed = 0_usize;
    for operation in operations.iter_mut() {
        let id = operation.get("id").and_then(Value::as_str).ok_or(SHAPE)?;
        if !Operation::ALL
            .iter()
            .any(|candidate| candidate.capability_id() == id)
        {
            continue;
        }
        let surfaces = operation
            .get_mut("surfaces")
            .and_then(Value::as_array_mut)
            .ok_or(SHAPE)?;
        let position = surfaces
            .iter()
            .position(|surface| surface == "rust")
            .map_or(0, |index| index.saturating_add(1));
        surfaces.insert(position, Value::from("cli"));
        exposed = exposed.saturating_add(1);
    }
    // Ogni comando operativo deve corrispondere a un'operazione dichiarata
    // (CLI 2.0 §10): se il catalogo del core non le contenesse tutte, la
    // CLI esporrebbe comandi senza capability.
    if exposed != Operation::ALL.len() {
        return Err(SHAPE);
    }
    Ok(document)
}

/// L'esito di un'operazione: il risultato da mettere nell'envelope di
/// successo o l'errore da mettere in quello di fallimento.
pub(crate) enum OperationOutcome {
    Success(Value),
    Failure(ErrorPayload),
}

impl From<CliError> for OperationOutcome {
    fn from(error: CliError) -> Self {
        Self::Failure(error.payload(no_details()))
    }
}

/// Esegue un comando operativo. Va chiamata dentro il runtime tokio.
pub(crate) async fn run_operation(
    operation: Operation,
    input: Source,
    config: Option<String>,
) -> OperationOutcome {
    // Il gestore dei segnali si installa per primo: un Ctrl-C durante la
    // lettura della richiesta è già una cancellazione cooperativa.
    let token = CancellationToken::new();
    let (cancelled_tx, mut cancelled_rx) = watch::channel(false);
    if signal::install(token.clone(), cancelled_tx).is_err() {
        return CliError::internal(
            "SIGNAL_HANDLER_UNAVAILABLE",
            "the interrupt handler could not be installed",
        )
        .into();
    }
    let prepared = tokio::select! {
        biased;
        // `Ok` soltanto: se il gestore dei segnali terminasse, il canale
        // chiuso disabilita il ramo invece di sembrare una cancellazione.
        Ok(_) = cancelled_rx.wait_for(|cancelled| *cancelled) => {
            return cancelled_before_execution().into();
        }
        prepared = prepare(operation, input, config) => prepared,
    };
    let (engine, request) = match prepared {
        Ok(prepared) => prepared,
        Err(outcome) => return outcome,
    };
    let mut control = ExecutionControl::new(token);
    if let Some(deadline) = request.options.deadline.as_deref() {
        control = match control.with_deadline(deadline) {
            Ok(control) => control,
            Err(error) => return OperationOutcome::Failure(error.payload()),
        };
    }
    let result = engine.execute_with_control(request, control).await;
    outcome_of(result)
}

/// Legge configurazione e richiesta, controlla che la richiesta dichiari
/// l'operazione del comando e costruisce il motore. Niente rete.
async fn prepare(
    operation: Operation,
    input: Source,
    config: Option<String>,
) -> Result<(Engine, ExecutionRequest), OperationOutcome> {
    let config = match config {
        None => EngineConfig::default(),
        Some(path) => {
            let bytes = read_file(&path, MAX_CONFIG_BYTES, Document::Config).await?;
            serde_json::from_slice::<EngineConfig>(&bytes)
                .map_err(|error| invalid_json(Document::Config, &error))?
        }
    };
    let bytes = match input {
        Source::Stdin => {
            read_bounded(tokio::io::stdin(), MAX_REQUEST_BYTES, Document::Request).await?
        }
        Source::File(path) => read_file(&path, MAX_REQUEST_BYTES, Document::Request).await?,
    };
    let request = serde_json::from_slice::<ExecutionRequest>(&bytes)
        .map_err(|error| invalid_json(Document::Request, &error))?;
    if request.operation != operation.execution() {
        return Err(CliError::invalid(
            "OPERATION_MISMATCH",
            "the request operation does not match the command",
        )
        .into());
    }
    Ok((Engine::new(config), request))
}

/// Il risultato del motore nella forma dell'envelope. Come il binding
/// runtime: `success` e `partial` sono un successo con l'intero
/// `ExecutionResult`; `failed` è il primo errore, con gli handle di recovery
/// dei job asincroni in `details.async_jobs`.
pub(crate) fn outcome_of(result: ExecutionResult) -> OperationOutcome {
    if result.status != ExecutionStatus::Failed {
        return match serde_json::to_value(&result) {
            Ok(value) => OperationOutcome::Success(value),
            Err(_) => CliError::internal(
                "RESULT_SERIALIZATION_FAILED",
                "the execution result could not be serialized",
            )
            .into(),
        };
    }
    let mut error = result.errors.first().map_or_else(
        || EngineError::Runtime("execution failed without an error".into()).payload(),
        execution_error_payload,
    );
    if !result.recoveries.is_empty() {
        error.details.insert(
            "async_jobs".to_owned(),
            serde_json::to_value(&result.recoveries).unwrap_or_else(|_| Value::Array(Vec::new())),
        );
    }
    OperationOutcome::Failure(error)
}

/// Lo stesso errore pubblico del binding runtime: `input_index` non fa parte
/// di `plenora-error-v1` e resta nel solo `ExecutionResult`.
fn execution_error_payload(error: &ExecutionError) -> ErrorPayload {
    ErrorPayload {
        category: error.category,
        phase: error.phase,
        remote_effect: error.remote_effect,
        retry: error.retry,
        code: error.code.clone(),
        message: error.message.clone(),
        details: error.details.clone(),
    }
}

fn cancelled_before_execution() -> CliError {
    CliError {
        category: ErrorCategory::Cancelled,
        phase: ErrorPhase::Validate,
        remote_effect: RemoteEffect::None,
        retry: RetryKind::Safe,
        code: "CANCELLED",
        message: "the command was cancelled before execution started",
    }
}

#[derive(Clone, Copy)]
enum Document {
    Request,
    Config,
}

impl Document {
    const fn invalid_json(self) -> CliError {
        match self {
            Self::Request => CliError::invalid(
                "INVALID_INPUT",
                "the request is not valid JSON for the execution request contract",
            ),
            Self::Config => CliError::invalid(
                "INVALID_CONFIGURATION",
                "the engine configuration is not valid JSON for EngineConfig",
            ),
        }
    }

    const fn too_large(self) -> CliError {
        let (code, message) = match self {
            Self::Request => ("INPUT_TOO_LARGE", "the request exceeds its byte limit"),
            Self::Config => (
                "CONFIG_TOO_LARGE",
                "the engine configuration exceeds its byte limit",
            ),
        };
        CliError {
            category: ErrorCategory::ResourceLimit,
            phase: ErrorPhase::Validate,
            remote_effect: RemoteEffect::None,
            retry: RetryKind::Never,
            code,
            message,
        }
    }

    /// Il tipo di guasto, mai il messaggio di `io::Error`, che può
    /// contenere il path.
    const fn unreadable(self, kind: io::ErrorKind) -> CliError {
        let (code, message) = match (self, kind) {
            (Self::Request, io::ErrorKind::NotFound) => {
                ("INPUT_NOT_FOUND", "the request file does not exist")
            }
            (Self::Request, io::ErrorKind::PermissionDenied) => (
                "INPUT_PERMISSION_DENIED",
                "the request file cannot be read: permission denied",
            ),
            (Self::Request, _) => ("INPUT_UNREADABLE", "the request could not be read"),
            (Self::Config, io::ErrorKind::NotFound) => (
                "CONFIG_NOT_FOUND",
                "the engine configuration file does not exist",
            ),
            (Self::Config, io::ErrorKind::PermissionDenied) => (
                "CONFIG_PERMISSION_DENIED",
                "the engine configuration file cannot be read: permission denied",
            ),
            (Self::Config, _) => (
                "CONFIG_UNREADABLE",
                "the engine configuration could not be read",
            ),
        };
        CliError {
            category: ErrorCategory::Io,
            phase: ErrorPhase::Read,
            remote_effect: RemoteEffect::None,
            retry: RetryKind::Never,
            code,
            message,
        }
    }
}

/// JSON non valido: la posizione (riga e colonna) aiuta a trovare l'errore
/// senza citare il contenuto; il messaggio di serde, che può citarlo, non
/// esce.
fn invalid_json(document: Document, error: &serde_json::Error) -> OperationOutcome {
    let details = BTreeMap::from([
        ("line".to_owned(), Value::from(error.line())),
        ("column".to_owned(), Value::from(error.column())),
    ]);
    OperationOutcome::Failure(document.invalid_json().payload(details))
}

async fn read_file(
    path: &str,
    limit: u64,
    document: Document,
) -> Result<Vec<u8>, OperationOutcome> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| OperationOutcome::from(document.unreadable(error.kind())))?;
    read_bounded(file, limit, document).await
}

/// Legge al più `limit` byte; un byte in più è un errore, mai un troncamento.
async fn read_bounded(
    reader: impl AsyncRead + Unpin,
    limit: u64,
    document: Document,
) -> Result<Vec<u8>, OperationOutcome> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| OperationOutcome::from(document.unreadable(error.kind())))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > limit) {
        let details = BTreeMap::from([("limit_bytes".to_owned(), Value::from(limit))]);
        return Err(OperationOutcome::Failure(
            document.too_large().payload(details),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use plenora_rest_core::{
        ErrorCategory, ExecutionMetrics, ExecutionOutput, ExecutionResult, ExecutionStatus,
    };
    use serde_json::{Value, json};

    use super::{Document, OperationOutcome, capability_document, outcome_of, read_bounded};

    #[test]
    fn capabilities_add_the_cli_interface_and_surface() {
        let document = capability_document().unwrap();
        let interfaces = document["interfaces"].as_array().unwrap();
        assert_eq!(interfaces[0]["kind"], "rust");
        assert_eq!(
            interfaces[1],
            json!({"kind": "cli", "contract": "plenora-cli-v2", "version": 2, "artifact": "plenora-rest"})
        );
        for operation in document["operations"].as_array().unwrap() {
            assert_eq!(
                operation["surfaces"],
                json!(["rust", "cli", "python_sdk", "runtime"])
            );
        }
        assert_eq!(document["operations"].as_array().unwrap().len(), 5);
    }

    fn result(status: ExecutionStatus, errors: Value, recoveries: Value) -> ExecutionResult {
        let errors = serde_json::from_value::<Vec<Value>>(errors).unwrap();
        let recoveries = serde_json::from_value::<Vec<Value>>(recoveries).unwrap();
        ExecutionResult {
            schema_version: 1,
            status,
            output: ExecutionOutput::None,
            metrics: ExecutionMetrics::default(),
            responses: Vec::new(),
            errors: errors
                .into_iter()
                .map(|error| plenora_rest_core::ExecutionError {
                    category: ErrorCategory::Execution,
                    phase: plenora_rest_core::ErrorPhase::Read,
                    remote_effect: plenora_rest_core::RemoteEffect::Unknown,
                    retry: plenora_rest_core::RetryAdvice {
                        kind: plenora_rest_core::RetryKind::Never,
                    },
                    code: error["code"].as_str().unwrap().to_owned(),
                    message: "m".to_owned(),
                    input_index: Some(3),
                    details: Default::default(),
                })
                .collect(),
            recoveries: recoveries
                .into_iter()
                .map(|recovery| plenora_rest_core::AsyncJobRecovery {
                    contract: "plenora-rest-async-job-recovery-v1".to_owned(),
                    job_id: recovery.as_str().unwrap().to_owned(),
                    cancel_requested: false,
                    cancel_accepted: None,
                })
                .collect(),
        }
    }

    #[test]
    fn partial_results_are_successes() {
        let outcome = outcome_of(result(
            ExecutionStatus::Partial,
            json!([{"code": "HTTP_STATUS"}]),
            json!([]),
        ));
        let OperationOutcome::Success(value) = outcome else {
            panic!("partial must be a success envelope");
        };
        assert_eq!(value["status"], "partial");
        assert_eq!(value["errors"][0]["input_index"], 3);
    }

    #[test]
    fn failures_carry_the_first_error_and_the_recovery_handles() {
        let outcome = outcome_of(result(
            ExecutionStatus::Failed,
            json!([{"code": "POLLING_TIMEOUT"}, {"code": "HTTP_STATUS"}]),
            json!(["job-1"]),
        ));
        let OperationOutcome::Failure(error) = outcome else {
            panic!("failed must be an error envelope");
        };
        assert_eq!(error.code, "POLLING_TIMEOUT");
        let value = serde_json::to_value(&error).unwrap();
        assert!(value.get("input_index").is_none());
        assert_eq!(value["details"]["async_jobs"][0]["job_id"], "job-1");
    }

    #[test]
    fn a_failure_without_errors_is_internal() {
        let OperationOutcome::Failure(error) =
            outcome_of(result(ExecutionStatus::Failed, json!([]), json!([])))
        else {
            panic!("failed must be an error envelope");
        };
        assert_eq!(error.category, ErrorCategory::Internal);
    }

    #[tokio::test]
    async fn reads_are_bounded_without_truncation() {
        let exact = read_bounded(&b"abcd"[..], 4, Document::Request).await;
        assert_eq!(exact.ok(), Some(b"abcd".to_vec()));
        let Err(OperationOutcome::Failure(error)) =
            read_bounded(&b"abcde"[..], 4, Document::Request).await
        else {
            panic!("an oversized input must be refused");
        };
        assert_eq!(error.category, ErrorCategory::ResourceLimit);
        assert_eq!(error.details["limit_bytes"], 4);
    }
}
