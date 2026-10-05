//! Report della campagna: aggregazione delle osservazioni, formato JSON
//! versionato e riassunto Markdown.

use std::{collections::BTreeMap, fmt::Write as _};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{Limits, Phase, PhaseProfile},
    resources::Sample,
    scenarios::{Coherence, Criterion, Observation, Scenario},
    stats::{Histogram, LatencySummary, Trend},
};

pub const REPORT_SCHEMA: &str = "plenora-rest-campaign-report-v1";
const MAX_EXAMPLES: usize = 50;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Environment {
    pub os: String,
    pub arch: String,
    pub cpus: u64,
    /// Versione del kernel (Linux), altrimenti «non misurato».
    pub kernel: String,
    /// Toolchain dichiarata dallo script (`CAMPAIGN_TOOLCHAIN`).
    pub toolchain: String,
    /// Profilo di compilazione del binario.
    pub build_profile: String,
    /// Dove gira: `CAMPAIGN_HOST` dello script o `github-actions`.
    pub host: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Durations {
    pub planned_load_s: u64,
    pub functional_ms: u64,
    pub load_ms: u64,
    pub total_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Totals {
    pub operations: u64,
    pub as_expected: u64,
    pub unexpected: u64,
    pub functional_operations: u64,
    pub load_operations: u64,
    pub target_ops_per_second: f64,
    pub achieved_ops_per_second: f64,
    /// Biglietti di carico non avviati perché tutti i worker erano occupati.
    pub missed_ticks: u64,
    pub engine_requests: u64,
    pub engine_retries: u64,
    pub server_requests: u64,
    pub server_connections: u64,
    pub malformed_connections: u64,
    /// Chiavi rimaste nel server a fine campagna (traffico arrivato dopo la
    /// verifica della sua operazione).
    pub server_pending_keys: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ScenarioReport {
    pub operations: u64,
    pub as_expected: u64,
    pub latency: LatencySummary,
    pub engine_requests: u64,
    pub engine_retries: u64,
    pub server_hits: u64,
    pub errors_by_code: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct FaultReport {
    pub injected: u64,
    pub outcomes: BTreeMap<String, u64>,
    pub remote_effects: BTreeMap<String, u64>,
    pub retry_advice: BTreeMap<String, u64>,
    pub coherent: u64,
    pub conservative: u64,
    pub incoherent: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ViolationExample {
    pub scenario: String,
    pub criterion: Criterion,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct EngineLimitsObserved {
    pub max_concurrent_requests: u64,
    pub peak_in_flight: u64,
    pub requests_per_second: Option<u32>,
    pub max_requests_in_one_second: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ResourceReport {
    pub measured: bool,
    pub note: String,
    pub measurement_errors: u64,
    pub warmup_s: f64,
    pub initial: Option<Sample>,
    pub final_after_idle: Option<Sample>,
    pub samples: Vec<Sample>,
    pub rss_bytes: Trend,
    pub open_fds: Trend,
    pub threads: Trend,
    pub temp_files: Trend,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct DisabledScenario {
    pub scenario: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Coverage {
    pub required: Vec<String>,
    pub executed: Vec<String>,
    pub disabled: Vec<DisabledScenario>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    NotEvaluated,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CriterionResult {
    pub id: String,
    pub description: String,
    pub status: Status,
    pub observed: String,
    pub limit: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    /// Stato di approvazione delle soglie usate.
    pub limits_approval: String,
    pub criteria: Vec<CriterionResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Report {
    pub schema: String,
    pub tool: String,
    pub workspace_version: String,
    pub commit: String,
    pub environment: Environment,
    pub phase: Phase,
    pub quick: bool,
    pub seed: u64,
    pub started_at: String,
    pub finished_at: String,
    pub profile: PhaseProfile,
    pub limits: Limits,
    /// Configurazione effettiva dell'Engine principale (il `file_root` è
    /// sostituito da un segnaposto: il report non contiene percorsi locali).
    pub engine_config: Value,
    pub durations: Durations,
    pub totals: Totals,
    pub scenarios: BTreeMap<String, ScenarioReport>,
    pub errors_by_category: BTreeMap<String, u64>,
    pub errors_by_phase: BTreeMap<String, u64>,
    pub errors_by_code: BTreeMap<String, u64>,
    pub faults: BTreeMap<String, FaultReport>,
    pub violations: BTreeMap<String, u64>,
    pub violation_examples: Vec<ViolationExample>,
    pub panics: u64,
    pub stuck_operations: u64,
    pub engine_limits: EngineLimitsObserved,
    pub resources: ResourceReport,
    pub coverage: Coverage,
    pub verdict: Verdict,
}

fn criterion_key(criterion: Criterion) -> String {
    match serde_json::to_value(criterion) {
        Ok(Value::String(text)) => text,
        _ => "non_rappresentabile".to_owned(),
    }
}

#[derive(Default)]
struct ScenarioAggregate {
    operations: u64,
    as_expected: u64,
    latency: Histogram,
    engine_requests: u64,
    engine_retries: u64,
    server_hits: u64,
    errors_by_code: BTreeMap<String, u64>,
}

/// Aggregazione in memoria costante (mappe con chiavi da insiemi finiti).
#[derive(Default)]
pub struct Aggregate {
    scenarios: BTreeMap<Scenario, ScenarioAggregate>,
    errors_by_category: BTreeMap<String, u64>,
    errors_by_phase: BTreeMap<String, u64>,
    errors_by_code: BTreeMap<String, u64>,
    faults: BTreeMap<String, FaultReport>,
    violations: BTreeMap<Criterion, u64>,
    examples: Vec<ViolationExample>,
    pub operations: u64,
    pub as_expected: u64,
    pub stuck: u64,
}

impl Aggregate {
    pub fn record(&mut self, observation: &Observation) {
        self.operations += 1;
        let expected = observation.as_expected();
        if expected {
            self.as_expected += 1;
        }
        let scenario = self.scenarios.entry(observation.scenario).or_default();
        scenario.operations += 1;
        if expected {
            scenario.as_expected += 1;
        }
        let micros = u64::try_from(observation.latency.as_micros()).unwrap_or(u64::MAX);
        scenario.latency.record(micros);
        scenario.engine_requests += observation.requests;
        scenario.engine_retries += observation.retries;
        scenario.server_hits += observation.server_hits;
        for error in &observation.errors {
            *scenario
                .errors_by_code
                .entry(error.code.clone())
                .or_default() += 1;
            *self
                .errors_by_category
                .entry(error.category.clone())
                .or_default() += 1;
            *self.errors_by_phase.entry(error.phase.clone()).or_default() += 1;
            *self.errors_by_code.entry(error.code.clone()).or_default() += 1;
        }
        for fault in &observation.faults {
            let report = self.faults.entry(fault.fault.to_owned()).or_default();
            report.injected += 1;
            *report.outcomes.entry(fault.outcome.clone()).or_default() += 1;
            if let Some(effect) = &fault.remote_effect {
                *report.remote_effects.entry(effect.clone()).or_default() += 1;
            }
            if let Some(retry) = &fault.retry {
                *report.retry_advice.entry(retry.clone()).or_default() += 1;
            }
            match fault.coherence {
                Coherence::Coherent => report.coherent += 1,
                Coherence::Conservative => report.conservative += 1,
                Coherence::Incoherent => report.incoherent += 1,
            }
        }
        for violation in &observation.violations {
            self.add_violation(observation.scenario, violation.criterion, violation.detail);
        }
    }

    pub fn add_violation(&mut self, scenario: Scenario, criterion: Criterion, detail: &str) {
        *self.violations.entry(criterion).or_default() += 1;
        let example = ViolationExample {
            scenario: scenario.name().to_owned(),
            criterion,
            detail: detail.to_owned(),
        };
        if self.examples.len() < MAX_EXAMPLES && !self.examples.contains(&example) {
            self.examples.push(example);
        }
    }

    pub fn record_stuck(&mut self, scenario: Scenario) {
        self.operations += 1;
        self.stuck += 1;
        self.scenarios.entry(scenario).or_default().operations += 1;
    }

    pub fn executed(&self) -> Vec<Scenario> {
        self.scenarios
            .iter()
            .filter(|(_, aggregate)| aggregate.operations > 0)
            .map(|(scenario, _)| *scenario)
            .collect()
    }

    pub fn engine_totals(&self) -> (u64, u64) {
        self.scenarios
            .values()
            .fold((0, 0), |(requests, retries), aggregate| {
                (
                    requests + aggregate.engine_requests,
                    retries + aggregate.engine_retries,
                )
            })
    }

    /// Sezioni del report derivate dalle osservazioni.
    pub fn sections(&self) -> AggregateSections {
        AggregateSections {
            scenarios: self
                .scenarios
                .iter()
                .map(|(scenario, aggregate)| {
                    (
                        scenario.name().to_owned(),
                        ScenarioReport {
                            operations: aggregate.operations,
                            as_expected: aggregate.as_expected,
                            latency: aggregate.latency.summary(),
                            engine_requests: aggregate.engine_requests,
                            engine_retries: aggregate.engine_retries,
                            server_hits: aggregate.server_hits,
                            errors_by_code: aggregate.errors_by_code.clone(),
                        },
                    )
                })
                .collect(),
            errors_by_category: self.errors_by_category.clone(),
            errors_by_phase: self.errors_by_phase.clone(),
            errors_by_code: self.errors_by_code.clone(),
            faults: self.faults.clone(),
            violations: self
                .violations
                .iter()
                .map(|(criterion, count)| (criterion_key(*criterion), *count))
                .collect(),
            violation_examples: self.examples.clone(),
        }
    }
}

pub struct AggregateSections {
    pub scenarios: BTreeMap<String, ScenarioReport>,
    pub errors_by_category: BTreeMap<String, u64>,
    pub errors_by_phase: BTreeMap<String, u64>,
    pub errors_by_code: BTreeMap<String, u64>,
    pub faults: BTreeMap<String, FaultReport>,
    pub violations: BTreeMap<String, u64>,
    pub violation_examples: Vec<ViolationExample>,
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "non misurato".to_owned(), |value| value.to_string())
}

fn mib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "non misurato".to_owned(),
        |bytes| format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
    )
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Pass => "superato",
        Status::Fail => "**FALLITO**",
        Status::NotEvaluated => "non valutato",
    }
}

/// Riassunto Markdown del report.
pub fn markdown(report: &Report) -> String {
    let mut text = String::new();
    let outcome = if report.verdict.passed {
        "SUPERATA"
    } else {
        "FALLITA"
    };
    let _ = writeln!(
        text,
        "# Campagna operativa: fase {}{}\n",
        report.phase,
        if report.quick { " (quick)" } else { "" }
    );
    let _ = writeln!(text, "Esito: **{outcome}**\n");
    let _ = writeln!(text, "| voce | valore |\n| --- | --- |");
    let _ = writeln!(text, "| commit | `{}` |", report.commit);
    let _ = writeln!(
        text,
        "| versione del workspace | {} |",
        report.workspace_version
    );
    let _ = writeln!(
        text,
        "| inizio / fine (UTC) | {} / {} |",
        report.started_at, report.finished_at
    );
    let _ = writeln!(
        text,
        "| ambiente | {} {} ({} CPU), kernel {}, {} |",
        report.environment.os,
        report.environment.arch,
        report.environment.cpus,
        report.environment.kernel,
        report.environment.host
    );
    let _ = writeln!(
        text,
        "| toolchain / profilo | {} / {} |",
        report.environment.toolchain, report.environment.build_profile
    );
    let _ = writeln!(text, "| seed | {} |", report.seed);
    let _ = writeln!(
        text,
        "| durata totale | {:.1} min (funzionale {:.1} s, carico {:.1} min su {} min pianificati) |",
        report.durations.total_ms as f64 / 60_000.0,
        report.durations.functional_ms as f64 / 1_000.0,
        report.durations.load_ms as f64 / 60_000.0,
        report.durations.planned_load_s / 60
    );
    let _ = writeln!(
        text,
        "| profilo | {} worker, obiettivo {} op/s, max_concurrent_requests {}, requests_per_second {} |",
        report.profile.workers,
        report.profile.ops_per_second,
        report.profile.engine.max_concurrent_requests,
        report
            .profile
            .engine
            .requests_per_second
            .map_or_else(|| "non configurato".to_owned(), |rate| rate.to_string())
    );
    let _ = writeln!(
        text,
        "| operazioni | {} ({} attese, {} inattese, {} bloccate) |",
        report.totals.operations,
        report.totals.as_expected,
        report.totals.unexpected,
        report.stuck_operations
    );
    let _ = writeln!(
        text,
        "| throughput carico | {:.2} op/s (biglietti persi {}) |",
        report.totals.achieved_ops_per_second, report.totals.missed_ticks
    );
    let _ = writeln!(
        text,
        "| richieste motore / server | {} / {} (retry {}) |",
        report.totals.engine_requests, report.totals.server_requests, report.totals.engine_retries
    );
    let _ = writeln!(
        text,
        "| concorrenza e rate osservati | picco {} su {}, massimo {} richieste in un secondo |",
        report.engine_limits.peak_in_flight,
        report.engine_limits.max_concurrent_requests,
        report.engine_limits.max_requests_in_one_second
    );
    let _ = writeln!(text, "| soglie | {} |\n", report.verdict.limits_approval);

    let _ = writeln!(text, "## Criteri\n");
    let _ = writeln!(
        text,
        "| criterio | esito | osservato | limite |\n| --- | --- | --- | --- |"
    );
    for criterion in &report.verdict.criteria {
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} |",
            criterion.description,
            status_text(criterion.status),
            criterion.observed,
            criterion.limit
        );
    }

    let _ = writeln!(text, "\n## Risorse\n");
    let resources = &report.resources;
    let _ = writeln!(text, "{}\n", resources.note);
    let _ = writeln!(
        text,
        "| risorsa | iniziale | finale a riposo | picco | crescita dopo warm-up | pendenza/h |\n| --- | --- | --- | --- | --- | --- |"
    );
    let initial = resources.initial.clone().unwrap_or_default();
    let final_sample = resources.final_after_idle.clone().unwrap_or_default();
    let _ = writeln!(
        text,
        "| RSS | {} | {} | {} | {} | {} |",
        mib(initial.rss_bytes),
        mib(final_sample.rss_bytes),
        mib(resources.rss_bytes.peak),
        resources.rss_bytes.growth.map_or_else(
            || "non valutata".to_owned(),
            |growth| format!("{:.1} MiB", growth as f64 / (1024.0 * 1024.0))
        ),
        resources.rss_bytes.slope_per_hour.map_or_else(
            || "non valutata".to_owned(),
            |slope| format!("{:.1} MiB", slope / (1024.0 * 1024.0))
        )
    );
    for (name, initial_value, final_value, trend) in [
        (
            "file descriptor",
            initial.open_fds,
            final_sample.open_fds,
            &resources.open_fds,
        ),
        (
            "thread",
            initial.threads,
            final_sample.threads,
            &resources.threads,
        ),
        (
            "file temporanei",
            Some(initial.temp_files),
            Some(final_sample.temp_files),
            &resources.temp_files,
        ),
    ] {
        let _ = writeln!(
            text,
            "| {name} | {} | {} | {} | {} | {} |",
            optional(initial_value),
            optional(final_value),
            optional(trend.peak),
            optional(trend.growth),
            trend
                .slope_per_hour
                .map_or_else(|| "non valutata".to_owned(), |slope| format!("{slope:.2}"))
        );
    }

    let _ = writeln!(text, "\n## Latenze per scenario (ms)\n");
    let _ = writeln!(
        text,
        "| scenario | operazioni | attese | p50 | p95 | p99 | max | errori |\n| --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    for (name, scenario) in &report.scenarios {
        let errors = scenario
            .errors_by_code
            .iter()
            .map(|(code, count)| format!("{code} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            text,
            "| {name} | {} | {} | {} | {} | {} | {} | {} |",
            scenario.operations,
            scenario.as_expected,
            optional(scenario.latency.p50_ms),
            optional(scenario.latency.p95_ms),
            optional(scenario.latency.p99_ms),
            optional(scenario.latency.max_ms),
            if errors.is_empty() {
                "-".to_owned()
            } else {
                errors
            }
        );
    }

    let _ = writeln!(text, "\n## Guasti iniettati e comportamento osservato\n");
    let _ = writeln!(
        text,
        "| guasto | iniettati | esiti | remote_effect | retry | coerenti / prudenti / incoerenti |\n| --- | --- | --- | --- | --- | --- |"
    );
    let join = |map: &BTreeMap<String, u64>| {
        if map.is_empty() {
            "-".to_owned()
        } else {
            map.iter()
                .map(|(name, count)| format!("{name} {count}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    for (name, fault) in &report.faults {
        let _ = writeln!(
            text,
            "| {name} | {} | {} | {} | {} | {} / {} / {} |",
            fault.injected,
            join(&fault.outcomes),
            join(&fault.remote_effects),
            join(&fault.retry_advice),
            fault.coherent,
            fault.conservative,
            fault.incoherent
        );
    }

    if !report.coverage.disabled.is_empty() {
        let _ = writeln!(text, "\n## Scenari non eseguiti\n");
        for disabled in &report.coverage.disabled {
            let _ = writeln!(text, "- {}: {}", disabled.scenario, disabled.reason);
        }
    }
    if !report.violation_examples.is_empty() {
        let _ = writeln!(text, "\n## Violazioni (esempi)\n");
        for example in &report.violation_examples {
            let _ = writeln!(
                text,
                "- {} — {}: {}",
                example.scenario,
                criterion_key(example.criterion),
                example.detail
            );
        }
    }
    text
}
