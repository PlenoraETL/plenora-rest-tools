//! Configurazione versionata della campagna.
//!
//! - `campaign/profiles.json`: profili di carico delle fasi (durata,
//!   concorrenza, rate, mix degli scenari, dimensioni, configurazione
//!   dell'Engine);
//! - `campaign/limits.json`: soglie dei criteri di accettazione. Sono una
//!   decisione dell'utente: il file contiene default proposti, ciascuno con la
//!   sua motivazione, e lo stato di approvazione viene copiato nel report.
//!
//! Ogni campo è obbligatorio e ogni campo sconosciuto è un errore: una soglia
//! scritta male non diventa un default silenzioso.

use std::{collections::BTreeMap, fmt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{CampaignError, scenarios::Scenario};

pub const PROFILES_SCHEMA: &str = "plenora-rest-campaign-profiles-v1";
pub const LIMITS_SCHEMA: &str = "plenora-rest-campaign-limits-v1";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Smoke,
    Load,
    Soak,
}

impl Phase {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "smoke" => Some(Self::Smoke),
            "load" => Some(Self::Load),
            "soak" => Some(Self::Soak),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Load => "load",
            Self::Soak => "soak",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Configurazione dell'Engine principale (gli altri campi restano ai default
/// del motore, salvo quelli che la campagna deve abilitare: reti private per
/// il server locale, trasferimenti di file nella directory di lavoro e cookie
/// store).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineProfile {
    pub max_concurrent_requests: usize,
    pub requests_per_second: Option<u32>,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub pool_idle_timeout_ms: u64,
    pub max_file_transfer_bytes: u64,
}

/// Dimensioni e parametri degli scenari.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSizes {
    pub download_bytes_min: u64,
    pub download_bytes_max: u64,
    pub upload_bytes_min: u64,
    pub upload_bytes_max: u64,
    pub enrich_records: u64,
    pub enrich_concurrency: u64,
    pub page_total: u64,
    pub page_size: u64,
    pub slow_ms_min: u64,
    pub slow_ms_max: u64,
    /// Tentativi massimi configurati negli scenari con retry.
    pub max_attempts: u32,
    /// Timeout per richiesta dello scenario `stall`.
    pub stall_timeout_ms: u64,
    /// Distanza della deadline e della cancellazione dall'avvio.
    pub deadline_ms: u64,
    pub cancel_after_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PhaseProfile {
    /// Durata del carico misto (dopo il passaggio funzionale).
    pub duration_s: u64,
    /// Operazioni concorrenti del driver.
    pub workers: u64,
    /// Operazioni avviate al secondo (obiettivo).
    pub ops_per_second: f64,
    pub sample_interval_s: u64,
    /// Attesa finale a riposo prima dell'ultimo campione, per lasciar chiudere
    /// le connessioni inattive del pool.
    pub idle_settle_s: u64,
    pub drain_timeout_s: u64,
    /// Oltre questa durata un'operazione è considerata bloccata.
    pub op_watchdog_s: u64,
    /// Engine aperti e chiusi in sequenza nel passaggio funzionale.
    pub churn_engines: u64,
    /// Indirizzo `host:porta` che non risponde, per il guasto di connect
    /// timeout; `null` disattiva lo scenario (dichiarato nel report).
    pub connect_timeout_target: Option<String>,
    pub engine: EngineProfile,
    pub sizes: ScenarioSizes,
    /// Peso di ogni scenario nel carico misto (nome dello scenario → peso).
    pub mix: BTreeMap<String, u64>,
}

/// Riduzioni applicate da `--quick` alla fase scelta.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuickOverrides {
    pub duration_s: u64,
    pub workers: u64,
    pub ops_per_second: f64,
    pub sample_interval_s: u64,
    pub idle_settle_s: u64,
    pub churn_engines: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    pub schema: String,
    pub smoke: PhaseProfile,
    pub load: PhaseProfile,
    pub soak: PhaseProfile,
    pub quick: QuickOverrides,
}

impl Profiles {
    pub fn phase(&self, phase: Phase) -> &PhaseProfile {
        match phase {
            Phase::Smoke => &self.smoke,
            Phase::Load => &self.load,
            Phase::Soak => &self.soak,
        }
    }

    pub fn validate(&self) -> Result<(), CampaignError> {
        if self.schema != PROFILES_SCHEMA {
            return Err(CampaignError::new(
                "versione dello schema dei profili non supportata",
            ));
        }
        for profile in [&self.smoke, &self.load, &self.soak] {
            profile.validate()?;
        }
        if self.quick.workers == 0
            || self.quick.sample_interval_s == 0
            || !(self.quick.ops_per_second.is_finite() && self.quick.ops_per_second > 0.0)
        {
            return Err(CampaignError::new(
                "profilo quick: workers, ops_per_second e sample_interval_s devono essere positivi",
            ));
        }
        Ok(())
    }
}

impl PhaseProfile {
    pub fn validate(&self) -> Result<(), CampaignError> {
        if self.workers == 0 || self.sample_interval_s == 0 || self.op_watchdog_s == 0 {
            return Err(CampaignError::new(
                "profilo: workers, sample_interval_s e op_watchdog_s devono essere positivi",
            ));
        }
        if !(self.ops_per_second.is_finite() && self.ops_per_second > 0.0) {
            return Err(CampaignError::new(
                "profilo: ops_per_second deve essere un numero positivo",
            ));
        }
        if self.engine.max_concurrent_requests == 0 {
            return Err(CampaignError::new(
                "profilo: max_concurrent_requests deve essere positivo",
            ));
        }
        let sizes = &self.sizes;
        if sizes.download_bytes_min == 0
            || sizes.download_bytes_min > sizes.download_bytes_max
            || sizes.upload_bytes_min == 0
            || sizes.upload_bytes_min > sizes.upload_bytes_max
            || sizes.slow_ms_min > sizes.slow_ms_max
            || sizes.page_size == 0
            || sizes.enrich_records == 0
            || sizes.enrich_concurrency == 0
            || sizes.max_attempts < 2
        {
            return Err(CampaignError::new(
                "profilo: dimensioni degli scenari incoerenti",
            ));
        }
        if sizes.download_bytes_max > self.engine.max_file_transfer_bytes
            || sizes.upload_bytes_max > self.engine.max_file_transfer_bytes
        {
            return Err(CampaignError::new(
                "profilo: i trasferimenti superano max_file_transfer_bytes",
            ));
        }
        let mut total = 0_u64;
        for (name, weight) in &self.mix {
            if Scenario::from_name(name).is_none() {
                return Err(CampaignError::new(
                    "profilo: il mix nomina uno scenario sconosciuto",
                ));
            }
            total = total
                .checked_add(*weight)
                .ok_or(CampaignError::new("profilo: pesi del mix fuori intervallo"))?;
        }
        if total == 0 {
            return Err(CampaignError::new(
                "profilo: il mix non contiene scenari con peso positivo",
            ));
        }
        if self
            .mix
            .get(Scenario::ConnectTimeout.name())
            .copied()
            .unwrap_or(0)
            > 0
            && self.connect_timeout_target.is_none()
        {
            return Err(CampaignError::new(
                "profilo: connect_timeout nel mix richiede connect_timeout_target",
            ));
        }
        Ok(())
    }

    /// Applica le riduzioni di `--quick`.
    pub fn quick(&self, overrides: &QuickOverrides) -> Self {
        let mut profile = self.clone();
        profile.duration_s = overrides.duration_s;
        profile.workers = overrides.workers;
        profile.ops_per_second = overrides.ops_per_second;
        profile.sample_interval_s = overrides.sample_interval_s;
        profile.idle_settle_s = overrides.idle_settle_s;
        profile.churn_engines = overrides.churn_engines;
        profile
    }
}

/// Soglie dei criteri di accettazione.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub schema: String,
    /// Stato di approvazione, copiato nel report (per esempio «proposta, da
    /// approvare»).
    pub approval: String,
    pub unexpected_outcomes_max: u64,
    /// Se vero, un `remote_effect` più prudente del necessario (`unknown` dove
    /// il guasto esclude ogni effetto) fa fallire il gate; se falso è
    /// registrato come imprecisione non bloccante.
    pub conservative_remote_effect_is_failure: bool,
    /// Se vero, `metrics.requests` inferiore alle richieste ricevute dal
    /// servizio per la stessa operazione fa fallire il gate.
    pub metrics_underreport_is_failure: bool,
    /// Ritardo massimo tra deadline o cancellazione e il ritorno del motore.
    pub cancellation_slack_ms: u64,
    /// p99 massimo in millisecondi per scenario.
    pub latency_p99_ms: BTreeMap<String, u64>,
    /// Frazione minima del rate obiettivo effettivamente avviata.
    pub throughput_min_fraction: f64,
    pub rate_tolerance_fraction: f64,
    pub rate_tolerance_abs: u64,
    pub rss_peak_max_mib: u64,
    pub rss_growth_max_mib: u64,
    pub rss_slope_max_mib_per_hour: f64,
    pub fd_peak_max: u64,
    pub fd_growth_max: u64,
    pub fd_slope_max_per_hour: f64,
    /// Descriptor in più rispetto all'inizio dopo l'attesa finale a riposo.
    pub fd_residual_max: u64,
    pub threads_growth_max: u64,
    pub temp_files_peak_max: u64,
    pub temp_files_growth_max: u64,
    pub trend_min_samples: usize,
    pub trend_window_fraction: f64,
    pub warmup_fraction: f64,
    /// Fasi in cui un andamento non valutabile (troppi pochi campioni) fa
    /// fallire il gate invece di risultare non valutato.
    pub trend_required_phases: Vec<Phase>,
    /// Motivazione di ogni soglia (nome del campo → testo).
    pub motivations: BTreeMap<String, String>,
}

impl Limits {
    pub fn validate(&self) -> Result<(), CampaignError> {
        if self.schema != LIMITS_SCHEMA {
            return Err(CampaignError::new(
                "versione dello schema delle soglie non supportata",
            ));
        }
        let fractions = [
            self.throughput_min_fraction,
            self.rate_tolerance_fraction,
            self.trend_window_fraction,
            self.warmup_fraction,
        ];
        if fractions
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0 || *value > 1.0)
            || self.trend_window_fraction == 0.0
        {
            return Err(CampaignError::new(
                "soglie: le frazioni devono stare in [0, 1] (finestra positiva)",
            ));
        }
        if !self.rss_slope_max_mib_per_hour.is_finite() || !self.fd_slope_max_per_hour.is_finite() {
            return Err(CampaignError::new("soglie: pendenze non finite"));
        }
        if self.trend_min_samples < 4 {
            return Err(CampaignError::new(
                "soglie: trend_min_samples deve essere almeno 4",
            ));
        }
        for name in self.latency_p99_ms.keys() {
            if Scenario::from_name(name).is_none() {
                return Err(CampaignError::new(
                    "soglie: latency_p99_ms nomina uno scenario sconosciuto",
                ));
            }
        }
        // Ogni soglia ha la sua motivazione scritta: è una decisione.
        let value = serde_json::to_value(self)
            .map_err(|_| CampaignError::new("soglie non serializzabili"))?;
        if let Some(fields) = value.as_object() {
            for field in fields.keys() {
                if matches!(field.as_str(), "schema" | "approval" | "motivations") {
                    continue;
                }
                if !self.motivations.contains_key(field) {
                    return Err(CampaignError::new("soglie: una soglia è senza motivazione"));
                }
            }
        }
        Ok(())
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    missing: &'static str,
    invalid: &'static str,
) -> Result<T, CampaignError> {
    let text = std::fs::read_to_string(path).map_err(|_| CampaignError::new(missing))?;
    serde_json::from_str(&text).map_err(|_| CampaignError::new(invalid))
}

pub fn load_profiles(path: &Path) -> Result<Profiles, CampaignError> {
    let profiles: Profiles = read_json(
        path,
        "file dei profili non leggibile",
        "file dei profili non conforme allo schema",
    )?;
    profiles.validate()?;
    Ok(profiles)
}

pub fn load_limits(path: &Path) -> Result<Limits, CampaignError> {
    let limits: Limits = read_json(
        path,
        "file delle soglie non leggibile",
        "file delle soglie non conforme allo schema",
    )?;
    limits.validate()?;
    Ok(limits)
}
