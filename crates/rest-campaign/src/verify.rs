//! Verificatore dei criteri di accettazione.
//!
//! È una funzione pura del report e delle soglie: non consulta lo stato
//! della campagna, quindi si può rieseguire su un report salvato
//! (`--verify-only`) e si può provare con report costruiti a mano. Un
//! criterio che non si può valutare non è mai «superato»: risulta «non
//! valutato» e, quando la fase lo richiede, fa fallire il gate.

use crate::{
    config::Limits,
    report::{CriterionResult, Report, Status, Verdict},
    resources::Sample,
    stats::Trend,
};

const MIB: u64 = 1024 * 1024;

struct Collector {
    criteria: Vec<CriterionResult>,
}

impl Collector {
    fn push(
        &mut self,
        id: &str,
        description: &str,
        status: Status,
        observed: String,
        limit: String,
    ) {
        self.criteria.push(CriterionResult {
            id: id.to_owned(),
            description: description.to_owned(),
            status,
            observed,
            limit,
        });
    }

    fn at_most(&mut self, id: &str, description: &str, observed: u64, limit: u64) {
        let status = if observed <= limit {
            Status::Pass
        } else {
            Status::Fail
        };
        self.push(
            id,
            description,
            status,
            observed.to_string(),
            format!("≤ {limit}"),
        );
    }
}

/// Criteri che contano violazioni osservate operazione per operazione: il
/// limite è sempre zero.
const ZERO_VIOLATIONS: &[(&str, &str)] = &[
    ("retry_over_max_attempts", "retry mai oltre max_attempts"),
    ("amplification", "nessuna amplificazione delle richieste"),
    ("duplicate_submit", "nessun submit duplicato al resume"),
    (
        "incomplete_file_published",
        "nessun file incompleto pubblicato",
    ),
    (
        "temp_file_left",
        "nessun file parziale lasciato dall'operazione",
    ),
    (
        "order_loss",
        "ordine e contenuto dell'enrichment conservati",
    ),
    (
        "exposure",
        "nessun segreto o percorso nei risultati e negli errori",
    ),
    (
        "remote_effect_incoherent",
        "remote_effect e retry advice coerenti con il guasto",
    ),
    (
        "cancellation_not_honored",
        "deadline e cancellazione rispettate",
    ),
    (
        "closed_engine_accepted",
        "un Engine chiuso non accetta lavoro",
    ),
    (
        "stale_session_accepted",
        "una sessione cookie chiusa non raggiunge la rete",
    ),
    ("harness_error", "harness senza errori propri"),
];

/// Valuta il report contro le soglie.
pub fn verify(report: &Report, limits: &Limits) -> Verdict {
    let mut collector = Collector {
        criteria: Vec::new(),
    };
    let violations = |key: &str| report.violations.get(key).copied().unwrap_or(0);

    collector.at_most("panic", "nessun panic", report.panics, 0);
    collector.at_most(
        "operazioni_bloccate",
        "nessuna operazione bloccata (deadlock)",
        report.stuck_operations,
        0,
    );
    collector.at_most(
        "esiti_inattesi",
        "esiti conformi a quelli dichiarati per ogni scenario",
        violations("unexpected_outcome"),
        limits.unexpected_outcomes_max,
    );
    for (key, description) in ZERO_VIOLATIONS {
        collector.at_most(key, description, violations(key), 0);
    }

    let underreported = violations("metrics_underreported");
    if limits.metrics_underreport_is_failure {
        collector.at_most(
            "metrics_underreported",
            "metrics.requests non inferiore alle richieste ricevute dal servizio",
            underreported,
            0,
        );
    } else {
        collector.push(
            "metrics_underreported",
            "metrics.requests non inferiore alle richieste ricevute dal servizio",
            Status::Pass,
            format!("{underreported} operazioni"),
            "non bloccante, da rivedere".to_owned(),
        );
    }

    let conservative: u64 = report.faults.values().map(|fault| fault.conservative).sum();
    let conservative_status = if conservative > 0 && limits.conservative_remote_effect_is_failure {
        Status::Fail
    } else {
        Status::Pass
    };
    collector.push(
        "remote_effect_prudente",
        "remote_effect più prudente del necessario (imprecisione)",
        conservative_status,
        format!("{conservative} osservazioni"),
        if limits.conservative_remote_effect_is_failure {
            "0 (bloccante)".to_owned()
        } else {
            "non bloccante, da rivedere".to_owned()
        },
    );

    coverage(&mut collector, report);
    engine_limits(&mut collector, report, limits);
    latency(&mut collector, report, limits);
    throughput(&mut collector, report, limits);
    resources(&mut collector, report, limits);

    let passed = collector
        .criteria
        .iter()
        .all(|criterion| criterion.status != Status::Fail);
    Verdict {
        passed,
        limits_approval: limits.approval.clone(),
        criteria: collector.criteria,
    }
}

fn coverage(collector: &mut Collector, report: &Report) {
    let missing: Vec<&String> = report
        .coverage
        .required
        .iter()
        .filter(|scenario| !report.coverage.executed.contains(scenario))
        .collect();
    let status = if report.totals.operations > 0 && missing.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    collector.push(
        "copertura",
        "ogni scenario abilitato eseguito almeno una volta",
        status,
        format!(
            "{} eseguiti su {} richiesti, {} non eseguiti",
            report.coverage.executed.len(),
            report.coverage.required.len(),
            missing.len()
        ),
        "tutti".to_owned(),
    );
}

fn engine_limits(collector: &mut Collector, report: &Report, limits: &Limits) {
    let observed = &report.engine_limits;
    if report.totals.server_requests == 0 {
        collector.push(
            "concorrenza",
            "concorrenza dell'Engine entro max_concurrent_requests",
            Status::NotEvaluated,
            "nessuna richiesta ricevuta".to_owned(),
            observed.max_concurrent_requests.to_string(),
        );
    } else {
        collector.at_most(
            "concorrenza",
            "concorrenza dell'Engine entro max_concurrent_requests",
            observed.peak_in_flight,
            observed.max_concurrent_requests,
        );
    }
    match observed.requests_per_second {
        Some(rate) => {
            let allowed = (f64::from(rate) * (1.0 + limits.rate_tolerance_fraction)).floor() as u64
                + limits.rate_tolerance_abs;
            collector.at_most(
                "rate",
                "richieste al secondo entro requests_per_second",
                observed.max_requests_in_one_second,
                allowed,
            );
        }
        None => collector.push(
            "rate",
            "richieste al secondo entro requests_per_second",
            Status::NotEvaluated,
            observed.max_requests_in_one_second.to_string(),
            "rate non configurato nel profilo".to_owned(),
        ),
    }
}

fn latency(collector: &mut Collector, report: &Report, limits: &Limits) {
    for (scenario, limit) in &limits.latency_p99_ms {
        let id = format!("latenza_p99_{scenario}");
        let description = format!("p99 di {scenario}");
        match report
            .scenarios
            .get(scenario)
            .and_then(|stats| stats.latency.p99_ms)
        {
            Some(p99) => collector.at_most(&id, &description, p99, *limit),
            None => collector.push(
                &id,
                &description,
                Status::NotEvaluated,
                "scenario non eseguito".to_owned(),
                format!("≤ {limit} ms"),
            ),
        }
    }
}

fn throughput(collector: &mut Collector, report: &Report, limits: &Limits) {
    let target = report.totals.target_ops_per_second;
    let achieved = report.totals.achieved_ops_per_second;
    let minimum = target * limits.throughput_min_fraction;
    let (status, observed) = if report.durations.load_ms < 1_000 {
        (Status::NotEvaluated, "carico misto non eseguito".to_owned())
    } else if achieved >= minimum {
        (Status::Pass, format!("{achieved:.2} op/s"))
    } else {
        (Status::Fail, format!("{achieved:.2} op/s"))
    };
    collector.push(
        "throughput",
        "throughput del carico misto rispetto all'obiettivo",
        status,
        observed,
        format!("≥ {minimum:.2} op/s"),
    );
}

fn resources(collector: &mut Collector, report: &Report, limits: &Limits) {
    let resources = &report.resources;
    collector.at_most(
        "misure",
        "misure delle risorse senza errori",
        resources.measurement_errors,
        0,
    );
    // Le esecuzioni --quick provano l'harness, non la stabilità: un andamento
    // con pochi campioni lì è «non valutato», mai «superato».
    let required = limits.trend_required_phases.contains(&report.phase) && !report.quick;

    // La directory di lavoro si misura su ogni piattaforma.
    match &resources.final_after_idle {
        Some(Sample {
            temp_files,
            partial_files,
            ..
        }) => collector.at_most(
            "file_temporanei_residui",
            "nessun file temporaneo o parziale a fine campagna",
            temp_files + partial_files,
            0,
        ),
        None => collector.push(
            "file_temporanei_residui",
            "nessun file temporaneo o parziale a fine campagna",
            Status::Fail,
            "campione finale assente".to_owned(),
            "0".to_owned(),
        ),
    }
    collector.at_most(
        "file_temporanei_picco",
        "picco di file temporanei",
        resources.temp_files.peak.unwrap_or(0),
        limits.temp_files_peak_max,
    );
    trend_criterion(
        collector,
        "andamento_file_temporanei",
        "file temporanei stabilizzati dopo il warm-up",
        &resources.temp_files,
        limits.temp_files_growth_max,
        None,
        limits,
        required,
    );

    if !resources.measured {
        let status = if required {
            Status::Fail
        } else {
            Status::NotEvaluated
        };
        for (id, description) in [
            ("rss", "memoria residente (picco e andamento)"),
            ("descriptor", "file descriptor (picco, andamento, residui)"),
            ("thread", "thread stabilizzati"),
        ] {
            collector.push(
                id,
                description,
                status,
                "non misurato su questa piattaforma".to_owned(),
                "misura su Linux".to_owned(),
            );
        }
        return;
    }

    let peak_mib = resources.rss_bytes.peak.map(|peak| peak.div_ceil(MIB));
    match peak_mib {
        Some(peak) => collector.at_most(
            "rss_picco",
            "picco di memoria residente (MiB)",
            peak,
            limits.rss_peak_max_mib,
        ),
        None => collector.push(
            "rss_picco",
            "picco di memoria residente (MiB)",
            Status::Fail,
            "nessun campione".to_owned(),
            format!("≤ {}", limits.rss_peak_max_mib),
        ),
    }
    trend_criterion(
        collector,
        "andamento_rss",
        "memoria residente stabilizzata dopo il warm-up (byte)",
        &resources.rss_bytes,
        limits.rss_growth_max_mib.saturating_mul(MIB),
        Some(limits.rss_slope_max_mib_per_hour * MIB as f64),
        limits,
        required,
    );
    collector.at_most(
        "descriptor_picco",
        "picco di file descriptor",
        resources.open_fds.peak.unwrap_or(u64::MAX),
        limits.fd_peak_max,
    );
    trend_criterion(
        collector,
        "andamento_descriptor",
        "file descriptor stabilizzati dopo il warm-up",
        &resources.open_fds,
        limits.fd_growth_max,
        Some(limits.fd_slope_max_per_hour),
        limits,
        required,
    );
    let residual = match (&resources.initial, &resources.final_after_idle) {
        (Some(initial), Some(last)) => initial
            .open_fds
            .zip(last.open_fds)
            .map(|(initial, last)| last.saturating_sub(initial)),
        _ => None,
    };
    match residual {
        Some(residual) => collector.at_most(
            "descriptor_residui",
            "descriptor in più a riposo rispetto all'inizio",
            residual,
            limits.fd_residual_max,
        ),
        None => collector.push(
            "descriptor_residui",
            "descriptor in più a riposo rispetto all'inizio",
            Status::Fail,
            "campione iniziale o finale assente".to_owned(),
            format!("≤ {}", limits.fd_residual_max),
        ),
    }
    trend_criterion(
        collector,
        "andamento_thread",
        "thread stabilizzati dopo il warm-up",
        &resources.threads,
        limits.threads_growth_max,
        None,
        limits,
        required,
    );
}

/// Una crescita è «non stabilizzata» quando la mediana della finestra
/// finale supera quella iniziale oltre `growth_max` **e**, se è data una
/// pendenza massima, la regressione lineare conferma la tendenza: un picco
/// isolato nella finestra finale non basta, una deriva lenta e costante sì.
#[allow(clippy::too_many_arguments)]
fn trend_criterion(
    collector: &mut Collector,
    id: &str,
    description: &str,
    trend: &Trend,
    growth_max: u64,
    slope_max: Option<f64>,
    limits: &Limits,
    required: bool,
) {
    let limit = match slope_max {
        Some(slope) => format!("crescita ≤ {growth_max} oppure pendenza ≤ {slope:.0}/h"),
        None => format!("crescita ≤ {growth_max}"),
    };
    if trend.samples < limits.trend_min_samples {
        let status = if required {
            Status::Fail
        } else {
            Status::NotEvaluated
        };
        collector.push(
            id,
            description,
            status,
            format!(
                "{} campioni dopo il warm-up, ne servono {}",
                trend.samples, limits.trend_min_samples
            ),
            limit,
        );
        return;
    }
    let Some(growth) = trend.growth else {
        collector.push(
            id,
            description,
            Status::Fail,
            "andamento non calcolabile".to_owned(),
            limit,
        );
        return;
    };
    let growth_exceeded = growth > 0 && growth.unsigned_abs() > growth_max;
    let slope_exceeded = match slope_max {
        Some(max) => trend.slope_per_hour.is_none_or(|slope| slope > max),
        None => true,
    };
    let status = if growth_exceeded && slope_exceeded {
        Status::Fail
    } else {
        Status::Pass
    };
    let observed = match trend.slope_per_hour {
        Some(slope) => format!("crescita {growth}, pendenza {slope:.1}/h"),
        None => format!("crescita {growth}"),
    };
    collector.push(id, description, status, observed, limit);
}
