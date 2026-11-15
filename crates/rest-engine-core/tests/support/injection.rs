//! Test-only ports. Production builds use Tokio directly and contain no fault state.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

macro_rules! points {
    ($($name:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Point { $($name),+ }
        impl Point {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$name),+];
        }
    }
}
points!(
    Prepare,
    OAuth,
    Polling,
    Cache,
    Circuit,
    Cookie,
    Open,
    Write,
    Flush,
    Finalize,
    Publish,
    Rename,
    Cleanup,
    CacheHit,
    CacheRevalidate,
    CircuitReject,
    CircuitProbe,
    OAuthRead,
    OAuthCached
);

pub(crate) enum Action {
    Cancel(crate::CancellationToken),
    Deadline,
}

pub(crate) struct Probe {
    pub(crate) point: Option<Point>,
    pub(crate) fired: AtomicBool,
    pub(crate) waits: mpsc::UnboundedSender<u64>,
    pub(crate) action: Option<Action>,
}
tokio::task_local! { pub(crate) static PROBE: Arc<Probe>; }

pub(crate) fn fault(point: Point) -> std::io::Result<()> {
    let first = visit(point);
    let fail = first
        && PROBE
            .try_with(|probe| probe.action.is_none())
            .unwrap_or(false);
    if fail {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "PRIVATE_IO_SENTINEL",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn visit(point: Point) -> bool {
    PROBE
        .try_with(|probe| {
            if probe.point != Some(point) || probe.fired.swap(true, Ordering::SeqCst) {
                return false;
            }
            match &probe.action {
                Some(Action::Cancel(token)) => token.cancel(),
                Some(Action::Deadline) => {
                    // Advance the test clock without inserting a yield into a
                    // synchronous region of the engine. The outer control observes
                    // expiry at the next genuine pending await, as in production.
                    use std::future::Future;
                    let mut advance = Box::pin(tokio::time::advance(Duration::from_secs(61)));
                    let _ = advance
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
                }
                None => {}
            }
            true
        })
        .unwrap_or(false)
}

pub(crate) async fn sleep(delay: Duration) {
    let _ = PROBE.try_with(|probe| {
        let _ = probe.waits.send(u64::try_from(delay.as_millis()).unwrap());
    });
    tokio::time::sleep(delay).await;
}
