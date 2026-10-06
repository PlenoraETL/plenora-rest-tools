//! `plenora-rest-campaign`: esegue una fase della campagna operativa e scrive
//! il report JSON e il riassunto Markdown.
//!
//! Exit code: 0 campagna superata, 1 criteri falliti, 2 uso o configurazione
//! non validi, 3 errore dell'harness.

#![forbid(unsafe_code)]

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use plenora_rest_campaign::{
    CampaignError,
    config::{self, Phase},
    driver::{self, RunPlan},
    report::{self, Environment, Report},
    verify,
};

const USAGE: &str = "uso:
  plenora-rest-campaign --phase smoke|load|soak [opzioni]
  plenora-rest-campaign --verify-only REPORT.json [--limits FILE] [--out PREFISSO]

opzioni:
  --quick               riduce la fase al profilo quick (CI, poche decine di secondi)
  --duration-min N      durata del carico misto in minuti (sostituisce il profilo)
  --seed N              seed della campagna (default 1)
  --profiles FILE       profili di carico (default campaign/profiles.json)
  --limits FILE         soglie (default campaign/limits.json)
  --out PREFISSO        scrive PREFISSO.json e PREFISSO.md (default campaign-out/<fase>)
  --work-dir DIR        directory dei file temporanei (default: temporanea di sistema)
  --commit SHA          commit esaminato (default: CAMPAIGN_COMMIT, GITHUB_SHA o git)
";

struct Arguments {
    phase: Option<Phase>,
    quick: bool,
    duration_min: Option<u64>,
    seed: u64,
    profiles: PathBuf,
    limits: PathBuf,
    out: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    commit: Option<String>,
    verify_only: Option<PathBuf>,
}

fn parse_arguments() -> Result<Arguments, &'static str> {
    let mut arguments = Arguments {
        phase: None,
        quick: false,
        duration_min: None,
        seed: 1,
        profiles: PathBuf::from("campaign/profiles.json"),
        limits: PathBuf::from("campaign/limits.json"),
        out: None,
        work_dir: None,
        commit: None,
        verify_only: None,
    };
    let mut iterator = std::env::args().skip(1);
    while let Some(argument) = iterator.next() {
        let mut value = || iterator.next().ok_or("opzione senza valore");
        match argument.as_str() {
            "--phase" => {
                arguments.phase = Some(Phase::parse(&value()?).ok_or("fase sconosciuta")?);
            }
            "--quick" => arguments.quick = true,
            "--duration-min" => {
                arguments.duration_min = Some(value()?.parse().map_err(|_| "durata non valida")?);
            }
            "--seed" => arguments.seed = value()?.parse().map_err(|_| "seed non valido")?,
            "--profiles" => arguments.profiles = PathBuf::from(value()?),
            "--limits" => arguments.limits = PathBuf::from(value()?),
            "--out" => arguments.out = Some(PathBuf::from(value()?)),
            "--work-dir" => arguments.work_dir = Some(PathBuf::from(value()?)),
            "--commit" => arguments.commit = Some(value()?),
            "--verify-only" => arguments.verify_only = Some(PathBuf::from(value()?)),
            "--help" | "-h" => return Err(""),
            _ => return Err("opzione sconosciuta"),
        }
    }
    if arguments.phase.is_none() && arguments.verify_only.is_none() {
        return Err("serve --phase oppure --verify-only");
    }
    Ok(arguments)
}

fn commit(explicit: Option<String>) -> String {
    if let Some(commit) = explicit {
        return commit;
    }
    for variable in ["CAMPAIGN_COMMIT", "GITHUB_SHA"] {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return value;
        }
    }
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "non determinato".to_owned())
}

fn environment() -> Environment {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "non misurato".to_owned());
    let declared = |name: &str, fallback: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_owned())
    };
    let host = if std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true") {
        "github-actions".to_owned()
    } else {
        declared("CAMPAIGN_HOST", "non dichiarato")
    };
    Environment {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        cpus: std::thread::available_parallelism()
            .map(|count| count.get() as u64)
            .unwrap_or(0),
        kernel,
        toolchain: declared("CAMPAIGN_TOOLCHAIN", "non dichiarata"),
        build_profile: if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        },
        host,
    }
}

fn write_outputs(report: &Report, prefix: &Path) -> Result<(PathBuf, PathBuf), CampaignError> {
    if let Some(parent) = prefix.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|_| CampaignError::new("directory del report non creabile"))?;
    }
    let json_path = prefix.with_extension("json");
    let markdown_path = prefix.with_extension("md");
    let json = serde_json::to_string_pretty(report)
        .map_err(|_| CampaignError::new("report non serializzabile"))?;
    std::fs::write(&json_path, json + "\n")
        .map_err(|_| CampaignError::new("scrittura del report JSON non riuscita"))?;
    std::fs::write(&markdown_path, report::markdown(report))
        .map_err(|_| CampaignError::new("scrittura del riassunto Markdown non riuscita"))?;
    Ok((json_path, markdown_path))
}

fn finish(report: &Report, prefix: &Path) -> ExitCode {
    match write_outputs(report, prefix) {
        Ok((json, markdown)) => {
            eprintln!(
                "campagna {}{}: {} — report {} e {}",
                report.phase,
                if report.quick { " (quick)" } else { "" },
                if report.verdict.passed {
                    "SUPERATA"
                } else {
                    "FALLITA"
                },
                json.display(),
                markdown.display()
            );
            for criterion in &report.verdict.criteria {
                if criterion.status == report::Status::Fail {
                    eprintln!(
                        "  fallito: {} (osservato {}, limite {})",
                        criterion.description, criterion.observed, criterion.limit
                    );
                }
            }
            if report.verdict.passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("errore: {error}");
            ExitCode::from(3)
        }
    }
}

fn main() -> ExitCode {
    let arguments = match parse_arguments() {
        Ok(arguments) => arguments,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("errore: {message}");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let limits = match config::load_limits(&arguments.limits) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("errore: {error}");
            return ExitCode::from(2);
        }
    };

    if let Some(path) = &arguments.verify_only {
        let report: Report = match std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
        {
            Some(report) => report,
            None => {
                eprintln!("errore: report non leggibile o non conforme");
                return ExitCode::from(2);
            }
        };
        let mut report = report;
        report.verdict = verify::verify(&report, &limits);
        report.limits = limits;
        let prefix = arguments.out.clone().unwrap_or_else(|| {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            path.with_file_name(format!("{stem}-verificato"))
        });
        return finish(&report, &prefix);
    }

    let Some(phase) = arguments.phase else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let profiles = match config::load_profiles(&arguments.profiles) {
        Ok(profiles) => profiles,
        Err(error) => {
            eprintln!("errore: {error}");
            return ExitCode::from(2);
        }
    };
    let mut profile = profiles.phase(phase).clone();
    if arguments.quick {
        profile = profile.quick(&profiles.quick);
    }
    if let Some(minutes) = arguments.duration_min {
        let Some(seconds) = minutes.checked_mul(60) else {
            eprintln!("errore: durata fuori intervallo");
            return ExitCode::from(2);
        };
        profile.duration_s = seconds;
    }
    let plan = RunPlan {
        phase,
        quick: arguments.quick,
        seed: arguments.seed,
        profile,
        limits,
        work_dir: arguments
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("plenora-rest-campaign")),
        commit: commit(arguments.commit.clone()),
        environment: environment(),
    };
    let prefix = arguments
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("campaign-out").join(phase.as_str()));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("errore: runtime asincrono non avviabile");
            return ExitCode::from(3);
        }
    };
    match runtime.block_on(driver::run(plan)) {
        Ok(report) => finish(&report, &prefix),
        Err(error) => {
            eprintln!("errore: {error}");
            ExitCode::from(3)
        }
    }
}
