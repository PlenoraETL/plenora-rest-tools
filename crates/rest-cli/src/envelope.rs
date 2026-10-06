//! Envelope CLI 2.0, proiezione degli exit code ed errori della CLI.

use std::collections::BTreeMap;

use plenora_rest_core::{
    COMPONENT_ID, ERROR_CONTRACT, ErrorCategory, ErrorPayload, ErrorPhase, RemoteEffect,
    RetryAdvice, RetryKind,
};
use serde_json::{Map, Value, json};

use crate::args::Command;

/// Versione del protocollo CLI adottato (`plenora-cli-v2`).
pub(crate) const CLI_PROTOCOL_VERSION: u32 = 2;
/// Identificatore del contratto CLI comune.
pub(crate) const CLI_CONTRACT: &str = "plenora-cli-v2";
/// Nome dell'artefatto binario, come compare nelle capability.
pub(crate) const CLI_ARTIFACT: &str = "plenora-rest";
/// Versione del componente: il workspace ne ha una sola.
pub(crate) const COMPONENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Il comando nell'envelope quando il primo argomento non è un comando.
pub(crate) const UNKNOWN_COMMAND: &str = "unknown";

/// Il documento d'errore emesso quando nemmeno l'envelope d'errore si lascia
/// serializzare. È statico, quindi non può fallire, e resta un envelope v2
/// valido.
pub(crate) const SERIALIZATION_FALLBACK: &str = concat!(
    r#"{"status":"error","protocol_version":2,"component":"plenora-rest-tools","#,
    r#""component_version":""#,
    env!("CARGO_PKG_VERSION"),
    r#"","contract":"plenora-error-v1","command":"unknown","#,
    r#""error":{"category":"internal","phase":"finalize","remote_effect":"unknown","#,
    r#""retry":{"kind":"quarantine"},"code":"OUTPUT_SERIALIZATION_FAILED","#,
    r#""message":"the command output could not be serialized","details":{}}}"#
);

/// Exit code di CLI 2.0 §8: la categoria JSON è autorevole, il codice ne è la
/// proiezione. Il `match` è esaustivo sull'enum del core: una categoria nuova
/// non compila finché non riceve qui una mappatura esplicita.
pub(crate) const fn exit_code(category: ErrorCategory) -> u8 {
    match category {
        ErrorCategory::InvalidPlan | ErrorCategory::InvalidConfiguration => 2,
        ErrorCategory::Schema | ErrorCategory::DataMapping | ErrorCategory::Unsupported => 3,
        ErrorCategory::ResourceLimit => 4,
        ErrorCategory::Io
        | ErrorCategory::Protocol
        | ErrorCategory::Authentication
        | ErrorCategory::Authorization
        | ErrorCategory::Timeout
        | ErrorCategory::Transient => 5,
        ErrorCategory::Execution => 6,
        ErrorCategory::Internal => 70,
        ErrorCategory::Cancelled => 130,
    }
}

/// Exit code dell'errore `internal`, anche per un panic intercettato.
pub(crate) const INTERNAL_EXIT: u8 = 70;
/// Exit code di un successo che non si è potuto scrivere su stdout: è un
/// errore di I/O locale, e non può uscire con 0.
pub(crate) const OUTPUT_FAILURE_EXIT: u8 = exit_code(ErrorCategory::Io);

/// Un errore della CLI stessa, con testo statico. Diventa un `ErrorPayload`
/// del core, lo stesso tipo che serializzano le altre superfici.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CliError {
    pub(crate) category: ErrorCategory,
    pub(crate) phase: ErrorPhase,
    pub(crate) remote_effect: RemoteEffect,
    pub(crate) retry: RetryKind,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

impl CliError {
    /// Un rifiuto prima di qualunque effetto: niente è partito.
    pub(crate) const fn invalid(code: &'static str, message: &'static str) -> Self {
        Self {
            category: ErrorCategory::InvalidConfiguration,
            phase: ErrorPhase::Validate,
            remote_effect: RemoteEffect::None,
            retry: RetryKind::Never,
            code,
            message,
        }
    }

    /// Un guasto interno della CLI. L'effetto remoto è ignoto perché un panic
    /// può arrivare a operazione avviata.
    pub(crate) const fn internal(code: &'static str, message: &'static str) -> Self {
        Self {
            category: ErrorCategory::Internal,
            phase: ErrorPhase::Cleanup,
            remote_effect: RemoteEffect::Unknown,
            retry: RetryKind::Quarantine,
            code,
            message,
        }
    }

    pub(crate) fn payload(self, details: BTreeMap<String, Value>) -> ErrorPayload {
        ErrorPayload {
            category: self.category,
            phase: self.phase,
            remote_effect: self.remote_effect,
            retry: RetryAdvice { kind: self.retry },
            code: self.code.to_owned(),
            message: self.message.to_owned(),
            details,
        }
    }
}

/// Un panic intercettato: nessun dettaglio, né il messaggio del panic né la
/// sua posizione.
pub(crate) const PANIC: CliError = CliError::internal(
    "INTERNAL_PANIC",
    "the command failed internally; no detail is reported",
);

/// Identità del comando nell'envelope: nome canonico e contratto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) command: &'static str,
    pub(crate) contract: &'static str,
}

impl Identity {
    pub(crate) const fn of(command: Command) -> Self {
        Self {
            command: command.name(),
            contract: command.output_contract(),
        }
    }

    /// Un comando non riconosciuto: il contratto è quello dell'errore.
    pub(crate) const UNKNOWN: Self = Self {
        command: UNKNOWN_COMMAND,
        contract: ERROR_CONTRACT,
    };

    pub(crate) fn from_command(command: Option<Command>) -> Self {
        command.map_or(Self::UNKNOWN, Self::of)
    }
}

fn identity_fields(status: &str, identity: Identity) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("status".to_owned(), Value::from(status));
    fields.insert(
        "protocol_version".to_owned(),
        Value::from(CLI_PROTOCOL_VERSION),
    );
    fields.insert("component".to_owned(), Value::from(COMPONENT_ID));
    fields.insert(
        "component_version".to_owned(),
        Value::from(COMPONENT_VERSION),
    );
    fields.insert("contract".to_owned(), Value::from(identity.contract));
    fields.insert("command".to_owned(), Value::from(identity.command));
    fields
}

/// L'envelope di successo, come una riga JSON senza newline.
pub(crate) fn success(identity: Identity, result: Value) -> String {
    let mut fields = identity_fields("ok", identity);
    fields.insert("result".to_owned(), result);
    serde_json::to_string(&Value::Object(fields))
        .unwrap_or_else(|_| SERIALIZATION_FALLBACK.to_owned())
}

/// L'envelope d'errore, come una riga JSON senza newline.
pub(crate) fn failure(identity: Identity, error: &ErrorPayload) -> String {
    let Ok(error) = serde_json::to_value(error) else {
        return SERIALIZATION_FALLBACK.to_owned();
    };
    let mut fields = identity_fields("error", identity);
    fields.insert("error".to_owned(), error);
    serde_json::to_string(&Value::Object(fields))
        .unwrap_or_else(|_| SERIALIZATION_FALLBACK.to_owned())
}

/// Dettagli vuoti.
pub(crate) fn no_details() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// Il risultato di `--version`.
pub(crate) fn version_result() -> Value {
    json!({
        "component_version": COMPONENT_VERSION,
        "protocol_version": CLI_PROTOCOL_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use plenora_rest_core::ErrorCategory;
    use serde_json::Value;

    use super::{SERIALIZATION_FALLBACK, exit_code};

    #[test]
    fn exit_codes_follow_the_cli_projection() {
        let expected = [
            (ErrorCategory::InvalidPlan, 2),
            (ErrorCategory::InvalidConfiguration, 2),
            (ErrorCategory::Schema, 3),
            (ErrorCategory::DataMapping, 3),
            (ErrorCategory::Unsupported, 3),
            (ErrorCategory::ResourceLimit, 4),
            (ErrorCategory::Io, 5),
            (ErrorCategory::Protocol, 5),
            (ErrorCategory::Authentication, 5),
            (ErrorCategory::Authorization, 5),
            (ErrorCategory::Timeout, 5),
            (ErrorCategory::Transient, 5),
            (ErrorCategory::Execution, 6),
            (ErrorCategory::Internal, 70),
            (ErrorCategory::Cancelled, 130),
        ];
        for (category, code) in expected {
            assert_eq!(exit_code(category), code, "{category:?}");
            assert_ne!(exit_code(category), 0);
        }
    }

    #[test]
    fn the_serialization_fallback_is_a_valid_error_envelope() {
        let document: Value = serde_json::from_str(SERIALIZATION_FALLBACK).unwrap();
        assert_eq!(document["status"], "error");
        assert_eq!(document["protocol_version"], 2);
        assert_eq!(document["component_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(document["error"]["category"], "internal");
    }
}
