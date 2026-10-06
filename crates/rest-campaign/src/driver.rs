//! Driver della campagna: passaggio funzionale, carico misto, campionamento
//! delle risorse, controlli finali e costruzione del report.
//!
//! Sequenza di una fase:
//!
//! 1. campione iniziale delle risorse;
//! 2. passaggio funzionale: ogni scenario abilitato una volta, in ordine,
//!    più `churn_engines` Engine aperti e chiusi (copertura garantita anche se
//!    il mix non estrae uno scenario);
//! 3. carico misto per `duration_s`: un generatore emette biglietti al rate
//!    obiettivo, ognuno con scenario e seed derivati dal seed della campagna,
//!    e `workers` operazioni concorrenti li eseguono; un biglietto che trova
//!    tutti i worker occupati è contato come perso, mai accodato senza limite;
//! 4. drenaggio, attesa a riposo e campione finale;
//! 5. chiusura dell'Engine (che da quel momento deve rifiutare il lavoro) e
//!    verifica dei criteri.
//!
//! Ogni operazione gira in un task con un watchdog: un panic è contatore del
//! report, un'operazione che non termina è un'operazione bloccata.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use plenora_rest_core::{Engine, EngineConfig, EngineError};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::{
    CampaignError,
    config::{Limits, Phase, PhaseProfile},
    report::{
        Aggregate, Coverage, DisabledScenario, Durations, EngineLimitsObserved, Environment,
        REPORT_SCHEMA, Report, ResourceReport, Totals,
    },
    resources::{self, Sample},
    rng::{self, SplitMix64},
    scenarios::{self, Context, Criterion, MAIN_ENGINE_LABEL, Scenario},
    server::TestServer,
    stats, timefmt, verify,
};

static PANICS: AtomicU64 = AtomicU64::new(0);
static PANIC_HOOK: OnceLock<()> = OnceLock::new();

/// Installa (una volta) un gancio che conta i panic di qualunque thread,
/// compresi quelli dei task interni del motore, e poi delega al gancio
/// precedente.
pub fn install_panic_counter() {
    PANIC_HOOK.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            previous(info);
        }));
    });
}

pub fn panic_count() -> u64 {
    PANICS.load(Ordering::SeqCst)
}

/// Che cosa eseguire.
#[derive(Clone, Debug)]
pub struct RunPlan {
    pub phase: Phase,
    pub quick: bool,
    pub seed: u64,
    /// Profilo effettivo (già ridotto da `--quick` e con la durata scelta).
    pub profile: PhaseProfile,
    pub limits: Limits,
    /// Directory sotto cui la campagna crea la propria directory di lavoro.
    pub work_dir: PathBuf,
    pub commit: String,
    pub environment: Environment,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Sampler {
    started: Instant,
    files_root: PathBuf,
    samples: Mutex<Vec<Sample>>,
    errors: AtomicU64,
    ops_completed: AtomicU64,
}

impl Sampler {
    fn take(&self) -> Sample {
        let process = match resources::process_resources() {
            Ok(process) => process,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                resources::ProcessResources::default()
            }
        };
        let usage = match resources::directory_usage(&self.files_root) {
            Ok(usage) => usage,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                resources::DirectoryUsage::default()
            }
        };
        Sample {
            t_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            rss_bytes: process.rss_bytes,
            open_fds: process.open_fds,
            threads: process.threads,
            temp_files: usage.files,
            temp_bytes: usage.bytes,
            partial_files: usage.partial_files,
            ops_completed: self.ops_completed.load(Ordering::Relaxed),
        }
    }

    fn record(&self) {
        let sample = self.take();
        locked(&self.samples).push(sample);
    }
}

struct Runner {
    context: Arc<Context>,
    aggregate: Arc<Mutex<Aggregate>>,
    sampler: Arc<Sampler>,
    seed: u64,
    watchdog: Duration,
}

impl Runner {
    async fn run_op(&self, scenario: Scenario, ticket: u64) {
        let key = format!("{}-{ticket}", scenario.name().replace('_', "-"));
        let seed = rng::derive(self.seed, ticket);
        let context = Arc::clone(&self.context);
        let mut handle =
            tokio::spawn(async move { scenarios::run(scenario, &context, &key, seed).await });
        let outcome = tokio::time::timeout(self.watchdog, &mut handle).await;
        let mut aggregate = locked(&self.aggregate);
        match outcome {
            Ok(Ok(observation)) => aggregate.record(&observation),
            Ok(Err(_)) => aggregate.add_violation(
                scenario,
                Criterion::UnexpectedOutcome,
                "operazione terminata da un panic",
            ),
            Err(_) => {
                handle.abort();
                aggregate.record_stuck(scenario);
            }
        }
        drop(aggregate);
        self.sampler.ops_completed.fetch_add(1, Ordering::Relaxed);
    }
}

fn path_forms(path: &Path) -> Vec<String> {
    let mut forms = Vec::new();
    let mut push = |text: String| {
        if !text.is_empty() && !forms.contains(&text) {
            // La forma con escape JSON è quella che compare in un risultato
            // serializzato.
            let escaped = text.replace('\\', "\\\\");
            if escaped != text {
                forms.push(escaped);
            }
            forms.push(text);
        }
    };
    push(path.to_string_lossy().into_owned());
    if let Ok(canonical) = std::fs::canonicalize(path) {
        let text = canonical.to_string_lossy().into_owned();
        push(text.trim_start_matches(r"\\?\").to_owned());
        push(text);
    }
    forms
}

fn enabled_scenarios(profile: &PhaseProfile) -> (Vec<Scenario>, Vec<DisabledScenario>) {
    let mut enabled = Vec::new();
    let mut disabled = Vec::new();
    for scenario in Scenario::ALL {
        if *scenario == Scenario::ConnectTimeout && profile.connect_timeout_target.is_none() {
            disabled.push(DisabledScenario {
                scenario: scenario.name().to_owned(),
                reason: "connect_timeout_target non configurato nel profilo".to_owned(),
            });
        } else {
            enabled.push(*scenario);
        }
    }
    (enabled, disabled)
}

fn engine_config(profile: &PhaseProfile, files_root: &Path) -> EngineConfig {
    EngineConfig {
        allow_private_networks: true,
        allow_file_transfers: true,
        allow_cookie_store: true,
        file_root: Some(files_root.to_string_lossy().into_owned()),
        max_concurrent_requests: profile.engine.max_concurrent_requests,
        requests_per_second: profile.engine.requests_per_second,
        connect_timeout_ms: profile.engine.connect_timeout_ms,
        request_timeout_ms: profile.engine.request_timeout_ms,
        pool_idle_timeout_ms: profile.engine.pool_idle_timeout_ms,
        max_file_transfer_bytes: profile.engine.max_file_transfer_bytes,
        ..EngineConfig::default()
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Esegue la fase descritta da `plan` e restituisce il report verificato.
pub async fn run(plan: RunPlan) -> Result<Report, CampaignError> {
    install_panic_counter();
    let panics_before = panic_count();
    let started_wall = SystemTime::now();
    let started = Instant::now();
    let profile = plan.profile.clone();
    profile.validate()?;

    let run_dir = plan
        .work_dir
        .join(format!("run-{}-{}", std::process::id(), plan.seed));
    if run_dir.exists() {
        return Err(CampaignError::new(
            "la directory di lavoro della campagna esiste già",
        ));
    }
    let files_root = run_dir.join("files");
    for sub in ["dl", "ul", "rt"] {
        std::fs::create_dir_all(files_root.join(sub))
            .map_err(|_| CampaignError::new("directory di lavoro non creabile"))?;
    }
    let bearer = format!("campagna-segreto-{:016x}", rng::derive(plan.seed, 0x5EC));
    let watchdog = Duration::from_secs(profile.op_watchdog_s);
    let server = Arc::new(TestServer::start(bearer.clone(), watchdog).await?);
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|_| CampaignError::new("porta chiusa di prova non ottenibile"))?;
        listener
            .local_addr()
            .map_err(|_| CampaignError::new("porta chiusa di prova non leggibile"))?
            .port()
    };
    let config = engine_config(&profile, &files_root);
    let engine = Arc::new(Engine::new(config.clone()));
    let mut forbidden = path_forms(&run_dir);
    forbidden.extend(path_forms(&files_root));
    let context = Arc::new(Context {
        engine: Arc::clone(&engine),
        server: Arc::clone(&server),
        files_root: files_root.clone(),
        forbidden,
        bearer,
        closed_port,
        sizes: profile.sizes.clone(),
        engine_config: config.clone(),
        connect_timeout_target: profile.connect_timeout_target.clone(),
        slack: Duration::from_millis(plan.limits.cancellation_slack_ms),
        watchdog,
    });
    let sampler = Arc::new(Sampler {
        started,
        files_root: files_root.clone(),
        samples: Mutex::new(Vec::new()),
        errors: AtomicU64::new(0),
        ops_completed: AtomicU64::new(0),
    });
    let initial = sampler.take();
    locked(&sampler.samples).push(initial.clone());
    let (stop_sampling, mut sampling_stopped) = watch::channel(false);
    let sampler_task = {
        let sampler = Arc::clone(&sampler);
        let interval = Duration::from_secs(profile.sample_interval_s);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = sampling_stopped.changed() => break,
                    _ = ticker.tick() => sampler.record(),
                }
            }
        })
    };
    let aggregate = Arc::new(Mutex::new(Aggregate::default()));
    let runner = Arc::new(Runner {
        context: Arc::clone(&context),
        aggregate: Arc::clone(&aggregate),
        sampler: Arc::clone(&sampler),
        seed: plan.seed,
        watchdog,
    });
    let (enabled, disabled) = enabled_scenarios(&profile);

    // Passaggio funzionale.
    let mut ticket = 0_u64;
    for scenario in &enabled {
        runner.run_op(*scenario, ticket).await;
        ticket += 1;
    }
    for _ in 0..profile.churn_engines {
        runner.run_op(Scenario::EngineChurn, ticket).await;
        ticket += 1;
    }
    let functional_ops = locked(&aggregate).operations;
    let functional_elapsed = started.elapsed();

    // Carico misto.
    let load_started = Instant::now();
    let weighted: Vec<(Scenario, u64)> = profile
        .mix
        .iter()
        .filter_map(|(name, weight)| {
            let scenario = Scenario::from_name(name)?;
            (*weight > 0 && enabled.contains(&scenario)).then_some((scenario, *weight))
        })
        .collect();
    let total_weight: u64 = weighted.iter().map(|(_, weight)| weight).sum();
    let load_duration = Duration::from_secs(profile.duration_s);
    let mut dispatched = 0_u64;
    let mut missed = 0_u64;
    if !load_duration.is_zero() && total_weight > 0 {
        let workers = usize::try_from(profile.workers).unwrap_or(usize::MAX);
        let (sender, receiver) = mpsc::channel::<(Scenario, u64)>(workers);
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        let mut worker_tasks = Vec::with_capacity(workers);
        for _ in 0..workers {
            let receiver = Arc::clone(&receiver);
            let runner = Arc::clone(&runner);
            worker_tasks.push(tokio::spawn(async move {
                loop {
                    let next = receiver.lock().await.recv().await;
                    let Some((scenario, ticket)) = next else {
                        break;
                    };
                    runner.run_op(scenario, ticket).await;
                }
            }));
        }
        let mut chooser = SplitMix64::new(rng::derive(plan.seed, u64::MAX));
        let period = Duration::from_secs_f64(1.0 / profile.ops_per_second);
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let end = load_started + load_duration;
        loop {
            ticker.tick().await;
            if Instant::now() >= end {
                break;
            }
            let mut pick = chooser.below(total_weight);
            let mut scenario = weighted[0].0;
            for (candidate, weight) in &weighted {
                if pick < *weight {
                    scenario = *candidate;
                    break;
                }
                pick -= weight;
            }
            match sender.try_send((scenario, ticket)) {
                Ok(()) => dispatched += 1,
                Err(mpsc::error::TrySendError::Full(_)) => missed += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
            ticket += 1;
        }
        drop(sender);
        let drain = Duration::from_secs(profile.drain_timeout_s) + watchdog;
        let drained = tokio::time::timeout(drain, async {
            for task in &mut worker_tasks {
                let _ = task.await;
            }
        })
        .await;
        if drained.is_err() {
            for task in &worker_tasks {
                task.abort();
            }
            locked(&aggregate).record_stuck(Scenario::Ok);
        }
    }
    let load_elapsed = load_started.elapsed();
    let load_ops = locked(&aggregate).operations.saturating_sub(functional_ops);
    let end_of_activity = started.elapsed();

    // Riposo, poi campione finale.
    let _ = stop_sampling.send(true);
    let _ = sampler_task.await;
    tokio::time::sleep(Duration::from_secs(profile.idle_settle_s)).await;
    let final_sample = sampler.take();

    // L'Engine chiuso deve rifiutare il lavoro.
    engine.close();
    let refused = matches!(
        engine
            .execute_json(r#"{"schema_version":1,"operation":"test","connection":{"url":"http://127.0.0.1:9/"}}"#)
            .await,
        Err(EngineError::EngineClosed)
    );
    if !refused {
        locked(&aggregate).add_violation(
            Scenario::EngineChurn,
            Criterion::ClosedEngineAccepted,
            "l'Engine principale ha accettato lavoro dopo close",
        );
    }
    let labels = server.label_stats();
    let main = labels.get(MAIN_ENGINE_LABEL).cloned().unwrap_or_default();
    let server_requests = server.requests();
    let server_connections = server.connections();
    let malformed = server.malformed_connections();
    let pending_keys = server.pending_keys() as u64;
    server.shutdown();
    drop(runner);
    drop(context);
    let _ = std::fs::remove_dir_all(&run_dir);

    let samples = std::mem::take(&mut *locked(&sampler.samples));
    let warmup_s = plan.limits.warmup_fraction * end_of_activity.as_secs_f64();
    let series = |value: fn(&Sample) -> Option<u64>| -> Vec<(f64, u64)> {
        samples
            .iter()
            .filter_map(|sample| value(sample).map(|v| (sample.t_ms as f64 / 1_000.0, v)))
            .collect()
    };
    let window = plan.limits.trend_window_fraction;
    let measured = resources::process_resources_measured();
    let resource_report = ResourceReport {
        measured,
        note: if measured {
            "RSS, file descriptor e thread letti da /proc/self (Linux).".to_owned()
        } else {
            "RSS, file descriptor e thread non misurati su questa piattaforma: i criteri relativi non sono valutati.".to_owned()
        },
        measurement_errors: sampler.errors.load(Ordering::Relaxed),
        warmup_s,
        initial: Some(initial),
        final_after_idle: Some(final_sample),
        rss_bytes: stats::trend(&series(|sample| sample.rss_bytes), warmup_s, window),
        open_fds: stats::trend(&series(|sample| sample.open_fds), warmup_s, window),
        threads: stats::trend(&series(|sample| sample.threads), warmup_s, window),
        temp_files: stats::trend(&series(|sample| Some(sample.temp_files)), warmup_s, window),
        samples,
    };

    let aggregate = locked(&aggregate);
    let sections = aggregate.sections();
    let (engine_requests, engine_retries) = aggregate.engine_totals();
    let executed: Vec<String> = aggregate
        .executed()
        .iter()
        .map(|scenario| scenario.name().to_owned())
        .collect();
    let load_seconds = load_duration.as_secs_f64();
    let mut engine_config_value = serde_json::to_value(&config).unwrap_or(Value::Null);
    if let Some(object) = engine_config_value.as_object_mut() {
        object.insert(
            "file_root".to_owned(),
            Value::String("<directory di lavoro della campagna>".to_owned()),
        );
    }
    let mut report = Report {
        schema: REPORT_SCHEMA.to_owned(),
        tool: "plenora-rest-campaign".to_owned(),
        workspace_version: env!("CARGO_PKG_VERSION").to_owned(),
        commit: plan.commit.clone(),
        environment: plan.environment.clone(),
        phase: plan.phase,
        quick: plan.quick,
        seed: plan.seed,
        started_at: timefmt::rfc3339(started_wall).unwrap_or_default(),
        finished_at: timefmt::rfc3339(SystemTime::now()).unwrap_or_default(),
        profile: profile.clone(),
        limits: plan.limits.clone(),
        engine_config: engine_config_value,
        durations: Durations {
            planned_load_s: profile.duration_s,
            functional_ms: millis(functional_elapsed),
            load_ms: millis(load_elapsed),
            total_ms: millis(started.elapsed()),
        },
        totals: Totals {
            operations: aggregate.operations,
            as_expected: aggregate.as_expected,
            unexpected: aggregate.operations - aggregate.as_expected,
            functional_operations: functional_ops,
            load_operations: load_ops,
            target_ops_per_second: profile.ops_per_second,
            achieved_ops_per_second: if load_seconds > 0.0 {
                dispatched as f64 / load_seconds
            } else {
                0.0
            },
            missed_ticks: missed,
            engine_requests,
            engine_retries,
            server_requests,
            server_connections,
            malformed_connections: malformed,
            server_pending_keys: pending_keys,
        },
        scenarios: sections.scenarios,
        errors_by_category: sections.errors_by_category,
        errors_by_phase: sections.errors_by_phase,
        errors_by_code: sections.errors_by_code,
        faults: sections.faults,
        violations: sections.violations,
        violation_examples: sections.violation_examples,
        panics: panic_count().saturating_sub(panics_before),
        stuck_operations: aggregate.stuck,
        engine_limits: EngineLimitsObserved {
            max_concurrent_requests: profile.engine.max_concurrent_requests as u64,
            peak_in_flight: main.peak_in_flight,
            requests_per_second: profile.engine.requests_per_second,
            max_requests_in_one_second: main.max_per_second,
        },
        resources: resource_report,
        coverage: Coverage {
            required: enabled
                .iter()
                .map(|scenario| scenario.name().to_owned())
                .collect(),
            executed,
            disabled,
        },
        verdict: Default::default(),
    };
    drop(aggregate);
    report.verdict = verify::verify(&report, &plan.limits);
    Ok(report)
}
