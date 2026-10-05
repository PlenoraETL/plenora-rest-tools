//! Prove dell'harness della campagna.
//!
//! - una campagna in-process di pochi secondi esegue ogni scenario e produce
//!   un report coerente con il verificatore;
//! - il verificatore fallisce su report costruiti con una violazione: la
//!   campagna non può passare per costruzione.

use std::{collections::BTreeMap, path::PathBuf};

use plenora_rest_campaign::{
    config::{self, Limits, Phase, PhaseProfile},
    driver::{self, RunPlan},
    report::{
        self, Coverage, Durations, EngineLimitsObserved, Environment, FaultReport, REPORT_SCHEMA,
        Report, ResourceReport, ScenarioReport, Status, Totals, Verdict,
    },
    resources::Sample,
    scenarios::Scenario,
    stats::{self, LatencySummary},
    verify::verify,
};

fn repository_file(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn limits() -> Limits {
    config::load_limits(&repository_file("campaign/limits.json"))
        .expect("campaign/limits.json deve essere valido")
}

fn profile(phase: Phase) -> PhaseProfile {
    config::load_profiles(&repository_file("campaign/profiles.json"))
        .expect("campaign/profiles.json deve essere valido")
        .phase(phase)
        .clone()
}

/// Profilo minimo: pochi secondi, nessuna destinazione esterna.
fn tiny_profile() -> PhaseProfile {
    let mut profile = profile(Phase::Smoke);
    profile.duration_s = 2;
    profile.workers = 3;
    profile.ops_per_second = 6.0;
    profile.sample_interval_s = 1;
    profile.idle_settle_s = 0;
    profile.churn_engines = 1;
    profile.op_watchdog_s = 30;
    profile.connect_timeout_target = None;
    profile.mix.insert("connect_timeout".to_owned(), 0);
    profile.engine.connect_timeout_ms = 300;
    profile.sizes.max_attempts = 2;
    profile.sizes.deadline_ms = 100;
    profile.sizes.cancel_after_ms = 50;
    profile.sizes.stall_timeout_ms = 100;
    profile.sizes.download_bytes_max = 128 * 1024;
    profile.sizes.upload_bytes_max = 128 * 1024;
    profile.sizes.slow_ms_max = 50;
    profile
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_campaign_covers_every_scenario_and_reports_consistently() {
    let work_dir =
        std::env::temp_dir().join(format!("plenora-rest-campaign-test-{}", std::process::id()));
    let limits = limits();
    let plan = RunPlan {
        phase: Phase::Smoke,
        quick: true,
        seed: 7,
        profile: tiny_profile(),
        limits: limits.clone(),
        work_dir: work_dir.clone(),
        commit: "test".to_owned(),
        environment: Environment::default(),
    };
    let report = driver::run(plan).await.expect("la campagna deve terminare");
    let _ = std::fs::remove_dir_all(&work_dir);

    // Copertura: ogni scenario abilitato è stato eseguito, il connect timeout
    // disattivato è dichiarato come tale.
    assert!(
        report
            .coverage
            .disabled
            .iter()
            .any(|disabled| disabled.scenario == "connect_timeout")
    );
    for scenario in Scenario::ALL {
        if *scenario == Scenario::ConnectTimeout {
            continue;
        }
        assert!(
            report
                .coverage
                .executed
                .iter()
                .any(|name| name == scenario.name()),
            "scenario non eseguito: {}",
            scenario.name()
        );
    }
    assert!(
        report.totals.load_operations > 0,
        "nessuna operazione di carico"
    );
    // L'harness non deve aver prodotto errori propri, panic o blocchi.
    assert_eq!(
        report.violations.get("harness_error"),
        None,
        "{:?}",
        report.violation_examples
    );
    assert_eq!(report.panics, 0);
    assert_eq!(report.stuck_operations, 0);
    // I guasti iniettati compaiono con il loro esito.
    for fault in [
        "dns_inesistente",
        "connessione_rifiutata",
        "tls_fallito",
        "corpo_letto_poi_chiusura",
        "polling_timeout_resume",
        "download_corrotto",
    ] {
        assert!(
            report
                .faults
                .get(fault)
                .is_some_and(|fault| fault.injected > 0),
            "guasto non iniettato: {fault}"
        );
    }
    // Nessun file temporaneo a fine campagna e nessun percorso locale nel
    // report.
    let final_sample = report
        .resources
        .final_after_idle
        .clone()
        .unwrap_or_default();
    assert_eq!(final_sample.temp_files + final_sample.partial_files, 0);
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains(&*work_dir.to_string_lossy()));
    assert!(!serialized.contains("campagna-segreto-"));
    // Il verdetto è funzione pura del report.
    let again: Report = serde_json::from_str(&serialized).unwrap();
    assert_eq!(verify(&again, &limits), report.verdict);
    assert!(!report::markdown(&report).is_empty());
}

fn flat_samples(count: u64, interval_ms: u64) -> Vec<Sample> {
    (0..count)
        .map(|step| Sample {
            t_ms: step * interval_ms,
            rss_bytes: Some(100 * 1024 * 1024 + (step % 3) * 4096),
            open_fds: Some(40 + step % 2),
            threads: Some(12),
            temp_files: step % 2,
            temp_bytes: 0,
            partial_files: 0,
            ops_completed: step * 10,
        })
        .collect()
}

fn resource_report(samples: Vec<Sample>, limits: &Limits, measured: bool) -> ResourceReport {
    let end_s = samples
        .last()
        .map_or(0.0, |sample| sample.t_ms as f64 / 1_000.0);
    let warmup_s = limits.warmup_fraction * end_s;
    let window = limits.trend_window_fraction;
    let series = |value: fn(&Sample) -> Option<u64>| -> Vec<(f64, u64)> {
        samples
            .iter()
            .filter_map(|sample| value(sample).map(|v| (sample.t_ms as f64 / 1_000.0, v)))
            .collect()
    };
    let mut last = samples.last().cloned().unwrap_or_default();
    last.temp_files = 0;
    ResourceReport {
        measured,
        note: String::new(),
        measurement_errors: 0,
        warmup_s,
        initial: samples.first().cloned(),
        final_after_idle: Some(last),
        rss_bytes: stats::trend(&series(|sample| sample.rss_bytes), warmup_s, window),
        open_fds: stats::trend(&series(|sample| sample.open_fds), warmup_s, window),
        threads: stats::trend(&series(|sample| sample.threads), warmup_s, window),
        temp_files: stats::trend(&series(|sample| Some(sample.temp_files)), warmup_s, window),
        samples,
    }
}

/// Report di un soak senza anomalie: ogni criterio è superato.
fn clean_soak_report(limits: &Limits) -> Report {
    let profile = profile(Phase::Soak);
    let latency = LatencySummary {
        count: 1_000,
        min_ms: Some(1),
        p50_ms: Some(5),
        p95_ms: Some(20),
        p99_ms: Some(50),
        max_ms: Some(80),
        mean_ms: Some(7),
    };
    let scenarios: BTreeMap<String, ScenarioReport> = Scenario::ALL
        .iter()
        .map(|scenario| {
            (
                scenario.name().to_owned(),
                ScenarioReport {
                    operations: 1_000,
                    as_expected: 1_000,
                    latency: latency.clone(),
                    engine_requests: 1_000,
                    engine_retries: 0,
                    server_hits: 1_000,
                    errors_by_code: BTreeMap::new(),
                },
            )
        })
        .collect();
    let all: Vec<String> = Scenario::ALL
        .iter()
        .map(|scenario| scenario.name().to_owned())
        .collect();
    let mut report = Report {
        schema: REPORT_SCHEMA.to_owned(),
        tool: "plenora-rest-campaign".to_owned(),
        workspace_version: "test".to_owned(),
        commit: "test".to_owned(),
        environment: Environment::default(),
        phase: Phase::Soak,
        quick: false,
        seed: 1,
        started_at: String::new(),
        finished_at: String::new(),
        profile: profile.clone(),
        limits: limits.clone(),
        engine_config: serde_json::Value::Null,
        durations: Durations {
            planned_load_s: 14_400,
            functional_ms: 30_000,
            load_ms: 14_400_000,
            total_ms: 14_440_000,
        },
        totals: Totals {
            operations: 32_000,
            as_expected: 32_000,
            target_ops_per_second: profile.ops_per_second,
            achieved_ops_per_second: profile.ops_per_second,
            server_requests: 32_000,
            ..Totals::default()
        },
        scenarios,
        errors_by_category: BTreeMap::new(),
        errors_by_phase: BTreeMap::new(),
        errors_by_code: BTreeMap::new(),
        faults: BTreeMap::from([(
            "dns_inesistente".to_owned(),
            FaultReport {
                injected: 10,
                coherent: 10,
                ..FaultReport::default()
            },
        )]),
        violations: BTreeMap::new(),
        violation_examples: Vec::new(),
        panics: 0,
        stuck_operations: 0,
        engine_limits: EngineLimitsObserved {
            max_concurrent_requests: 32,
            peak_in_flight: 20,
            requests_per_second: Some(200),
            max_requests_in_one_second: 150,
        },
        resources: resource_report(flat_samples(480, 30_000), limits, true),
        coverage: Coverage {
            required: all.clone(),
            executed: all,
            disabled: Vec::new(),
        },
        verdict: Verdict::default(),
    };
    report.verdict = verify(&report, limits);
    report
}

fn failed_ids(report: &Report, limits: &Limits) -> Vec<String> {
    verify(report, limits)
        .criteria
        .into_iter()
        .filter(|criterion| criterion.status == Status::Fail)
        .map(|criterion| criterion.id)
        .collect()
}

#[test]
fn a_clean_report_passes_every_criterion() {
    let limits = limits();
    let report = clean_soak_report(&limits);
    assert!(report.verdict.passed, "{:?}", failed_ids(&report, &limits));
    assert!(
        report
            .verdict
            .criteria
            .iter()
            .all(|criterion| criterion.status == Status::Pass),
        "un soak completo non deve lasciare criteri non valutati"
    );
}

#[test]
fn the_verifier_fails_on_every_injected_violation() {
    let limits = limits();
    let base = clean_soak_report(&limits);
    let mut cases: Vec<(&str, Report)> = Vec::new();

    for (key, criterion) in [
        ("retry_over_max_attempts", "retry_over_max_attempts"),
        ("duplicate_submit", "duplicate_submit"),
        ("incomplete_file_published", "incomplete_file_published"),
        ("order_loss", "order_loss"),
        ("exposure", "exposure"),
        ("remote_effect_incoherent", "remote_effect_incoherent"),
        ("cancellation_not_honored", "cancellation_not_honored"),
        ("closed_engine_accepted", "closed_engine_accepted"),
        ("stale_session_accepted", "stale_session_accepted"),
        ("amplification", "amplification"),
        ("temp_file_left", "temp_file_left"),
        ("metrics_underreported", "metrics_underreported"),
        ("unexpected_outcome", "esiti_inattesi"),
        ("harness_error", "harness_error"),
    ] {
        let mut report = base.clone();
        report.violations.insert(key.to_owned(), 1);
        cases.push((criterion, report));
    }

    let mut panicked = base.clone();
    panicked.panics = 1;
    cases.push(("panic", panicked));

    let mut stuck = base.clone();
    stuck.stuck_operations = 1;
    cases.push(("operazioni_bloccate", stuck));

    let mut uncovered = base.clone();
    uncovered
        .coverage
        .executed
        .retain(|name| name != "job_resume");
    cases.push(("copertura", uncovered));

    let mut overloaded = base.clone();
    overloaded.engine_limits.peak_in_flight = 33;
    cases.push(("concorrenza", overloaded));

    let mut too_fast = base.clone();
    too_fast.engine_limits.max_requests_in_one_second = 300;
    cases.push(("rate", too_fast));

    let mut slow = base.clone();
    if let Some(ok) = slow.scenarios.get_mut("ok") {
        ok.latency.p99_ms = Some(60_000);
    }
    cases.push(("latenza_p99_ok", slow));

    let mut starved = base.clone();
    starved.totals.achieved_ops_per_second = 1.0;
    cases.push(("throughput", starved));

    // Memoria che cresce linearmente di 1 MiB ogni 30 s per 4 ore.
    let mut leaking = base.clone();
    let mut samples = flat_samples(480, 30_000);
    for (step, sample) in samples.iter_mut().enumerate() {
        sample.rss_bytes = Some(100 * 1024 * 1024 + step as u64 * 1024 * 1024);
    }
    leaking.resources = resource_report(samples, &limits, true);
    cases.push(("andamento_rss", leaking));

    // Descriptor che non tornano: uno ogni minuto.
    let mut fd_leak = base.clone();
    let mut samples = flat_samples(480, 30_000);
    for (step, sample) in samples.iter_mut().enumerate() {
        sample.open_fds = Some(40 + step as u64 / 2);
    }
    fd_leak.resources = resource_report(samples, &limits, true);
    cases.push(("andamento_descriptor", fd_leak));

    let mut residual = base.clone();
    if let Some(last) = residual.resources.final_after_idle.as_mut() {
        last.open_fds = Some(500);
    }
    cases.push(("descriptor_residui", residual));

    let mut leftovers = base.clone();
    if let Some(last) = leftovers.resources.final_after_idle.as_mut() {
        last.partial_files = 1;
    }
    cases.push(("file_temporanei_residui", leftovers));

    // Un soak con troppi pochi campioni non passa per mancanza di dati.
    let mut short = base.clone();
    short.resources = resource_report(flat_samples(5, 30_000), &limits, true);
    cases.push(("andamento_rss", short));

    // Un soak su una piattaforma che non misura le risorse non passa.
    let mut unmeasured = base.clone();
    unmeasured.resources.measured = false;
    cases.push(("rss", unmeasured));

    for (expected, report) in cases {
        let failed = failed_ids(&report, &limits);
        assert!(
            failed.iter().any(|id| id == expected),
            "il criterio {expected} doveva fallire, falliti: {failed:?}"
        );
        assert!(!verify(&report, &limits).passed);
    }
}

#[test]
fn shipped_configuration_is_valid_and_motivated() {
    let limits = limits();
    assert!(limits.approval.contains("da approvare"));
    let profiles = config::load_profiles(&repository_file("campaign/profiles.json")).unwrap();
    for phase in [Phase::Smoke, Phase::Load, Phase::Soak] {
        let profile = profiles.phase(phase);
        // Ogni scenario compare nel mix, anche con peso zero: il mix è una
        // scelta esplicita.
        for scenario in Scenario::ALL {
            assert!(
                profile.mix.contains_key(scenario.name()),
                "scenario assente dal mix: {}",
                scenario.name()
            );
        }
        profile.quick(&profiles.quick).validate().unwrap();
    }
}
