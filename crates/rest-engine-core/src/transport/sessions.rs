//! Bounded cookie sessions, generations and jar leases.

use super::{
    Arc, AtomicUsize, CookiePolicy, CookieSession, CookieStore, EngineError, ErrorDetail,
    HeaderValue, Jar, MAX_SET_COOKIE_BYTES, Ordering, PreparedRequest, Transport, Url,
};

/// Fixed set of cookie session slots.
///
/// Memory is bounded by the slot count alone: nothing is remembered about
/// sessions that ended. A handle stays recognisable as stale because the slot
/// it names has moved to a later generation, not because the engine keeps a
/// list of what it evicted.
pub(super) struct SessionRegistry {
    /// Random per engine, so a handle issued by another engine is refused.
    /// `None` when the system random source failed at construction: every
    /// session operation is then refused rather than run with a guessable id.
    pub(super) engine: Option<u64>,
    pub(super) capacity: usize,
    pub(super) slots: Vec<SessionSlot>,
}

pub(super) struct SessionSlot {
    /// Generation a handle must carry to use this slot. It moves forward every
    /// time a session in the slot ends, so no handle outlives its session.
    pub(super) generation: u64,
    pub(super) state: SlotState,
}

pub(super) enum SlotState {
    Free,
    Open(OpenSession),
    /// Closed while operations still held it. The handle is already refused
    /// for new requests; the slot is freed, and its generation advanced, only
    /// once the last of those operations has released its lease.
    Closing(CookieJar),
    /// The generation counter is exhausted. The slot is never used again rather
    /// than restarting from a generation an old handle might still carry.
    Retired,
}

pub(super) struct OpenSession {
    /// Random per session, 128 bits: a handle cannot be assembled from a slot
    /// number and a guessed generation.
    pub(super) nonce: u128,
    pub(super) jar: CookieJar,
    /// Order of last use, for evicting the least recently used session.
    pub(super) last_used: u64,
}

/// Why a cookie session handle was not honoured.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SessionRefusal {
    /// Issued by another engine.
    ForeignEngine,
    /// Closed, evicted, or never issued by this engine.
    Stale,
}

/// 128 random bits from the operating system's generator.
fn random_u128() -> Result<u128, EngineError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| random_source_error())?;
    Ok(u128::from_le_bytes(bytes))
}

fn random_source_error() -> EngineError {
    EngineError::Runtime(ErrorDetail::from("the system random source failed"))
}

impl SessionRegistry {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            engine: getrandom::u64().ok(),
            capacity,
            slots: Vec::new(),
        }
    }

    /// The open session `handle` names, if the handle is current.
    pub(super) fn resolve(
        &mut self,
        handle: &CookieSession,
    ) -> Result<&mut OpenSession, SessionRefusal> {
        if Some(handle.engine) != self.engine {
            return Err(SessionRefusal::ForeignEngine);
        }
        let slot = usize::try_from(handle.slot)
            .ok()
            .and_then(|index| self.slots.get_mut(index))
            .ok_or(SessionRefusal::Stale)?;
        match &mut slot.state {
            SlotState::Open(session)
                if slot.generation == handle.generation && session.nonce == handle.nonce =>
            {
                Ok(session)
            }
            _ => Err(SessionRefusal::Stale),
        }
    }

    /// Ends the session in `index`, moving the slot to its next generation or
    /// retiring it when the generation cannot advance.
    pub(super) fn end(&mut self, index: usize) -> Result<Option<u64>, EngineError> {
        let slot = self.slots.get_mut(index).ok_or_else(slot_out_of_range)?;
        let incarnation = match std::mem::replace(&mut slot.state, SlotState::Free) {
            SlotState::Open(session) => Some(session.jar.incarnation),
            SlotState::Closing(jar) => Some(jar.incarnation),
            SlotState::Free | SlotState::Retired => None,
        };
        match slot.generation.checked_add(1) {
            Some(next) => slot.generation = next,
            None => slot.state = SlotState::Retired,
        }
        Ok(incarnation)
    }
}

/// An index into the session slots that the registry did not produce.
///
/// Slot indexes come from the registry's own scan or from a handle that
/// `resolve` has already matched to a slot, so this is an internal invariant
/// failure, reported as such rather than as a panic.
fn slot_out_of_range() -> EngineError {
    EngineError::Runtime(ErrorDetail::from(
        "cookie session slot index is out of range",
    ))
}
/// Cookie store that drops implausibly long `Set-Cookie` headers.
///
/// A resource bound, not a vulnerability mitigation: a remote service should not
/// be able to push arbitrarily large values into engine-held state one header at
/// a time. A legitimate `Set-Cookie` is far below this limit.
#[derive(Default)]
pub(crate) struct BoundedJar {
    pub(super) inner: Jar,
}

impl CookieStore for BoundedJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let accepted = cookie_headers
            .filter(|value| value.len() <= MAX_SET_COOKIE_BYTES)
            .cloned()
            .collect::<Vec<_>>();
        if accepted.is_empty() {
            return;
        }
        self.inner.set_cookies(&mut accepted.iter(), url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        // The inner jar keeps its cookies in hash maps, so the order of the
        // pairs in the Cookie header changed from one jar to the next for the
        // same cookies. Sorted, the header depends only on the cookies. A
        // pair never contains `;`: the cookie-octet grammar excludes it.
        let value = self.inner.cookies(url)?;
        let mut pairs = value
            .as_bytes()
            .split(|byte| *byte == b';')
            .map(<[u8]>::trim_ascii)
            .filter(|pair| !pair.is_empty())
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        Some(HeaderValue::from_bytes(&pairs.join(&b"; "[..])).unwrap_or(value))
    }
}

#[derive(Clone)]
pub(crate) struct CookieJar {
    pub(super) jar: Arc<BoundedJar>,
    /// Operations currently holding this jar, counted by this crate rather than
    /// inferred from `Arc::strong_count`.
    ///
    /// How many references a pooled HTTP client keeps to its cookie store is an
    /// implementation detail of that client; counting reservations here is
    /// independent of it, and a pooled client alone never makes a jar look busy.
    pub(super) leases: Arc<AtomicUsize>,
    /// Identifies this jar instance. Never reused, so a pooled client built for
    /// an earlier session in the same slot is never handed to a later one.
    pub(super) incarnation: u64,
}

/// Keeps a jar reserved for as long as an operation needs it.
///
/// Taken while the registry lock is held, so a jar cannot be evicted between
/// the lookup that found it and the reservation that protects it. Released on
/// drop, including when the operation fails or is cancelled.
pub(super) struct JarLease {
    pub(super) leases: Arc<AtomicUsize>,
}

impl Drop for JarLease {
    fn drop(&mut self) {
        // Released without the registry lock, so the atomic itself has to carry
        // the ordering that lets a later eviction see the count reach zero.
        self.leases.fetch_sub(1, Ordering::Release);
    }
}

impl Transport {
    /// Opens a cookie session and returns its handle.
    ///
    /// Uses a free slot when there is one. Otherwise the least recently used
    /// session that no operation is holding is evicted: its slot moves to the
    /// next generation, so the evicted handle fails from then on instead of
    /// reaching the new, empty session. When every session is held by a
    /// running operation the call is refused.
    pub async fn open_cookie_session(&self) -> Result<CookieSession, EngineError> {
        if !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        let incarnation = self.next_sequence();
        let last_used = self.next_sequence();
        let nonce = random_u128()?;
        let mut registry = self.sessions.lock().await;
        let engine = registry.engine.ok_or_else(random_source_error)?;
        let index = loop {
            if let Some(index) = registry
                .slots
                .iter()
                .position(|slot| matches!(slot.state, SlotState::Free))
            {
                break index;
            }
            // A closed session whose last operation has finished: its slot can
            // move to the next generation now. Leases are released without this
            // lock, so the count is read here rather than acted on at release.
            if let Some(index) = registry.slots.iter().position(|slot| {
                matches!(&slot.state, SlotState::Closing(jar) if jar.leases.load(Ordering::Acquire) == 0)
            }) {
                if let Some(incarnation) = registry.end(index)? {
                    self.drop_session_clients(incarnation).await;
                }
                continue;
            }
            if registry.slots.len() < registry.capacity {
                registry.slots.push(SessionSlot {
                    generation: 0,
                    state: SlotState::Free,
                });
                break registry.slots.len() - 1;
            }
            // Only a session no operation holds may go: evicting one in use
            // would split it between the running request and the next one.
            // Leases are taken and read under this lock, so a zero count here
            // cannot be stale.
            let victim = registry
                .slots
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| match &slot.state {
                    SlotState::Open(session) if session.jar.leases.load(Ordering::Acquire) == 0 => {
                        Some((index, session.last_used))
                    }
                    _ => None,
                })
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(index, _)| index);
            let Some(victim) = victim else {
                return Err(EngineError::PolicyViolation(ErrorDetail::from(
                    "every cookie session is held by a running operation",
                )));
            };
            if let Some(incarnation) = registry.end(victim)? {
                self.drop_session_clients(incarnation).await;
            }
            // A retired slot is skipped by the next iteration; the loop ends
            // because every iteration either returns, or frees or retires one
            // of finitely many slots.
        };
        let slot = registry
            .slots
            .get_mut(index)
            .ok_or_else(slot_out_of_range)?;
        slot.state = SlotState::Open(OpenSession {
            nonce,
            jar: CookieJar {
                jar: Arc::new(BoundedJar::default()),
                leases: Arc::new(AtomicUsize::new(0)),
                incarnation,
            },
            last_used,
        });
        Ok(CookieSession {
            engine,
            slot: u32::try_from(index).map_err(|_| {
                EngineError::Runtime(ErrorDetail::from("cookie session slot index overflowed"))
            })?,
            generation: slot.generation,
            nonce,
        })
    }

    /// Closes the session `handle` names.
    ///
    /// The handle, and every copy of it, is refused for new requests from now
    /// on. Operations already admitted keep the jar they were admitted with
    /// until they finish; the slot is freed and moves to its next generation
    /// only after the last of them has released it, so a closed session is
    /// never handed to a new one while still in use. Closing a handle that is
    /// already stale is an error rather than a no-op, so a caller that lost
    /// track of a session finds out.
    pub async fn close_cookie_session(&self, handle: &CookieSession) -> Result<(), EngineError> {
        let mut registry = self.sessions.lock().await;
        let session = registry.resolve(handle).map_err(session_refusal_error)?;
        let held = session.jar.leases.load(Ordering::Acquire) > 0;
        let incarnation = session.jar.incarnation;
        let index = usize::try_from(handle.slot).map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("cookie session slot index overflowed"))
        })?;
        if held {
            let slot = registry
                .slots
                .get_mut(index)
                .ok_or_else(slot_out_of_range)?;
            slot.state = match std::mem::replace(&mut slot.state, SlotState::Free) {
                SlotState::Open(session) => SlotState::Closing(session.jar),
                other => other,
            };
            // Idle pooled clients go now; one built by an operation still in
            // flight is removed when the slot is reclaimed.
            self.drop_session_clients(incarnation).await;
        } else if let Some(incarnation) = registry.end(index)? {
            self.drop_session_clients(incarnation).await;
        }
        Ok(())
    }

    /// Removes the pooled clients built on a session that ended.
    ///
    /// Called with the session registry locked; locks are always taken
    /// sessions then clients, as in `client_for`, so the two cannot cross.
    async fn drop_session_clients(&self, incarnation: u64) {
        self.clients
            .lock()
            .await
            .retain(|key, _| key.cookie_jar != Some(incarnation));
    }

    /// Reserves the session this request needs, before any step that can reach
    /// the network.
    ///
    /// Doing this up front is what keeps the error honest: refusing later, from
    /// `client_for`, would report a policy violation with no remote effect after
    /// an OAuth token had already been fetched. The returned lease keeps the
    /// session from being evicted while this operation authenticates and
    /// resolves DNS.
    ///
    /// The jar resolved here is the one the whole operation uses: it is stored
    /// in the request and `client_for` never resolves the handle again, so a
    /// session closed while the operation is in flight cannot turn into a
    /// refusal after network activity has already happened.
    pub(super) async fn admit_cookie_jar(
        &self,
        request: &mut PreparedRequest,
    ) -> Result<Option<JarLease>, EngineError> {
        // A follow-up whose cookies were stripped still belongs to the caller's
        // session: it must not run once that session has ended.
        if request.cookies.session.is_none() {
            self.check_cookie_session(request.caller_session.as_ref())
                .await?;
        }
        Ok(match self.cookie_jar(&request.cookies).await? {
            Some((jar, lease)) => {
                request.admitted_jar = Some(jar);
                Some(lease)
            }
            None => None,
        })
    }

    /// The jar of the session `cookies` names, with a lease that keeps the
    /// session from being evicted while the caller holds it.
    ///
    /// `Ok(None)` when the request uses no session. A stale handle, or one from
    /// another engine, fails: the session it named is gone, and an empty one in
    /// its place would quietly log the caller out.
    pub(super) async fn cookie_jar(
        &self,
        cookies: &CookiePolicy,
    ) -> Result<Option<(CookieJar, JarLease)>, EngineError> {
        let Some(handle) = &cookies.session else {
            return Ok(None);
        };
        let last_used = self.next_sequence();
        let mut registry = self.sessions.lock().await;
        let session = registry.resolve(handle).map_err(session_refusal_error)?;
        session.last_used = last_used;
        let jar = session.jar.clone();
        jar.leases.fetch_add(1, Ordering::Acquire);
        let lease = JarLease {
            leases: jar.leases.clone(),
        };
        Ok(Some((jar, lease)))
    }

    /// Refuses a handle that is stale, foreign, or used on an engine without a
    /// cookie store. `None` is accepted: the request names no session.
    pub async fn check_cookie_session(
        &self,
        session: Option<&CookieSession>,
    ) -> Result<(), EngineError> {
        let Some(handle) = session else {
            return Ok(());
        };
        #[cfg(test)]
        crate::injection::visit(crate::injection::Point::Cookie);
        if !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        self.sessions
            .lock()
            .await
            .resolve(handle)
            .map(|_| ())
            .map_err(session_refusal_error)
    }
}

fn session_refusal_error(refusal: SessionRefusal) -> EngineError {
    EngineError::PolicyViolation(ErrorDetail::from(match refusal {
        SessionRefusal::ForeignEngine => "the cookie session handle was issued by another engine",
        SessionRefusal::Stale => "the cookie session was closed or evicted; open a new session",
    }))
}
