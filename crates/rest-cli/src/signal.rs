//! Ctrl-C come cancellazione cooperativa (CLI 2.0 §9).
//!
//! Su Unix SIGINT e SIGTERM, su Windows Ctrl-C e Ctrl-Break: il primo segnale
//! cancella il token dell'esecuzione, e il motore chiude l'operazione con un
//! errore `cancelled` (exit 130) che descrive comunque fase, effetto remoto e
//! retry. I segnali successivi sono assorbiti: la chiusura cooperativa, che
//! può chiedere la cancellazione remota di un job, non viene troncata a metà.
//!
//! La registrazione avviene in modo sincrono, prima di leggere la richiesta e
//! prima di qualunque rete: se fallisce, il comando si rifiuta invece di
//! partire senza poter essere cancellato.

use std::io;

use plenora_rest_core::CancellationToken;
use tokio::sync::watch;

/// Installa il gestore. Va chiamata dentro il runtime tokio.
#[cfg(unix)]
pub(crate) fn install(token: CancellationToken, cancelled: watch::Sender<bool>) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        let mut first = true;
        // Il ciclo tiene vivi i ricevitori: su Windows un Ctrl-C senza
        // ricevitori torna al comportamento predefinito e termina il processo.
        loop {
            tokio::select! {
                Some(()) = interrupt.recv() => {}
                Some(()) = terminate.recv() => {}
                else => return,
            }
            if first {
                cancel(&token, &cancelled);
                first = false;
            }
        }
    });
    Ok(())
}

/// Installa il gestore. Va chiamata dentro il runtime tokio.
#[cfg(windows)]
pub(crate) fn install(token: CancellationToken, cancelled: watch::Sender<bool>) -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};
    let mut interrupt = ctrl_c()?;
    let mut brk = ctrl_break()?;
    tokio::spawn(async move {
        let mut first = true;
        // Il ciclo tiene vivi i ricevitori: su Windows un Ctrl-C senza
        // ricevitori torna al comportamento predefinito e termina il processo.
        loop {
            tokio::select! {
                Some(()) = interrupt.recv() => {}
                Some(()) = brk.recv() => {}
                else => return,
            }
            if first {
                cancel(&token, &cancelled);
                first = false;
            }
        }
    });
    Ok(())
}

/// Su una piattaforma senza segnali supportati il comando si rifiuta.
#[cfg(not(any(unix, windows)))]
pub(crate) fn install(
    _token: CancellationToken,
    _cancelled: watch::Sender<bool>,
) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(any(unix, windows))]
fn cancel(token: &CancellationToken, cancelled: &watch::Sender<bool>) {
    token.cancel();
    // Nessun ricevitore vuol dire che la lettura è già finita: il token
    // basta al motore.
    let _ = cancelled.send(true);
}
