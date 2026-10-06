//! Campagna operativa del motore REST: smoke, carico con iniezione di guasti
//! e soak, eseguiti contro un server HTTP locale in-process.
//!
//! La campagna usa soltanto l'API pubblica di `plenora-rest-core` (`Engine`,
//! `RuntimeBinding` e i tipi del contratto). Ogni scenario dichiara l'esito
//! atteso e gli invarianti da controllare; il verificatore ([`verify`]) legge
//! soltanto il report e le soglie di `campaign/limits.json`, quindi un report
//! con una violazione non può superare il gate per costruzione.
//!
//! Le misure di risorse (RSS, file descriptor, thread) vengono lette da
//! `/proc/self` su Linux; sulle altre piattaforme il report le dichiara «non
//! misurate» e i criteri relativi risultano non valutati, mai superati.

#![forbid(unsafe_code)]

pub mod config;
pub mod driver;
pub mod http;
pub mod report;
pub mod resources;
pub mod rng;
pub mod scenarios;
pub mod server;
pub mod stats;
pub mod timefmt;
pub mod verify;

use std::fmt;

/// Errore della campagna stessa (configurazione, I/O dell'harness), distinto
/// dai difetti del motore, che finiscono nel report come violazioni.
///
/// Il messaggio è sempre un testo dell'harness: non contiene valori letti da
/// file, risposte o percorsi.
#[derive(Debug)]
pub struct CampaignError {
    message: &'static str,
}

impl CampaignError {
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }

    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for CampaignError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for CampaignError {}
