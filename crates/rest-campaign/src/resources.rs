//! Campionamento delle risorse del processo e della directory di lavoro.
//!
//! RSS, file descriptor e thread si leggono da `/proc/self` soltanto su Linux.
//! Altrove i campi restano `None` e il report li dichiara non misurati: un
//! valore inventato renderebbe verde un criterio che nessuno ha osservato.
//! Su Linux una lettura fallita è un errore di misura, contato nel report e
//! trattato dal verificatore come criterio fallito.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::CampaignError;

/// Un campione delle risorse.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Sample {
    /// Millisecondi dall'inizio della campagna.
    pub t_ms: u64,
    pub rss_bytes: Option<u64>,
    pub open_fds: Option<u64>,
    pub threads: Option<u64>,
    /// File regolari presenti nella directory dei trasferimenti.
    pub temp_files: u64,
    pub temp_bytes: u64,
    /// File parziali del motore (`*.part`) presenti nella stessa directory.
    pub partial_files: u64,
    /// Operazioni completate fino a questo istante.
    pub ops_completed: u64,
}

/// Risorse del processo lette dal sistema operativo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessResources {
    pub rss_bytes: Option<u64>,
    pub open_fds: Option<u64>,
    pub threads: Option<u64>,
}

/// Vero quando questa piattaforma misura RSS, descriptor e thread.
pub const fn process_resources_measured() -> bool {
    cfg!(target_os = "linux")
}

/// Legge le risorse del processo. Fuori da Linux restituisce campi `None`.
pub fn process_resources() -> Result<ProcessResources, CampaignError> {
    if !process_resources_measured() {
        return Ok(ProcessResources::default());
    }
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|_| CampaignError::new("lettura di /proc/self/status non riuscita"))?;
    let rss_kib = status_field(&status, "VmRSS:")
        .ok_or(CampaignError::new("VmRSS assente in /proc/self/status"))?;
    let threads = status_field(&status, "Threads:")
        .ok_or(CampaignError::new("Threads assente in /proc/self/status"))?;
    let open_fds = fs::read_dir("/proc/self/fd")
        .map_err(|_| CampaignError::new("lettura di /proc/self/fd non riuscita"))?
        .count();
    Ok(ProcessResources {
        rss_bytes: Some(
            rss_kib
                .checked_mul(1_024)
                .ok_or(CampaignError::new("VmRSS fuori intervallo"))?,
        ),
        // La directory aperta da read_dir compare nell'elenco: non è del
        // processo misurato, quindi non si conta.
        open_fds: Some(u64::try_from(open_fds.saturating_sub(1)).unwrap_or(u64::MAX)),
        threads: Some(threads),
    })
}

/// Valore intero di un campo `Nome:\t  123 kB` di `/proc/self/status`.
fn status_field(status: &str, name: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// Contenuto di una directory di trasferimento.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectoryUsage {
    pub files: u64,
    pub bytes: u64,
    pub partial_files: u64,
}

/// Conta ricorsivamente file, byte e file parziali del motore sotto `root`.
pub fn directory_usage(root: &Path) -> Result<DirectoryUsage, CampaignError> {
    let mut usage = DirectoryUsage::default();
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            // Una sottodirectory rimossa tra l'elenco e la lettura non è un
            // errore di misura.
            Err(error) if error.kind() == io::ErrorKind::NotFound && directory != root => continue,
            Err(_) => {
                return Err(CampaignError::new(
                    "lettura della directory di lavoro non riuscita",
                ));
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                usage.files += 1;
                if let Ok(metadata) = entry.metadata() {
                    usage.bytes = usage.bytes.saturating_add(metadata.len());
                }
                if entry.file_name().to_string_lossy().ends_with(".part") {
                    usage.partial_files += 1;
                }
            }
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::{directory_usage, status_field};

    #[test]
    fn status_fields_are_parsed_without_units() {
        let status = "Name:\tcampaign\nVmRSS:\t   20480 kB\nThreads:\t7\n";
        assert_eq!(status_field(status, "VmRSS:"), Some(20_480));
        assert_eq!(status_field(status, "Threads:"), Some(7));
        assert_eq!(status_field(status, "VmSwap:"), None);
    }

    #[test]
    fn partial_files_are_counted_apart() {
        let root = std::env::temp_dir().join(format!(
            "plenora-rest-campaign-usage-{}",
            std::process::id()
        ));
        let nested = root.join("dl");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.bin"), b"1234").unwrap();
        std::fs::write(nested.join(".a.bin.rest-engine-1-1.part"), b"12").unwrap();
        let usage = directory_usage(&root).unwrap();
        assert_eq!(usage.files, 2);
        assert_eq!(usage.bytes, 6);
        assert_eq!(usage.partial_files, 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
