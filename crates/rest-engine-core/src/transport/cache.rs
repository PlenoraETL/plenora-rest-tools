//! HTTP cache identity, storage and revalidation.

use super::{
    AuthConfig, BTreeMap, CACHE_CONTROL, CONTENT_LENGTH, Digest, ETAG, EngineError, ErrorDetail,
    ErrorPhase, Fingerprint, Hash, HashMap, Hasher, HttpMethod, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    Instant, LAST_MODIFIED, PreparedBody, PreparedFileSource, PreparedRequest, ResponseData,
    Sha256, Transport, Url, VARY, enter_phase,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct CacheKey {
    pub(super) method: String,
    pub(super) url: String,
    pub(super) request_fingerprint: Fingerprint,
}

#[derive(Clone)]
pub(super) struct CachedResponse {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
    pub(super) final_url: Url,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) validated_at: Instant,
    pub(super) last_access: Instant,
    pub(super) must_revalidate: bool,
    pub(super) server_fresh_for_ms: Option<u64>,
    pub(super) size_bytes: usize,
    pub(super) sequence: u64,
}

#[derive(Default)]
pub(super) struct CacheStore {
    pub(super) entries: HashMap<CacheKey, CachedResponse>,
    pub(super) size_bytes: usize,
}

impl Transport {
    pub(super) async fn execute_cached(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        #[cfg(test)]
        if request.cache.enabled {
            crate::injection::visit(crate::injection::Point::Cache);
        }
        if !request.cache.enabled {
            return self.execute_with_circuit(request).await;
        }
        if !matches!(request.method, HttpMethod::Get | HttpMethod::Head) {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "HTTP cache is supported only for GET and HEAD",
            )));
        }
        if self.config.max_cache_entries == 0 || self.config.max_cache_bytes == 0 {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "HTTP cache capacity is disabled by this engine",
            )));
        }
        // Defensive: `validate_request` already refused this combination before
        // anything reached the network.
        if request.cookies.is_enabled() {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "the HTTP cache cannot be combined with the cookie store",
            )));
        }
        // A client certificate authenticates the request just as much as a
        // bearer token does, so caching it needs the same explicit opt-in.
        let authenticated = !matches!(request.auth, AuthConfig::None)
            || request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
            || request.cookies.is_enabled()
            || request.tls.client_identity_pem.is_some();
        if authenticated && !request.cache.allow_authenticated {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "authenticated HTTP caching requires allow_authenticated",
            )));
        }

        let key = cache_key(request)?;
        let cached = {
            let mut store = self.cache.lock().await;
            match store.entries.get_mut(&key) {
                Some(cached) => {
                    cached.last_access = Instant::now();
                    Some(cached.clone())
                }
                None => None,
            }
        };
        if let Some(cached) = &cached {
            let fresh_for_ms = cached
                .server_fresh_for_ms
                .map_or(request.cache.fresh_for_ms, |server| {
                    request.cache.fresh_for_ms.min(server)
                });
            let fresh = !cached.must_revalidate
                && fresh_for_ms > 0
                && cached.validated_at.elapsed() <= crate::temporal::millis(fresh_for_ms);
            if fresh {
                #[cfg(test)]
                crate::injection::visit(crate::injection::Point::CacheHit);
                // Interpreted from here on like a response that was read; it
                // is not a request sent, so the tally does not count it.
                enter_phase(ErrorPhase::Read);
                return Ok(cached_response(cached, 1, 0));
            }
        }

        let mut conditional = request.clone();
        if let Some(cached) = &cached {
            add_conditional_headers(&mut conditional.headers, &cached.headers);
        }
        let response = self.execute_with_circuit(&conditional).await?;
        if response.status == 304 {
            let Some(mut cached) = cached else {
                return Ok(response);
            };
            #[cfg(test)]
            crate::injection::visit(crate::injection::Point::CacheRevalidate);
            merge_headers(&mut cached.headers, &response.headers);
            cached.validated_at = Instant::now();
            cached.last_access = cached.validated_at;
            cached.must_revalidate = response_requires_revalidation(&cached.headers);
            cached.server_fresh_for_ms = cache_max_age_ms(&cached.headers);
            cached.size_bytes = cached_response_size(&cached.body, &cached.headers);
            self.store_cache_entry(key, cached.clone()).await;
            let mut resolved = cached_response(&cached, 1, 1);
            resolved.attempts = response.attempts;
            resolved.network_requests = response.network_requests;
            resolved.rate_limit_wait_ms = response.rate_limit_wait_ms;
            return Ok(resolved);
        }

        if (200..300).contains(&response.status)
            && !response_forbids_store(&response.headers)
            && response.body.len() <= self.config.max_cache_bytes
        {
            let now = Instant::now();
            let cached = CachedResponse {
                status: response.status,
                body: response.body.clone(),
                final_url: response.final_url.clone(),
                headers: response.headers.clone(),
                validated_at: now,
                last_access: now,
                must_revalidate: response_requires_revalidation(&response.headers),
                server_fresh_for_ms: cache_max_age_ms(&response.headers),
                size_bytes: cached_response_size(&response.body, &response.headers),
                sequence: self.next_sequence(),
            };
            self.store_cache_entry(key, cached).await;
        }
        Ok(response)
    }

    async fn store_cache_entry(&self, key: CacheKey, entry: CachedResponse) {
        if entry.size_bytes > self.config.max_cache_bytes {
            return;
        }
        let mut store = self.cache.lock().await;
        if let Some(previous) = store.entries.remove(&key) {
            store.size_bytes = store.size_bytes.saturating_sub(previous.size_bytes);
        }
        while store.entries.len() >= self.config.max_cache_entries
            || store.size_bytes.saturating_add(entry.size_bytes) > self.config.max_cache_bytes
        {
            let Some(oldest) = store
                .entries
                .iter()
                .min_by_key(|(_, value)| (value.last_access, value.sequence))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(removed) = store.entries.remove(&oldest) {
                store.size_bytes = store.size_bytes.saturating_sub(removed.size_bytes);
            }
        }
        if store.entries.len() < self.config.max_cache_entries
            && store.size_bytes.saturating_add(entry.size_bytes) <= self.config.max_cache_bytes
        {
            store.size_bytes = store.size_bytes.saturating_add(entry.size_bytes);
            store.entries.insert(key, entry);
        }
    }
}

/// Feeds `Hash` output into SHA-256 so isolation keys keep a collision resistant
/// fingerprint instead of a 64 bit `DefaultHasher` value.
#[derive(Default)]
pub(super) struct DigestHasher {
    pub(super) digest: Sha256,
}

impl DigestHasher {
    fn fingerprint(self) -> Fingerprint {
        self.digest.finalize().into()
    }
}

impl Hasher for DigestHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
    }

    fn finish(&self) -> u64 {
        self.digest
            .clone()
            .finalize()
            .iter()
            .take(8)
            .fold(0_u64, |accumulator, byte| {
                (accumulator << 8) | u64::from(*byte)
            })
    }
}

pub(super) fn fingerprint(value: &(impl Hash + ?Sized)) -> Fingerprint {
    let mut hasher = DigestHasher::default();
    value.hash(&mut hasher);
    hasher.fingerprint()
}

fn cache_key(request: &PreparedRequest) -> Result<CacheKey, EngineError> {
    let mut hasher = DigestHasher::default();
    request.headers.hash(&mut hasher);
    request.auth.hash(&mut hasher);
    request.cookies.hash(&mut hasher);
    // The transport identity is part of who is asking, not just of how the
    // connection is made: two requests that differ only by client certificate,
    // trust anchor, or proxy are different requests and must not share an entry.
    // The connection pool already isolates them; the cache is consulted before
    // the pool, so it needs the same isolation of its own.
    request.tls.hash(&mut hasher);
    request.proxy.hash(&mut hasher);
    // The redirect policy decides which resource actually answered: a request
    // that followed a redirect must not serve its answer to one that forbids
    // them.
    request.allow_redirects.hash(&mut hasher);
    request.max_redirects.hash(&mut hasher);
    match &request.body {
        PreparedBody::None => 0_u8.hash(&mut hasher),
        PreparedBody::Json(value) => {
            1_u8.hash(&mut hasher);
            serde_json::to_vec(value)
                .map_err(|_| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "JSON body could not be serialized for the cache key",
                    ))
                })?
                .hash(&mut hasher);
        }
        PreparedBody::Form(values) => {
            2_u8.hash(&mut hasher);
            values.hash(&mut hasher);
        }
        PreparedBody::Multipart { fields, files } => {
            3_u8.hash(&mut hasher);
            fields.hash(&mut hasher);
            for file in files {
                file.field_name.hash(&mut hasher);
                file.filename.hash(&mut hasher);
                file.content_type.hash(&mut hasher);
                match &file.source {
                    PreparedFileSource::Bytes(data) => data.hash(&mut hasher),
                    PreparedFileSource::Path { .. } => {
                        return Err(EngineError::InvalidInput(ErrorDetail::from(
                            "HTTP cache cannot fingerprint streaming multipart files",
                        )));
                    }
                }
            }
        }
        PreparedBody::Raw(value) => {
            4_u8.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        PreparedBody::Stream(_) => {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "HTTP cache cannot fingerprint streaming request bodies",
            )));
        }
    }
    Ok(CacheKey {
        method: request.method.as_str().to_owned(),
        url: request.url.as_str().to_owned(),
        request_fingerprint: hasher.fingerprint(),
    })
}

fn cached_response(
    cached: &CachedResponse,
    cache_hits: u64,
    cache_revalidations: u64,
) -> ResponseData {
    ResponseData {
        status: cached.status,
        body: cached.body.clone(),
        final_url: cached.final_url.clone(),
        // The public contract requires `attempts >= 1`; a cache hit still
        // resolves one request attempt. `network_requests` stays zero because
        // that is the metric describing actual network traffic.
        attempts: 1,
        network_requests: 0,
        auth_requests: 0,
        auth_retries: 0,
        rate_limit_wait_ms: 0,
        cache_hits,
        cache_revalidations,
        headers: cached.headers.clone(),
        retry_after_ms: None,
    }
}

fn add_conditional_headers(
    request_headers: &mut BTreeMap<String, String>,
    cached_headers: &BTreeMap<String, String>,
) {
    if !contains_header(request_headers, IF_NONE_MATCH.as_str())
        && let Some(etag) = cached_headers.get(ETAG.as_str())
    {
        request_headers.insert(IF_NONE_MATCH.as_str().to_owned(), etag.clone());
    }
    if !contains_header(request_headers, IF_MODIFIED_SINCE.as_str())
        && let Some(last_modified) = cached_headers.get(LAST_MODIFIED.as_str())
    {
        request_headers.insert(IF_MODIFIED_SINCE.as_str().to_owned(), last_modified.clone());
    }
}

fn contains_header(headers: &BTreeMap<String, String>, name: &str) -> bool {
    headers
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
}

fn merge_headers(cached: &mut BTreeMap<String, String>, revalidated: &BTreeMap<String, String>) {
    for (name, value) in revalidated {
        if !name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str()) {
            cached.insert(name.clone(), value.clone());
        }
    }
}

pub(crate) fn response_forbids_store(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-store")
        || headers
            .get(VARY.as_str())
            .is_some_and(|value| value.split(',').any(|value| value.trim() == "*"))
}

pub(crate) fn response_requires_revalidation(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-cache")
}

pub(crate) fn cache_max_age_ms(headers: &BTreeMap<String, String>) -> Option<u64> {
    headers
        .get(CACHE_CONTROL.as_str())?
        .split(',')
        .find_map(|directive| {
            let (name, value) = directive.trim().split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("max-age")
                .then(|| crate::temporal::remote_seconds_ms(value.trim().trim_matches('"')))
                .flatten()
        })
}

fn header_has_directive(headers: &BTreeMap<String, String>, header: &str, directive: &str) -> bool {
    headers.get(header).is_some_and(|value| {
        value
            .split(',')
            .filter_map(|value| {
                value
                    .trim()
                    .split_once('=')
                    .map_or_else(|| Some(value.trim()), |(name, _)| Some(name.trim()))
            })
            .any(|value| value.eq_ignore_ascii_case(directive))
    })
}

fn cached_response_size(body: &[u8], headers: &BTreeMap<String, String>) -> usize {
    headers.iter().fold(body.len(), |size, (name, value)| {
        size.saturating_add(name.len()).saturating_add(value.len())
    })
}
