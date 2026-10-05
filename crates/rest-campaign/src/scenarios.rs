//! Scenari della campagna.
//!
//! Ogni scenario costruisce una richiesta del contratto v1, la esegue con
//! l'API pubblica del motore e confronta il risultato con l'esito atteso e con
//! i contatori del server. Ogni scostamento è una [`Violation`] con un
//! criterio della roadmap; un'osservazione senza violazioni è un esito
//! atteso, anche quando l'esito atteso è un errore (i guasti iniettati).
//!
//! I dettagli delle violazioni sono testi statici: nessun valore di risposta,
//! percorso o segreto finisce nel report.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use plenora_rest_core::{
    CancellationToken, EXECUTION_REQUEST_CONTRACT, Engine, EngineConfig, EngineError,
    ExecutionControl, ExecutionError, ExecutionOutput, ExecutionRequest, ExecutionResult,
    ExecutionStatus, FILE_TRANSFER_INPUT_CONTRACT, FileTransferDirection, REST_DOWNLOAD, REST_TEST,
    RUNTIME_INTERFACE_CONTRACT, RuntimeBinding, RuntimeMessage, RuntimeMessageKind,
    RuntimeResources,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::ScenarioSizes,
    rng::SplitMix64,
    server::{ENGINE_LABEL_HEADER, TestServer, content_chunk, content_sha256, key_seed},
    timefmt,
};

/// Etichetta dell'Engine principale nelle richieste.
pub const MAIN_ENGINE_LABEL: &str = "principale";
/// Etichetta degli Engine aperti e chiusi dallo scenario di ricambio.
pub const CHURN_ENGINE_LABEL: &str = "ricambio";
/// Riferimento della credenziale risolta dal runtime binding.
pub const CREDENTIAL_REFERENCE: &str = "secret://campaign/credential";

macro_rules! scenarios {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// Scenari eseguibili. Il nome è quello usato nei profili, nelle
        /// soglie e nel report.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Scenario {
            $($variant),+
        }

        impl Scenario {
            pub const ALL: &'static [Scenario] = &[$(Scenario::$variant),+];

            pub fn name(self) -> &'static str {
                match self {
                    $(Scenario::$variant => $name),+
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Scenario::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

scenarios! {
    Ok => "ok",
    Slow => "slow",
    RateLimited => "rate_limited",
    Flaky5xx => "flaky_5xx",
    Persistent5xx => "persistent_5xx",
    DropBeforeResponse => "drop_before_response",
    PostBodyThenDrop => "post_body_then_drop",
    Truncated => "truncated",
    Stall => "stall",
    Deadline => "deadline",
    DeadlineWithControl => "deadline_with_control",
    RuntimeDeadline => "runtime_deadline",
    Cancel => "cancel",
    PageOffset => "page_offset",
    PageCursorFault => "page_cursor_fault",
    PageLink => "page_link",
    Enrich => "enrich",
    Job => "job",
    JobPollFault => "job_poll_fault",
    JobResume => "job_resume",
    JobCancel => "job_cancel",
    JobDeadline => "job_deadline",
    Download => "download",
    DownloadResume => "download_resume",
    DownloadCorrupt => "download_corrupt",
    DownloadCut => "download_cut",
    Upload => "upload",
    CookieSession => "cookie_session",
    RuntimeBindingScenario => "runtime_binding",
    DnsFailure => "dns_failure",
    ConnectRefused => "connect_refused",
    TlsFailure => "tls_failure",
    ConnectTimeout => "connect_timeout",
    EngineChurn => "engine_churn",
}

/// Criteri della roadmap a cui si riferisce una violazione.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Criterion {
    /// Esito diverso da quello dichiarato per lo scenario.
    UnexpectedOutcome,
    /// Più tentativi di `max_attempts`.
    RetryOverMaxAttempts,
    /// Richieste ripetute che il contratto non consente (retry di un metodo
    /// non idempotente, richieste oltre quelle attese).
    Amplification,
    /// Submit ripetuto durante un resume.
    DuplicateSubmit,
    /// File pubblicato incompleto, corrotto o pubblicato dopo un errore.
    IncompleteFilePublished,
    /// File temporanei del motore rimasti dopo l'operazione.
    TempFileLeft,
    /// Ordine o contenuto dell'enrichment non conservato.
    OrderLoss,
    /// Segreto o percorso locale nel risultato o negli errori.
    Exposure,
    /// `remote_effect` o retry advice incoerenti con il guasto iniettato.
    RemoteEffectIncoherent,
    /// Deadline o cancellazione non rispettate entro la tolleranza.
    CancellationNotHonored,
    /// Un Engine chiuso ha accettato lavoro.
    ClosedEngineAccepted,
    /// Una sessione cookie chiusa ha raggiunto la rete.
    StaleSessionAccepted,
    /// `metrics.requests` inferiore alle richieste ricevute dal servizio per
    /// la stessa operazione.
    MetricsUnderreported,
    /// Difetto dell'harness (richiesta non conforme, I/O locale).
    HarnessError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    pub criterion: Criterion,
    pub detail: &'static str,
}

/// Coerenza tra il guasto iniettato e quanto il motore dichiara.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Coherence {
    Coherent,
    /// Più prudente del necessario (per esempio `unknown` dove il guasto
    /// esclude ogni effetto remoto): sicuro ma impreciso.
    Conservative,
    Incoherent,
}

/// Classe del guasto, che fissa i `remote_effect` e retry advice ammessi.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultClass {
    /// Nessun byte della richiesta ha raggiunto il servizio (DNS, connessione
    /// rifiutata, TLS fallito, connect timeout).
    NoRequestSent,
    /// La richiesta è partita: l'effetto remoto non si può escludere.
    AfterSend,
    /// Come sopra, per un metodo non idempotente: un retry «safe» sarebbe un
    /// doppio effetto.
    AfterSendNonIdempotent,
    /// Il servizio ha risposto con un errore dopo i tentativi.
    RemoteStatus,
    /// Polling non concluso: si recupera con resume, non con un nuovo submit.
    PollingTimeout,
    /// Rifiuto locale prima della rete (sessione chiusa, Engine chiuso).
    LocalRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultObservation {
    pub fault: &'static str,
    /// Codice d'errore, oppure `recuperato` quando il guasto è stato assorbito.
    pub outcome: String,
    pub remote_effect: Option<String>,
    pub retry: Option<String>,
    pub coherence: Coherence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorRecord {
    pub category: String,
    pub phase: String,
    pub code: String,
    pub remote_effect: String,
    pub retry: String,
}

/// Esito osservato di un'operazione.
#[derive(Clone, Debug)]
pub struct Observation {
    pub scenario: Scenario,
    pub latency: Duration,
    pub errors: Vec<ErrorRecord>,
    pub requests: u64,
    pub retries: u64,
    pub server_hits: u64,
    pub violations: Vec<Violation>,
    pub faults: Vec<FaultObservation>,
}

impl Observation {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            latency: Duration::ZERO,
            errors: Vec::new(),
            requests: 0,
            retries: 0,
            server_hits: 0,
            violations: Vec::new(),
            faults: Vec::new(),
        }
    }

    fn violate(&mut self, criterion: Criterion, detail: &'static str) {
        self.violations.push(Violation { criterion, detail });
    }

    fn check(&mut self, condition: bool, criterion: Criterion, detail: &'static str) {
        if !condition {
            self.violate(criterion, detail);
        }
    }

    pub fn as_expected(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Risorse del runtime binding: credenziale e artifact della campagna.
struct CampaignResources {
    bearer: String,
    files_root: PathBuf,
}

impl RuntimeResources for CampaignResources {
    fn resolve_credentials(
        &self,
        reference: &str,
    ) -> Result<plenora_rest_core::AuthConfig, EngineError> {
        if reference == CREDENTIAL_REFERENCE {
            Ok(plenora_rest_core::AuthConfig::Bearer {
                token: self.bearer.clone(),
            })
        } else {
            Err(EngineError::PolicyViolation(
                "credential reference is not authorized".into(),
            ))
        }
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::PolicyViolation(
            "artifact source is not authorized".into(),
        ))
    }

    fn resolve_artifact_sink(&self, reference: &str) -> Result<PathBuf, EngineError> {
        let name = reference
            .strip_prefix("artifact://campaign/")
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            .ok_or_else(|| {
                EngineError::PolicyViolation("artifact sink is not authorized".into())
            })?;
        Ok(self.files_root.join("rt").join(format!("{name}.bin")))
    }
}

/// Tutto ciò che serve agli scenari.
pub struct Context {
    pub engine: Arc<Engine>,
    pub server: Arc<TestServer>,
    /// Directory dei trasferimenti, assoluta: è il `file_root` dell'Engine.
    pub files_root: PathBuf,
    /// Testi che non devono mai comparire in un risultato: il segreto e i
    /// percorsi locali assoluti (anche nella forma con escape JSON).
    pub forbidden: Vec<String>,
    pub bearer: String,
    pub closed_port: u16,
    pub sizes: ScenarioSizes,
    pub engine_config: EngineConfig,
    pub connect_timeout_target: Option<String>,
    pub slack: Duration,
    pub watchdog: Duration,
}

impl Context {
    fn base(&self) -> String {
        self.server.base_url()
    }
}

fn label_of<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => "non_rappresentabile".to_owned(),
    }
}

fn error_record(error: &ExecutionError) -> ErrorRecord {
    ErrorRecord {
        category: label_of(&error.category),
        phase: label_of(&error.phase),
        code: error.code.clone(),
        remote_effect: label_of(&error.remote_effect),
        retry: label_of(&error.retry.kind),
    }
}

fn headers(label: &str) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        ENGINE_LABEL_HEADER.to_owned(),
        Value::String(label.to_owned()),
    );
    Value::Object(map)
}

fn retry_policy(max_attempts: u32, statuses: &[u16]) -> Value {
    json!({
        "max_attempts": max_attempts,
        "backoff_base_ms": 0,
        "retry_on_status": statuses,
        "max_retry_after_ms": 5_000
    })
}

fn build(observation: &mut Observation, request: Value) -> Option<ExecutionRequest> {
    match serde_json::from_value::<ExecutionRequest>(request) {
        Ok(request) => Some(request),
        Err(_) => {
            observation.violate(
                Criterion::HarnessError,
                "richiesta della campagna non conforme al contratto",
            );
            None
        }
    }
}

/// Registra metriche ed errori del risultato e cerca segreti e percorsi.
fn absorb(observation: &mut Observation, context: &Context, result: &ExecutionResult) {
    observation.requests = observation.requests.saturating_add(result.metrics.requests);
    observation.retries = observation.retries.saturating_add(result.metrics.retries);
    observation
        .errors
        .extend(result.errors.iter().map(error_record));
    match serde_json::to_string(result) {
        Ok(text) => scan(observation, context, &text),
        Err(_) => observation.violate(
            Criterion::UnexpectedOutcome,
            "risultato del motore non serializzabile",
        ),
    }
}

fn scan(observation: &mut Observation, context: &Context, text: &str) {
    if text.contains(&context.bearer) {
        observation.violate(Criterion::Exposure, "segreto presente nel risultato");
    }
    if context
        .forbidden
        .iter()
        .any(|forbidden| text.contains(forbidden.as_str()))
    {
        observation.violate(
            Criterion::Exposure,
            "percorso locale presente nel risultato",
        );
    }
}

async fn run_request(
    observation: &mut Observation,
    context: &Context,
    engine: &Engine,
    request: Value,
    control: ExecutionControl,
) -> Option<ExecutionResult> {
    let request = build(observation, request)?;
    let result = engine.execute_with_control(request, control).await;
    absorb(observation, context, &result);
    Some(result)
}

fn expect_success(observation: &mut Observation, result: &ExecutionResult) -> bool {
    let ok = result.status == ExecutionStatus::Success && result.errors.is_empty();
    observation.check(
        ok,
        Criterion::UnexpectedOutcome,
        "esito diverso da success senza guasto bloccante",
    );
    ok
}

fn expect_failure<'a>(
    observation: &mut Observation,
    result: &'a ExecutionResult,
    codes: &[&str],
) -> Option<&'a ExecutionError> {
    let error = result.errors.first();
    let matches = result.status == ExecutionStatus::Failed
        && error.is_some_and(|error| codes.contains(&error.code.as_str()));
    if !matches {
        observation.violate(
            Criterion::UnexpectedOutcome,
            "il guasto non ha prodotto l'errore atteso",
        );
        return None;
    }
    if !matches!(result.output, ExecutionOutput::None) {
        observation.violate(
            Criterion::UnexpectedOutcome,
            "un'operazione fallita ha restituito un output",
        );
    }
    error
}

fn bound_attempts(observation: &mut Observation, hits: u64, max: u64) {
    if hits > max {
        observation.violate(
            Criterion::RetryOverMaxAttempts,
            "richieste al servizio oltre max_attempts",
        );
    }
}

fn worst(left: Coherence, right: Coherence) -> Coherence {
    left.max(right)
}

fn coherence(class: FaultClass, error: &ExecutionError) -> Coherence {
    let effect = label_of(&error.remote_effect);
    let retry = label_of(&error.retry.kind);
    let effect_coherence = match class {
        FaultClass::NoRequestSent | FaultClass::LocalRefusal => match effect.as_str() {
            "none" => Coherence::Coherent,
            "unknown" => Coherence::Conservative,
            _ => Coherence::Incoherent,
        },
        FaultClass::AfterSend
        | FaultClass::AfterSendNonIdempotent
        | FaultClass::RemoteStatus
        | FaultClass::PollingTimeout => {
            if effect == "none" {
                Coherence::Incoherent
            } else {
                Coherence::Coherent
            }
        }
    };
    let retry_coherence = match class {
        FaultClass::NoRequestSent => match retry.as_str() {
            "safe" => Coherence::Coherent,
            "requires_recovery" => Coherence::Incoherent,
            _ => Coherence::Conservative,
        },
        FaultClass::AfterSendNonIdempotent => {
            if retry == "safe" {
                Coherence::Incoherent
            } else {
                Coherence::Coherent
            }
        }
        FaultClass::PollingTimeout => match retry.as_str() {
            "requires_recovery" => Coherence::Coherent,
            "safe" => Coherence::Incoherent,
            _ => Coherence::Conservative,
        },
        FaultClass::LocalRefusal => match retry.as_str() {
            "never" | "safe" => Coherence::Coherent,
            _ => Coherence::Conservative,
        },
        FaultClass::AfterSend | FaultClass::RemoteStatus => Coherence::Coherent,
    };
    worst(effect_coherence, retry_coherence)
}

fn fault_error(
    observation: &mut Observation,
    fault: &'static str,
    class: FaultClass,
    error: &ExecutionError,
) {
    let coherence = coherence(class, error);
    if coherence == Coherence::Incoherent {
        observation.violate(
            Criterion::RemoteEffectIncoherent,
            "remote_effect o retry advice incoerenti con il guasto",
        );
    }
    observation.faults.push(FaultObservation {
        fault,
        outcome: error.code.clone(),
        remote_effect: Some(label_of(&error.remote_effect)),
        retry: Some(label_of(&error.retry.kind)),
        coherence,
    });
}

fn fault_recovered(observation: &mut Observation, fault: &'static str) {
    observation.faults.push(FaultObservation {
        fault,
        outcome: "recuperato".to_owned(),
        remote_effect: None,
        retry: None,
        coherence: Coherence::Coherent,
    });
}

fn fault_failed_unexpectedly(observation: &mut Observation, fault: &'static str) {
    observation.faults.push(FaultObservation {
        fault,
        outcome: "esito_inatteso".to_owned(),
        remote_effect: None,
        retry: None,
        coherence: Coherence::Coherent,
    });
}

fn uuid(rng: &mut SplitMix64) -> String {
    let high = rng.next_u64();
    let low = rng.next_u64();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xFFFF,
        high & 0x0FFF,
        0x8000 | ((low >> 48) & 0x3FFF),
        low & 0xFFFF_FFFF_FFFF
    )
}

/// Esegue uno scenario e restituisce l'osservazione.
pub async fn run(scenario: Scenario, context: &Context, key: &str, seed: u64) -> Observation {
    let mut observation = Observation::new(scenario);
    let mut rng = SplitMix64::new(seed);
    let started = Instant::now();
    match scenario {
        Scenario::Ok => simple_ok(&mut observation, context, key).await,
        Scenario::Slow => slow(&mut observation, context, key, &mut rng).await,
        Scenario::RateLimited | Scenario::Flaky5xx => {
            transient_status(&mut observation, context, key, &mut rng).await;
        }
        Scenario::Persistent5xx => {
            persistent_status(&mut observation, context, key, &mut rng).await
        }
        Scenario::DropBeforeResponse | Scenario::Truncated => {
            broken_response(&mut observation, context, key).await;
        }
        Scenario::PostBodyThenDrop => post_body_then_drop(&mut observation, context, key).await,
        Scenario::Stall | Scenario::Deadline | Scenario::DeadlineWithControl => {
            stalled(&mut observation, context, key).await;
        }
        Scenario::RuntimeDeadline => {
            runtime_deadline(&mut observation, context, key, &mut rng).await;
        }
        Scenario::Cancel => cancel(&mut observation, context, key).await,
        Scenario::PageOffset | Scenario::PageLink => {
            pages_success(&mut observation, context, key, &mut rng).await;
        }
        Scenario::PageCursorFault => pages_failure(&mut observation, context, key, &mut rng).await,
        Scenario::Enrich => enrich(&mut observation, context, key, &mut rng).await,
        Scenario::Job | Scenario::JobPollFault => {
            job(&mut observation, context, key, &mut rng).await;
        }
        Scenario::JobResume => job_resume(&mut observation, context, key).await,
        Scenario::JobCancel | Scenario::JobDeadline => {
            job_interrupted(&mut observation, context, key).await;
        }
        Scenario::Download
        | Scenario::DownloadResume
        | Scenario::DownloadCorrupt
        | Scenario::DownloadCut => download(&mut observation, context, key, &mut rng).await,
        Scenario::Upload => upload(&mut observation, context, key, &mut rng).await,
        Scenario::CookieSession => cookie_session(&mut observation, context, key).await,
        Scenario::RuntimeBindingScenario => {
            runtime_binding(&mut observation, context, key, &mut rng).await;
        }
        Scenario::DnsFailure
        | Scenario::ConnectRefused
        | Scenario::TlsFailure
        | Scenario::ConnectTimeout => network_fault(&mut observation, context, key).await,
        Scenario::EngineChurn => engine_churn(&mut observation, context, key).await,
    }
    observation.latency = started.elapsed();
    // Il runtime binding non espone metriche nei messaggi d'errore: lì il
    // confronto non è possibile.
    let has_metrics = !matches!(
        scenario,
        Scenario::RuntimeBindingScenario | Scenario::RuntimeDeadline
    );
    if has_metrics && observation.server_hits > observation.requests {
        observation.violate(
            Criterion::MetricsUnderreported,
            "metrics.requests inferiore alle richieste ricevute dal servizio",
        );
    }
    observation
}

fn test_request(context: &Context, url: String) -> Value {
    json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": url,
            "method": "GET",
            "headers": headers(MAIN_ENGINE_LABEL),
            "auth": {"type": "bearer", "token": context.bearer}
        }
    })
}

async fn simple_ok(observation: &mut Observation, context: &Context, key: &str) {
    let request = test_request(context, format!("{}/ok/{key}", context.base()));
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        let value_ok = matches!(&result.output, ExecutionOutput::Json { value } if *value == json!({"ok": true}));
        observation.check(
            value_ok,
            Criterion::UnexpectedOutcome,
            "valore della risposta non conservato",
        );
    }
    bound_attempts(observation, counters.hits, 1);
}

async fn slow(observation: &mut Observation, context: &Context, key: &str, rng: &mut SplitMix64) {
    let delay = rng.between(context.sizes.slow_ms_min, context.sizes.slow_ms_max);
    let request = test_request(context, format!("{}/slow/{key}/{delay}", context.base()));
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    expect_success(observation, &result);
    bound_attempts(observation, counters.hits, 1);
}

async fn transient_status(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let max = context.sizes.max_attempts;
    let failures = rng.between(1, u64::from(max - 1));
    let (status, retry_after, fault) = if observation.scenario == Scenario::RateLimited {
        // Retry-After di un secondo in un caso su quattro: verifica che il
        // motore lo rispetti senza allungare troppo la campagna.
        let seconds = u64::from(rng.below(4) == 0);
        (429_u16, seconds.to_string(), "http_429_retry_after")
    } else {
        let status = [500_u16, 502, 503, 504][rng.below(4) as usize];
        (status, "-".to_owned(), "http_5xx_transitorio")
    };
    let mut request = test_request(
        context,
        format!(
            "{}/status/{key}/{failures}/{status}/{retry_after}",
            context.base()
        ),
    );
    request["connection"]["retry"] = retry_policy(max, &[status]);
    let started = Instant::now();
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let elapsed = started.elapsed();
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        fault_recovered(observation, fault);
    } else {
        fault_failed_unexpectedly(observation, fault);
    }
    bound_attempts(observation, counters.hits, u64::from(max));
    observation.check(
        counters.hits == failures + 1 && result.metrics.requests == failures + 1,
        Criterion::Amplification,
        "richieste diverse da guasti iniettati più una",
    );
    if retry_after == "1" {
        let minimum =
            Duration::from_millis(failures * 1_000).saturating_sub(Duration::from_millis(50));
        observation.check(
            elapsed >= minimum,
            Criterion::UnexpectedOutcome,
            "Retry-After non rispettato",
        );
    }
}

async fn persistent_status(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let max = context.sizes.max_attempts;
    let status = [500_u16, 502, 503][rng.below(3) as usize];
    let mut request = test_request(
        context,
        format!("{}/status/{key}/1000000/{status}/-", context.base()),
    );
    request["connection"]["retry"] = retry_policy(max, &[status]);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if let Some(error) = expect_failure(observation, &result, &["HTTP_STATUS"]) {
        fault_error(
            observation,
            "http_5xx_persistente",
            FaultClass::RemoteStatus,
            error,
        );
    }
    bound_attempts(observation, counters.hits, u64::from(max));
    observation.check(
        counters.hits >= u64::from(max),
        Criterion::UnexpectedOutcome,
        "retry configurati non eseguiti",
    );
}

async fn broken_response(observation: &mut Observation, context: &Context, key: &str) {
    let max = context.sizes.max_attempts;
    let (route, fault, codes): (&str, &str, &[&str]) =
        if observation.scenario == Scenario::DropBeforeResponse {
            (
                "drop",
                "chiusura_prima_della_risposta",
                &["TRANSPORT_ERROR"],
            )
        } else {
            (
                "truncated",
                "risposta_troncata",
                &["TRANSPORT_ERROR", "INVALID_RESPONSE"],
            )
        };
    let mut request = test_request(context, format!("{}/{route}/{key}", context.base()));
    request["connection"]["retry"] = retry_policy(max, &[503]);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if let Some(error) = expect_failure(observation, &result, codes) {
        fault_error(observation, fault, FaultClass::AfterSend, error);
    }
    bound_attempts(observation, counters.hits, u64::from(max));
    observation.check(
        counters.hits >= 1,
        Criterion::UnexpectedOutcome,
        "la richiesta non ha raggiunto il servizio",
    );
}

async fn post_body_then_drop(observation: &mut Observation, context: &Context, key: &str) {
    let max = context.sizes.max_attempts;
    let request = json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{}/sink_drop/{key}", context.base()),
            "method": "POST",
            "headers": headers(MAIN_ENGINE_LABEL),
            "request": {"body_type": "raw", "raw_body": "{\"ordine\":1}"},
            "retry": retry_policy(max, &[503])
        }
    });
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if let Some(error) = expect_failure(observation, &result, &["TRANSPORT_ERROR"]) {
        fault_error(
            observation,
            "corpo_letto_poi_chiusura",
            FaultClass::AfterSendNonIdempotent,
            error,
        );
    }
    // Un POST senza retry_non_idempotent non si ripete mai: un secondo corpo
    // sarebbe un secondo effetto remoto.
    observation.check(
        counters.hits <= 1 && counters.bodies <= 1,
        Criterion::Amplification,
        "metodo non idempotente ripetuto dopo un possibile effetto remoto",
    );
    observation.check(
        counters.bodies == 1,
        Criterion::UnexpectedOutcome,
        "il corpo della richiesta non è arrivato al servizio",
    );
}

async fn stalled(observation: &mut Observation, context: &Context, key: &str) {
    let mut request = test_request(context, format!("{}/hang/{key}", context.base()));
    let (limit, fault) = if observation.scenario == Scenario::Stall {
        let timeout = context.sizes.stall_timeout_ms;
        request["connection"]["request"] = json!({"timeout_ms": timeout});
        (Duration::from_millis(timeout), "risposta_assente_timeout")
    } else {
        let offset = Duration::from_millis(context.sizes.deadline_ms);
        let Some(deadline) = timefmt::deadline_after(offset) else {
            observation.violate(Criterion::HarnessError, "deadline non rappresentabile");
            return;
        };
        request["options"] = json!({"deadline": deadline});
        (offset, "deadline")
    };
    if observation.scenario == Scenario::DeadlineWithControl {
        // Se la deadline non viene applicata, il timeout per richiesta chiude
        // comunque l'operazione invece del timeout dell'Engine.
        request["connection"]["request"] =
            json!({"timeout_ms": fallback_timeout_ms(&context.sizes)});
    }
    let started = Instant::now();
    let result = match build(observation, request) {
        // `execute` legge la deadline da `options.deadline`;
        // `execute_with_control` riceve un controllo senza deadline e deve
        // comunque rispettare quella della richiesta.
        Some(request) if observation.scenario == Scenario::Deadline => {
            Some(context.engine.execute(request).await)
        }
        Some(request) => Some(
            context
                .engine
                .execute_with_control(request, ExecutionControl::default())
                .await,
        ),
        None => None,
    };
    let elapsed = started.elapsed();
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    absorb(observation, context, &result);
    if let Some(error) = expect_failure(observation, &result, &["TIMEOUT"]) {
        fault_error(observation, fault, FaultClass::AfterSend, error);
    }
    observation.check(
        elapsed <= limit + context.slack,
        Criterion::CancellationNotHonored,
        "timeout o deadline superati oltre la tolleranza",
    );
    bound_attempts(observation, counters.hits, 1);
}

/// Deadline nel payload di un messaggio del runtime binding, senza la
/// metadata `plenora.execution.deadline`: il binding deve applicarla oppure
/// rifiutare il messaggio (come fa per la chiave di idempotenza nel payload),
/// mai ignorarla.
async fn runtime_deadline(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let offset = Duration::from_millis(context.sizes.deadline_ms);
    let Some(deadline) = timefmt::deadline_after(offset) else {
        observation.violate(Criterion::HarnessError, "deadline non rappresentabile");
        return;
    };
    let resources = CampaignResources {
        bearer: context.bearer.clone(),
        files_root: context.files_root.clone(),
    };
    let binding = RuntimeBinding::new(&context.engine, &resources);
    let message = runtime_message(
        rng,
        REST_TEST,
        EXECUTION_REQUEST_CONTRACT,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{}/hang/{key}", context.base()),
                "method": "GET",
                "headers": headers(MAIN_ENGINE_LABEL),
                "request": {"timeout_ms": fallback_timeout_ms(&context.sizes)}
            },
            "options": {"deadline": deadline}
        }),
    );
    let started = Instant::now();
    let response = binding.invoke(message, CancellationToken::new()).await;
    let elapsed = started.elapsed();
    scan_message(observation, context, &response);
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let code = response.payload["code"].as_str().unwrap_or_default();
    let refused = response.kind == RuntimeMessageKind::Error && code == "INVALID_INPUT";
    let timed_out = response.kind == RuntimeMessageKind::Error && code == "TIMEOUT";
    observation.check(
        refused || timed_out,
        Criterion::UnexpectedOutcome,
        "deadline del payload né applicata né rifiutata",
    );
    observation.check(
        refused || elapsed <= offset + context.slack,
        Criterion::CancellationNotHonored,
        "deadline del payload del runtime ignorata",
    );
    if refused {
        observation.check(
            counters.hits == 0,
            Criterion::UnexpectedOutcome,
            "richiesta inviata nonostante il rifiuto",
        );
    }
}

async fn cancel(observation: &mut Observation, context: &Context, key: &str) {
    let request = test_request(context, format!("{}/hang/{key}", context.base()));
    let Some(request) = build(observation, request) else {
        return;
    };
    let token = CancellationToken::new();
    let engine = Arc::clone(&context.engine);
    let control = ExecutionControl::new(token.clone());
    let task = tokio::spawn(async move { engine.execute_with_control(request, control).await });
    tokio::time::sleep(Duration::from_millis(context.sizes.cancel_after_ms)).await;
    let cancelled_at = Instant::now();
    token.cancel();
    let joined = tokio::time::timeout(context.watchdog, task).await;
    let returned = cancelled_at.elapsed();
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let result = match joined {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            observation.violate(
                Criterion::UnexpectedOutcome,
                "esecuzione terminata senza risultato",
            );
            return;
        }
        Err(_) => {
            observation.violate(
                Criterion::CancellationNotHonored,
                "la cancellazione non ha fermato l'esecuzione",
            );
            return;
        }
    };
    absorb(observation, context, &result);
    if let Some(error) = expect_failure(observation, &result, &["CANCELLED"]) {
        fault_error(observation, "cancellazione", FaultClass::AfterSend, error);
    }
    observation.check(
        returned <= context.slack,
        Criterion::CancellationNotHonored,
        "cancellazione onorata oltre la tolleranza",
    );
    bound_attempts(observation, counters.hits, 1);
}

fn expected_rows(total: u64) -> Vec<Value> {
    (0..total).map(|n| json!({"n": n})).collect()
}

fn records_of(result: &ExecutionResult) -> Option<Vec<Value>> {
    match &result.output {
        ExecutionOutput::Records { records } => Some(
            records
                .iter()
                .map(|record| Value::Object(record.clone()))
                .collect(),
        ),
        _ => None,
    }
}

fn pagination_request(
    context: &Context,
    key: &str,
    mode: &str,
    fail_page: Option<u64>,
    fail_times: u64,
) -> Value {
    let sizes = &context.sizes;
    let fail = fail_page.map_or_else(|| "-".to_owned(), |page| page.to_string());
    let url = format!(
        "{}/pages/{key}/{mode}/{}/{}/{fail}/{fail_times}",
        context.base(),
        sizes.page_total,
        sizes.page_size
    );
    let pagination = match mode {
        "offset" => json!({"type": "offset", "page_size": sizes.page_size, "max_rows": 1_000_000}),
        "cursor" => {
            json!({"type": "cursor", "cursor_param": "cursor", "cursor_path": "next_cursor", "max_rows": 1_000_000, "max_pages": 100_000})
        }
        _ => {
            json!({"type": "link", "link_path": "next", "max_rows": 1_000_000, "max_pages": 100_000})
        }
    };
    json!({
        "schema_version": 1,
        "operation": "generate",
        "connection": {
            "url": url,
            "method": "GET",
            "headers": headers(MAIN_ENGINE_LABEL),
            "retry": retry_policy(sizes.max_attempts, &[503]),
            "response": {
                "records_path": "items",
                "output_mapping": [{"path": "n", "column": "n"}]
            },
            "pagination": pagination
        }
    })
}

async fn pages_success(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let mode = if observation.scenario == Scenario::PageOffset {
        "offset"
    } else {
        "link"
    };
    let pages = context.sizes.page_total.div_ceil(context.sizes.page_size);
    // Errore transitorio a metà in metà dei casi: il retry deve recuperarlo
    // senza duplicare né perdere righe.
    let fail_page = (rng.below(2) == 0 && pages > 0).then(|| rng.below(pages));
    let request = pagination_request(context, key, mode, fail_page, 1);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        observation.check(
            records_of(&result) == Some(expected_rows(context.sizes.page_total)),
            Criterion::OrderLoss,
            "righe paginate perse, duplicate o fuori ordine",
        );
        if fail_page.is_some() {
            fault_recovered(observation, "errore_paginazione_transitorio");
        }
    } else if fail_page.is_some() {
        fault_failed_unexpectedly(observation, "errore_paginazione_transitorio");
    }
    observation.check(
        counters.hits == result.metrics.requests,
        Criterion::UnexpectedOutcome,
        "richieste dichiarate diverse da quelle ricevute",
    );
    // Pagine con righe, una eventuale pagina vuota finale e un guasto.
    bound_attempts(observation, counters.hits, pages + 2);
}

async fn pages_failure(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let pages = context
        .sizes
        .page_total
        .div_ceil(context.sizes.page_size)
        .max(1);
    let fail_page = rng.below(pages);
    let request = pagination_request(context, key, "cursor", Some(fail_page), 1_000_000);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if let Some(error) = expect_failure(observation, &result, &["HTTP_STATUS"]) {
        fault_error(
            observation,
            "errore_paginazione_persistente",
            FaultClass::RemoteStatus,
            error,
        );
    }
    let max = u64::from(context.sizes.max_attempts);
    bound_attempts(observation, counters.hits, fail_page + max);
    observation.check(
        counters.hits == fail_page + max,
        Criterion::UnexpectedOutcome,
        "pagine richieste diverse da quelle attese prima del guasto",
    );
}

async fn enrich(observation: &mut Observation, context: &Context, key: &str, rng: &mut SplitMix64) {
    let sizes = &context.sizes;
    let records: Vec<Value> = (0..sizes.enrich_records)
        .map(|id| json!({"id": id}))
        .collect();
    let flaky_mod = if rng.below(2) == 0 {
        0
    } else {
        rng.between(3, 7)
    };
    let flaky = if flaky_mod == 0 {
        0
    } else {
        (0..sizes.enrich_records)
            .filter(|id| id % flaky_mod == 0)
            .count() as u64
    };
    let request = json!({
        "schema_version": 1,
        "operation": "enrich",
        "connection": {
            "url": format!("{}/enrich/{key}/{flaky_mod}/{{id}}", context.base()),
            "method": "GET",
            "headers": headers(MAIN_ENGINE_LABEL),
            "retry": retry_policy(sizes.max_attempts, &[503]),
            "parameters": [{
                "name": "id",
                "mode": "mapped",
                "source": "id",
                "required": true,
                "location": "path"
            }]
        },
        "input": {"records": records},
        "options": {
            "continue_on_error": true,
            "enrichment_concurrency": sizes.enrich_concurrency
        }
    });
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        let expected: Vec<Value> = (0..sizes.enrich_records)
            .map(|id| json!({"id": id, "remote": id}))
            .collect();
        observation.check(
            records_of(&result) == Some(expected),
            Criterion::OrderLoss,
            "record arricchiti fuori ordine o alterati",
        );
        if flaky > 0 {
            fault_recovered(observation, "http_5xx_transitorio_enrichment");
        }
    } else if flaky > 0 {
        fault_failed_unexpectedly(observation, "http_5xx_transitorio_enrichment");
    }
    observation.check(
        counters.hits == sizes.enrich_records + flaky,
        Criterion::Amplification,
        "richieste di enrichment diverse da record più guasti",
    );
}

fn job_request(context: &Context, key: &str, polls: u64, fail_poll: u64) -> Value {
    json!({
        "schema_version": 1,
        "operation": "test",
        "connection": {
            "url": format!("{}/jobs/{key}/submit/{polls}/{fail_poll}", context.base()),
            "method": "POST",
            "headers": headers(MAIN_ENGINE_LABEL),
            "retry": retry_policy(context.sizes.max_attempts, &[503]),
            "polling": {
                "url_template": "{base}/jobs/{job_id}",
                "location_header": null,
                "status_path": "status",
                "result_path": "result",
                "interval_ms": 5,
                "max_attempts": polls + 2
            }
        }
    })
}

async fn job(observation: &mut Observation, context: &Context, key: &str, rng: &mut SplitMix64) {
    let polls = rng.between(1, 4);
    let fail_poll = u64::from(observation.scenario == Scenario::JobPollFault);
    let request = job_request(context, key, polls, fail_poll);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        let value_ok = matches!(&result.output, ExecutionOutput::Json { value } if *value == json!({"job": key, "answer": 42}));
        observation.check(
            value_ok,
            Criterion::UnexpectedOutcome,
            "risultato del job non conservato",
        );
        if fail_poll > 0 {
            fault_recovered(observation, "errore_polling_transitorio");
        }
    } else if fail_poll > 0 {
        fault_failed_unexpectedly(observation, "errore_polling_transitorio");
    }
    observation.check(
        counters.submits == 1,
        Criterion::DuplicateSubmit,
        "submit del job ripetuto",
    );
    observation.check(
        counters.polls == polls + fail_poll,
        Criterion::Amplification,
        "polling diversi da quelli necessari",
    );
}

async fn job_resume(observation: &mut Observation, context: &Context, key: &str) {
    let polls = 3;
    let mut first = job_request(context, key, polls, 0);
    first["connection"]["polling"]["max_attempts"] = json!(1);
    let result = run_request(
        observation,
        context,
        &context.engine,
        first,
        ExecutionControl::default(),
    )
    .await;
    let Some(result) = result else {
        context.server.take(key);
        return;
    };
    if let Some(error) = expect_failure(observation, &result, &["POLLING_TIMEOUT"]) {
        fault_error(
            observation,
            "polling_timeout_resume",
            FaultClass::PollingTimeout,
            error,
        );
    }
    let recovered = result.recoveries.first();
    observation.check(
        recovered.is_some_and(|recovery| recovery.job_id == key && !recovery.cancel_requested),
        Criterion::UnexpectedOutcome,
        "handle di recovery assente o diverso dal job",
    );
    let mut resume = job_request(context, key, polls, 0);
    resume["connection"]["polling"]["resume"] = json!({"job_id": key});
    resume["connection"]["polling"]["max_attempts"] = json!(polls + 2);
    let resumed = run_request(
        observation,
        context,
        &context.engine,
        resume,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(resumed) = resumed else { return };
    if expect_success(observation, &resumed) {
        let value_ok = matches!(&resumed.output, ExecutionOutput::Json { value } if *value == json!({"job": key, "answer": 42}));
        observation.check(
            value_ok,
            Criterion::UnexpectedOutcome,
            "risultato del job ripreso non conservato",
        );
    }
    observation.check(
        counters.submits == 1,
        Criterion::DuplicateSubmit,
        "il resume ha ripetuto il submit",
    );
    observation.check(
        counters.polls == polls,
        Criterion::Amplification,
        "polling diversi da quelli necessari",
    );
}

async fn job_interrupted(observation: &mut Observation, context: &Context, key: &str) {
    let mut request = job_request(context, key, 1_000_000, 0);
    request["connection"]["polling"]["interval_ms"] = json!(60_000);
    request["connection"]["polling"]["max_attempts"] = json!(10);
    request["connection"]["polling"]["cancel"] = json!({});
    let is_cancel = observation.scenario == Scenario::JobCancel;
    let offset = Duration::from_millis(context.sizes.deadline_ms);
    if !is_cancel {
        let Some(deadline) = timefmt::deadline_after(offset) else {
            observation.violate(Criterion::HarnessError, "deadline non rappresentabile");
            return;
        };
        request["options"] = json!({"deadline": deadline});
    }
    let Some(request) = build(observation, request) else {
        return;
    };
    let token = CancellationToken::new();
    let engine = Arc::clone(&context.engine);
    let control = ExecutionControl::new(token.clone());
    let started = Instant::now();
    let task = tokio::spawn(async move {
        if is_cancel {
            engine.execute_with_control(request, control).await
        } else {
            engine.execute(request).await
        }
    });
    let mut reference = started;
    if is_cancel {
        // Si cancella dopo che il servizio ha ricevuto il submit e dopo un
        // margine perché il motore legga la risposta, così il caso osservato
        // di solito è quello con un job remoto da cancellare. Il server conta
        // il submit prima di scrivere la risposta: una cancellazione che
        // arriva prima che il motore conosca il job id resta possibile ed è
        // registrata come guasto distinto.
        let wait_until = Instant::now() + context.watchdog / 2;
        while context.server.peek(key).submits == 0 && Instant::now() < wait_until {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(context.sizes.cancel_after_ms)).await;
        reference = Instant::now();
        token.cancel();
    }
    let joined = tokio::time::timeout(context.watchdog, task).await;
    let elapsed = reference.elapsed();
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let result = match joined {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            observation.violate(
                Criterion::UnexpectedOutcome,
                "esecuzione terminata senza risultato",
            );
            return;
        }
        Err(_) => {
            observation.violate(
                Criterion::CancellationNotHonored,
                "cancellazione o deadline non hanno fermato il polling",
            );
            return;
        }
    };
    absorb(observation, context, &result);
    let known_job = !result.recoveries.is_empty();
    let (code, fault) = match (is_cancel, known_job) {
        (true, true) => ("CANCELLED", "cancellazione_job"),
        (true, false) => ("CANCELLED", "cancellazione_job_prima_del_job_id"),
        (false, true) => ("TIMEOUT", "deadline_job"),
        (false, false) => ("TIMEOUT", "deadline_job_prima_del_job_id"),
    };
    if let Some(error) = expect_failure(observation, &result, &[code]) {
        fault_error(observation, fault, FaultClass::AfterSend, error);
    }
    // La cancellazione remota fa parte del tempo concesso: una richiesta
    // DELETE locale in più rispetto alla tolleranza.
    let limit = if is_cancel {
        context.slack
    } else {
        offset + context.slack
    };
    observation.check(
        elapsed <= limit,
        Criterion::CancellationNotHonored,
        "cancellazione o deadline del job onorate oltre la tolleranza",
    );
    observation.check(
        counters.submits <= 1,
        Criterion::DuplicateSubmit,
        "submit del job ripetuto",
    );
    match result.recoveries.first() {
        Some(recovery) => {
            observation.check(
                recovery.job_id == key
                    && recovery.cancel_requested
                    && recovery.cancel_accepted == Some(true),
                Criterion::UnexpectedOutcome,
                "recovery senza cancellazione remota accettata",
            );
            observation.check(
                counters.cancels == 1,
                Criterion::Amplification,
                "cancellazioni remote diverse da una",
            );
        }
        None => {
            // Interrotto prima di conoscere il job id: nessuna cancellazione
            // remota possibile; l'effetto remoto non può essere escluso, e la
            // classe AfterSend del guasto vieta remote_effect none.
            observation.check(
                counters.cancels == 0,
                Criterion::UnexpectedOutcome,
                "cancellazione remota senza handle di recovery",
            );
        }
    }
}

async fn file_digest(path: &Path) -> Option<(u64, String)> {
    use sha2::{Digest, Sha256};
    let bytes = tokio::fs::read(path).await.ok()?;
    Some((
        bytes.len() as u64,
        crate::http::hex(&Sha256::digest(&bytes)),
    ))
}

async fn partials_for(directory: &Path, file_name: &str) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return false;
    };
    let prefix = format!(".{file_name}.");
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".part") {
            return true;
        }
    }
    false
}

async fn download(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let sizes = &context.sizes;
    let size = rng.between(sizes.download_bytes_min, sizes.download_bytes_max);
    let (mode, resume, max) = match observation.scenario {
        Scenario::DownloadResume => ("cut", true, sizes.max_attempts),
        Scenario::DownloadCorrupt => ("corrupt", false, 1),
        Scenario::DownloadCut => ("cut", false, 1),
        _ => ("plain", false, 1),
    };
    let file_name = format!("{key}.bin");
    let relative = format!("dl/{file_name}");
    let destination = context.files_root.join("dl").join(&file_name);
    let expected = content_sha256(key_seed(key), size);
    let request = json!({
        "schema_version": 1,
        "operation": "download",
        "connection": {
            "url": format!("{}/download/{key}/{size}/{mode}", context.base()),
            "method": "GET",
            "headers": headers(MAIN_ENGINE_LABEL),
            "retry": retry_policy(max, &[503])
        },
        "input": {"file": {"path": relative, "resume": resume, "expected_sha256": expected}}
    });
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let on_disk = file_digest(&destination).await;
    let leftover = partials_for(&context.files_root.join("dl"), &file_name).await;
    observation.check(
        !leftover,
        Criterion::TempFileLeft,
        "file parziale del download rimasto dopo l'operazione",
    );
    let Some(result) = result else {
        let _ = tokio::fs::remove_file(&destination).await;
        return;
    };
    match observation.scenario {
        Scenario::Download | Scenario::DownloadResume => {
            if expect_success(observation, &result) {
                let output_ok = matches!(&result.output, ExecutionOutput::File { direction: FileTransferDirection::Download, bytes_transferred, checksum, .. } if *bytes_transferred == size && checksum.value == expected);
                observation.check(
                    output_ok,
                    Criterion::UnexpectedOutcome,
                    "metadati del download diversi dal contenuto",
                );
                observation.check(
                    on_disk == Some((size, expected.clone())),
                    Criterion::IncompleteFilePublished,
                    "file scaricato diverso dal contenuto atteso",
                );
                if observation.scenario == Scenario::DownloadResume {
                    fault_recovered(observation, "download_interrotto_ripreso");
                    observation.check(
                        counters.range_requests == 1 && counters.hits == 2,
                        Criterion::UnexpectedOutcome,
                        "ripresa senza richiesta Range",
                    );
                }
            } else if observation.scenario == Scenario::DownloadResume {
                fault_failed_unexpectedly(observation, "download_interrotto_ripreso");
            }
            let bound = if observation.scenario == Scenario::DownloadResume {
                u64::from(max)
            } else {
                1
            };
            bound_attempts(observation, counters.hits, bound);
        }
        _ => {
            let (codes, fault): (&[&str], &str) =
                if observation.scenario == Scenario::DownloadCorrupt {
                    (&["CHECKSUM_MISMATCH"], "download_corrotto")
                } else {
                    (&["TRANSPORT_ERROR"], "download_interrotto")
                };
            if let Some(error) = expect_failure(observation, &result, codes) {
                fault_error(observation, fault, FaultClass::AfterSend, error);
            }
            observation.check(
                on_disk.is_none(),
                Criterion::IncompleteFilePublished,
                "file pubblicato dopo un download fallito",
            );
            bound_attempts(observation, counters.hits, 1);
        }
    }
    let _ = tokio::fs::remove_file(&destination).await;
}

async fn upload(observation: &mut Observation, context: &Context, key: &str, rng: &mut SplitMix64) {
    let sizes = &context.sizes;
    let size = rng.between(sizes.upload_bytes_min, sizes.upload_bytes_max);
    let seed = key_seed(key);
    let source = context.files_root.join("ul").join(format!("{key}.bin"));
    if tokio::fs::write(&source, content_chunk(seed, 0, size))
        .await
        .is_err()
    {
        // Disco pieno o directory non scrivibile: l'harness non lascia il
        // file a metà, e l'operazione conta come errore dell'harness.
        let _ = tokio::fs::remove_file(&source).await;
        observation.violate(
            Criterion::HarnessError,
            "file sorgente dell'upload non scrivibile",
        );
        return;
    }
    let expected = content_sha256(seed, size);
    let request = json!({
        "schema_version": 1,
        "operation": "upload",
        "connection": {
            "url": format!("{}/upload/{key}", context.base()),
            "method": "PUT",
            "headers": headers(MAIN_ENGINE_LABEL),
            "request": {"body_type": "raw"}
        },
        "input": {"file": {"path": format!("ul/{key}.bin"), "content_type": "application/octet-stream"}}
    });
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let _ = tokio::fs::remove_file(&source).await;
    let Some(result) = result else { return };
    if expect_success(observation, &result) {
        let response = json!({"bytes": size, "sha256": expected});
        let output_ok = matches!(&result.output, ExecutionOutput::File { direction: FileTransferDirection::Upload, bytes_transferred, checksum, response: Some(body), .. } if *bytes_transferred == size && checksum.value == expected && *body == response);
        observation.check(
            output_ok,
            Criterion::UnexpectedOutcome,
            "metadati dell'upload diversi dal contenuto",
        );
        observation.check(
            counters.upload_bytes == size && counters.upload_sha256 == expected,
            Criterion::UnexpectedOutcome,
            "il servizio ha ricevuto un contenuto diverso",
        );
    }
    bound_attempts(observation, counters.hits, 1);
}

async fn cookie_session(observation: &mut Observation, context: &Context, key: &str) {
    let session = match context.engine.open_cookie_session().await {
        Ok(session) => session,
        Err(_) => {
            observation.violate(
                Criterion::UnexpectedOutcome,
                "apertura della sessione cookie rifiutata",
            );
            return;
        }
    };
    let with_session = |path: &str| {
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{}/cookie/{key}/{path}", context.base()),
                "method": "GET",
                "headers": headers(MAIN_ENGINE_LABEL),
                "cookies": {"session": session}
            }
        })
    };
    let set = with_session("set");
    let check = with_session("check");
    let first = run_request(
        observation,
        context,
        &context.engine,
        set,
        ExecutionControl::default(),
    )
    .await;
    let second = run_request(
        observation,
        context,
        &context.engine,
        check.clone(),
        ExecutionControl::default(),
    )
    .await;
    let closed = context.engine.close_cookie_session(&session).await.is_ok();
    observation.check(
        closed,
        Criterion::UnexpectedOutcome,
        "chiusura della sessione cookie rifiutata",
    );
    let stale = run_request(
        observation,
        context,
        &context.engine,
        check,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    if let (Some(first), Some(second)) = (first, second) {
        if expect_success(observation, &first) && expect_success(observation, &second) {
            let has_cookie = matches!(&second.output, ExecutionOutput::Json { value } if *value == json!({"has_cookie": true}));
            observation.check(
                has_cookie && counters.cookie_seen,
                Criterion::UnexpectedOutcome,
                "la sessione non ha conservato il cookie",
            );
        }
    }
    if let Some(stale) = stale {
        if let Some(error) = expect_failure(observation, &stale, &["POLICY_VIOLATION"]) {
            fault_error(
                observation,
                "sessione_cookie_chiusa",
                FaultClass::LocalRefusal,
                error,
            );
        }
    }
    observation.check(
        counters.hits <= 2,
        Criterion::StaleSessionAccepted,
        "una sessione chiusa ha raggiunto la rete",
    );
}

fn runtime_message(
    rng: &mut SplitMix64,
    operation: &str,
    contract: &str,
    payload: Value,
) -> RuntimeMessage {
    let metadata = BTreeMap::from([
        ("plenora.message.id".to_owned(), uuid(rng)),
        ("plenora.trace.correlation_id".to_owned(), uuid(rng)),
        (
            "plenora.capability.name".to_owned(),
            plenora_rest_core::CAPABILITY_NAME.to_owned(),
        ),
        ("plenora.capability.version".to_owned(), "1".to_owned()),
        (
            "plenora.capability.operation".to_owned(),
            operation.to_owned(),
        ),
        ("plenora.operation.version".to_owned(), "1".to_owned()),
        ("plenora.input.contract".to_owned(), contract.to_owned()),
    ]);
    RuntimeMessage {
        schema_version: 1,
        contract: RUNTIME_INTERFACE_CONTRACT.to_owned(),
        kind: RuntimeMessageKind::Request,
        content_type: "application/json".to_owned(),
        metadata,
        payload,
    }
}

fn scan_message(observation: &mut Observation, context: &Context, message: &RuntimeMessage) {
    match serde_json::to_string(message) {
        Ok(text) => scan(observation, context, &text),
        Err(_) => observation.violate(
            Criterion::UnexpectedOutcome,
            "messaggio del runtime non serializzabile",
        ),
    }
}

async fn runtime_binding(
    observation: &mut Observation,
    context: &Context,
    key: &str,
    rng: &mut SplitMix64,
) {
    let resources = CampaignResources {
        bearer: context.bearer.clone(),
        files_root: context.files_root.clone(),
    };
    let binding = RuntimeBinding::new(&context.engine, &resources);
    let test = runtime_message(
        rng,
        REST_TEST,
        EXECUTION_REQUEST_CONTRACT,
        json!({
            "schema_version": 1,
            "operation": "test",
            "connection": {
                "url": format!("{}/auth/{key}", context.base()),
                "method": "GET",
                "headers": headers(MAIN_ENGINE_LABEL),
                "credential_ref": CREDENTIAL_REFERENCE
            }
        }),
    );
    let response = binding.invoke(test, CancellationToken::new()).await;
    scan_message(observation, context, &response);
    observation.check(
        response.kind == RuntimeMessageKind::Success
            && response.payload["status"] == "success"
            && response.payload["output"]["value"] == json!({"authorized": true}),
        Criterion::UnexpectedOutcome,
        "credential_ref non risolta o risposta non conservata",
    );

    let size = rng.between(
        context.sizes.download_bytes_min,
        context.sizes.download_bytes_max,
    );
    let expected = content_sha256(key_seed(key), size);
    let sink = context.files_root.join("rt").join(format!("{key}.bin"));
    let download = runtime_message(
        rng,
        REST_DOWNLOAD,
        FILE_TRANSFER_INPUT_CONTRACT,
        json!({
            "schema_version": 1,
            "operation": "download",
            "connection": {
                "url": format!("{}/download/{key}/{size}/plain", context.base()),
                "method": "GET",
                "headers": headers(MAIN_ENGINE_LABEL)
            },
            "input": {"file": {"artifact_sink": {"reference": format!("artifact://campaign/{key}")}, "expected_sha256": expected}}
        }),
    );
    let response = binding.invoke(download, CancellationToken::new()).await;
    scan_message(observation, context, &response);
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let succeeded =
        response.kind == RuntimeMessageKind::Success && response.payload["status"] == "success";
    observation.check(
        succeeded,
        Criterion::UnexpectedOutcome,
        "download verso artifact_sink non riuscito",
    );
    let on_disk = file_digest(&sink).await;
    if succeeded {
        observation.check(
            on_disk == Some((size, expected)),
            Criterion::IncompleteFilePublished,
            "artifact scritto diverso dal contenuto atteso",
        );
    } else {
        observation.check(
            on_disk.is_none(),
            Criterion::IncompleteFilePublished,
            "artifact pubblicato dopo un download fallito",
        );
    }
    let _ = tokio::fs::remove_file(&sink).await;
    bound_attempts(observation, counters.hits, 2);
}

async fn network_fault(observation: &mut Observation, context: &Context, key: &str) {
    let max = context.sizes.max_attempts;
    let (url, fault, codes): (String, &str, &[&str]) = match observation.scenario {
        Scenario::DnsFailure => (
            format!("http://campaign-{key}.invalid/ok/{key}"),
            "dns_inesistente",
            &["DNS_RESOLUTION_FAILED"],
        ),
        Scenario::ConnectRefused => (
            format!("http://127.0.0.1:{}/ok/{key}", context.closed_port),
            "connessione_rifiutata",
            // Su Windows il connect verso una porta chiusa viene ritentato dal
            // sistema e si presenta come timeout.
            &["TRANSPORT_ERROR", "TIMEOUT"],
        ),
        Scenario::TlsFailure => (
            format!("https://127.0.0.1:{}/ok/{key}", context.server.port()),
            "tls_fallito",
            &["TRANSPORT_ERROR"],
        ),
        _ => {
            let Some(target) = context.connect_timeout_target.as_deref() else {
                observation.violate(
                    Criterion::HarnessError,
                    "connect_timeout senza destinazione configurata",
                );
                return;
            };
            (
                format!("http://{target}/ok/{key}"),
                "connect_timeout",
                &["TIMEOUT", "TRANSPORT_ERROR"],
            )
        }
    };
    let mut request = test_request(context, url);
    request["connection"]["retry"] = retry_policy(max, &[503]);
    let result = run_request(
        observation,
        context,
        &context.engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    let Some(result) = result else { return };
    if let Some(error) = expect_failure(observation, &result, codes) {
        fault_error(observation, fault, FaultClass::NoRequestSent, error);
    }
    // Nessuna richiesta HTTP deve arrivare al server: il TLS fallito non può
    // degradare a una richiesta in chiaro.
    observation.check(
        counters.hits == 0,
        Criterion::UnexpectedOutcome,
        "richiesta arrivata al servizio nonostante il guasto di rete",
    );
    bound_attempts(observation, result.metrics.requests, u64::from(max));
}

async fn engine_churn(observation: &mut Observation, context: &Context, key: &str) {
    let engine = Engine::new(context.engine_config.clone());
    let session = engine.open_cookie_session().await;
    observation.check(
        session.is_ok(),
        Criterion::UnexpectedOutcome,
        "apertura della sessione cookie rifiutata",
    );
    let mut request = test_request(context, format!("{}/ok/{key}", context.base()));
    request["connection"]["headers"] = headers(CHURN_ENGINE_LABEL);
    if let Ok(session) = &session {
        request["connection"]["cookies"] = json!({"session": session});
    }
    if let Some(result) = run_request(
        observation,
        context,
        &engine,
        request.clone(),
        ExecutionControl::default(),
    )
    .await
    {
        expect_success(observation, &result);
    }
    if let Ok(session) = &session {
        observation.check(
            engine.close_cookie_session(session).await.is_ok(),
            Criterion::UnexpectedOutcome,
            "chiusura della sessione cookie rifiutata",
        );
    }
    engine.close();
    observation.check(
        engine.is_closed(),
        Criterion::UnexpectedOutcome,
        "Engine non chiuso dopo close",
    );
    // Dopo close nessun lavoro: né JSON né tipizzato né nuove sessioni.
    let json_refused = matches!(
        engine.execute_json(&request.to_string()).await,
        Err(EngineError::EngineClosed)
    );
    let session_refused = engine.open_cookie_session().await.is_err();
    let typed = run_request(
        observation,
        context,
        &engine,
        request,
        ExecutionControl::default(),
    )
    .await;
    let typed_refused = typed.as_ref().is_some_and(|result| {
        result.status == ExecutionStatus::Failed
            && result
                .errors
                .first()
                .is_some_and(|error| error.code == "ENGINE_CLOSED")
    });
    if let Some(error) = typed.as_ref().and_then(|result| result.errors.first()) {
        fault_error(
            observation,
            "engine_chiuso",
            FaultClass::LocalRefusal,
            error,
        );
    }
    observation.check(
        json_refused && session_refused && typed_refused,
        Criterion::ClosedEngineAccepted,
        "un Engine chiuso ha accettato lavoro",
    );
    drop(engine);
    let counters = context.server.take(key);
    observation.server_hits = counters.hits;
    observation.check(
        counters.hits == 1,
        Criterion::ClosedEngineAccepted,
        "richieste arrivate al servizio dopo la chiusura dell'Engine",
    );
}

/// Timeout per richiesta degli scenari che verificano una deadline: dieci
/// volte la deadline, così una deadline ignorata si vede come ritardo senza
/// attendere il timeout dell'Engine.
fn fallback_timeout_ms(sizes: &ScenarioSizes) -> u64 {
    sizes.deadline_ms.saturating_mul(10).max(1_000)
}

#[cfg(test)]
mod tests {
    use super::{Scenario, uuid};
    use crate::rng::SplitMix64;

    #[test]
    fn names_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(*scenario));
            assert!(seen.insert(scenario.name()));
        }
        assert_eq!(Scenario::from_name("inesistente"), None);
    }

    #[test]
    fn generated_uuids_are_canonical_v4() {
        let mut rng = SplitMix64::new(1);
        for _ in 0..100 {
            let value = uuid(&mut rng);
            assert_eq!(value.len(), 36);
            assert_eq!(&value[14..15], "4");
            assert!(matches!(&value[19..20], "8" | "9" | "a" | "b"));
            assert!(
                value
                    .bytes()
                    .all(|byte| byte == b'-'
                        || byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            );
        }
    }
}
