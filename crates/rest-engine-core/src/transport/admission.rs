//! Admit wire requests, headers, URLs and resolved endpoints.

use super::{
    ApiKeyLocation, AuthConfig, BTreeMap, CONTENT_LENGTH, CONTENT_TYPE, Client, ClientKey,
    Duration, EngineError, ErrorDetail, HeaderMap, HeaderName, HeaderValue, Host, HttpMethod,
    IpAddr, Ipv4Addr, Ipv6Addr, Method, Policy, PooledClient, PreparedBody, PreparedRequest, Proxy,
    ProxyConfig, SocketAddr, TlsConfig, Transport, Url, fingerprint, lookup_host,
};
impl Transport {
    pub(super) fn validate_request(&self, request: &PreparedRequest) -> Result<(), EngineError> {
        if request.method.is_custom()
            && !self
                .config
                .allowed_custom_methods
                .iter()
                .any(|allowed| allowed == request.method.as_str())
        {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "custom HTTP method is not in allowed_custom_methods",
            )));
        }
        // Refused rather than approximated, and refused here so that a request
        // that cannot run produces no effect at all — not even an OAuth token
        // acquisition, which `execute` performs before consulting the cache.
        //
        // A cached entry belongs to the session that produced it, but the cookie
        // jar lives inside the HTTP client and changes while the operation runs:
        // a retry or a redirect can pick up a new session after the key was
        // computed, and cookies expire on their own with nothing to observe.
        // Keying the cache on a snapshot of the jar would therefore describe a
        // session that may not be the one that answered.
        if request.cookies.is_enabled() && request.cache.enabled {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "the HTTP cache cannot be combined with the cookie store",
            )));
        }
        if request.cookies.is_enabled() && !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        Ok(())
    }
}

impl Transport {
    pub(super) async fn client_for(
        &self,
        url: &Url,
        tls: &TlsConfig,
        proxy: Option<&ProxyConfig>,
        request: &PreparedRequest,
    ) -> Result<Client, EngineError> {
        if !tls.verify && !self.config.allow_insecure_tls {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "TLS verification cannot be disabled by this engine",
            )));
        }
        if proxy.is_some() && !self.config.allow_proxies {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "proxies are not enabled for this engine",
            )));
        }

        let (host, address, should_pin, port) = self.resolve_endpoint(url).await?;
        let proxy_endpoint = match proxy {
            Some(proxy) => {
                let url = Url::parse(&proxy.url).map_err(|_| {
                    EngineError::InvalidInput(ErrorDetail::from("invalid proxy URL"))
                })?;
                Some((proxy, url.clone(), self.resolve_endpoint(&url).await?))
            }
            None => None,
        };
        // The jar admitted with the operation, never a fresh resolution of the
        // handle: the operation's lease keeps it alive, and resolving again
        // here could refuse a session closed in the meantime after an OAuth
        // token had already been fetched.
        let jar = match (&request.cookies.session, &request.admitted_jar) {
            (None, _) => None,
            (Some(_), Some(jar)) => Some(jar),
            (Some(_), None) => {
                return Err(EngineError::Runtime(ErrorDetail::from(
                    "a cookie session was used without being admitted",
                )));
            }
        };
        let key = ClientKey {
            host: host.clone(),
            port,
            address,
            policy_fingerprint: fingerprint(&(tls, proxy)),
            cookie_jar: jar.map(|jar| jar.incarnation),
        };
        if let Some(pooled) = self.clients.lock().await.get(&key) {
            return Ok(pooled.client.clone());
        }

        let mut builder = Client::builder()
            .tls_backend_preconfigured(crate::tls::client_config(tls)?)
            .connect_timeout(Duration::from_millis(self.config.connect_timeout_ms))
            .pool_idle_timeout(Duration::from_millis(self.config.pool_idle_timeout_ms))
            .pool_max_idle_per_host(self.config.pool_max_idle_per_host)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(&self.config.user_agent);
        if let Some(jar) = jar {
            builder = builder.cookie_provider(jar.jar.clone());
        }
        if !self.config.automatic_decompression {
            builder = builder.no_brotli().no_deflate().no_gzip().no_zstd();
        }
        if should_pin {
            builder = builder.resolve(&host, SocketAddr::new(address, port));
        }
        if let Some((config, _, (proxy_host, proxy_address, proxy_pin, proxy_port))) =
            proxy_endpoint
        {
            let mut configured = Proxy::all(&config.url)
                .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("invalid proxy URL")))?;
            match (&config.username, &config.password) {
                (Some(username), password) => {
                    configured = configured.basic_auth(username, password.as_deref().unwrap_or(""));
                }
                (None, Some(_)) => {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "proxy password requires a username",
                    )));
                }
                (None, None) => {}
            }
            builder = builder.proxy(configured);
            if proxy_pin {
                builder = builder.resolve(&proxy_host, SocketAddr::new(proxy_address, proxy_port));
            }
        }
        let client = builder.build().map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("HTTP client could not be built"))
        })?;
        if self.config.max_pooled_origins > 0 {
            let sequence = self.next_sequence();
            let mut clients = self.clients.lock().await;
            if clients.len() >= self.config.max_pooled_origins {
                let oldest = clients
                    .iter()
                    .min_by_key(|(_, pooled)| pooled.sequence)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    clients.remove(&oldest);
                }
            }
            clients.insert(
                key,
                PooledClient {
                    client: client.clone(),
                    sequence,
                },
            );
        }
        Ok(client)
    }
}

impl Transport {
    async fn resolve_endpoint(
        &self,
        url: &Url,
    ) -> Result<(String, IpAddr, bool, u16), EngineError> {
        validate_url(url)?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| EngineError::InvalidUrl(ErrorDetail::from("URL has no usable port")))?;
        let (host, address, should_pin) = match url
            .host()
            .ok_or_else(|| EngineError::InvalidUrl(ErrorDetail::from("URL must include a host")))?
        {
            Host::Ipv4(address) => {
                let address = IpAddr::V4(address);
                self.validate_address(address)?;
                (address.to_string(), address, false)
            }
            Host::Ipv6(address) => {
                let address = IpAddr::V6(address);
                self.validate_address(address)?;
                (address.to_string(), address, false)
            }
            Host::Domain(domain) => {
                let addresses: Vec<IpAddr> = lookup_host((domain, port))
                    .await
                    .map_err(|_| {
                        EngineError::DnsResolution(ErrorDetail::from("DNS resolution failed"))
                    })?
                    .map(|socket| socket.ip())
                    .collect();
                let Some(first) = addresses.first().copied() else {
                    return Err(EngineError::DnsResolution(ErrorDetail::from(
                        "no addresses returned",
                    )));
                };
                for address in &addresses {
                    self.validate_address(*address)?;
                }
                (domain.to_owned(), first, true)
            }
        };
        Ok((host, address, should_pin, port))
    }

    fn validate_address(&self, address: IpAddr) -> Result<(), EngineError> {
        if self.config.allow_private_networks || is_public_address(address) {
            return Ok(());
        }
        Err(EngineError::UnsafeAddress(ErrorDetail::from(
            "the resolved address is private, loopback, or otherwise not public",
        )))
    }
}

fn validate_url(url: &Url) -> Result<(), EngineError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EngineError::InvalidUrl(ErrorDetail::from(
            "only http and https URLs are supported",
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EngineError::InvalidUrl(ErrorDetail::from(
            "credentials embedded in URLs are not allowed",
        )));
    }
    Ok(())
}

pub(super) fn set_request_header(
    headers: &mut BTreeMap<String, String>,
    name: &str,
    value: String,
) {
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
    headers.insert(name.to_owned(), value);
}

pub(super) fn request_headers(request: &PreparedRequest) -> Result<HeaderMap, EngineError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| EngineError::InvalidHeader(ErrorDetail::from("header name is invalid")))?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("header value is invalid"))
        })?;
        headers.insert(name, value);
    }

    if let AuthConfig::ApiKey {
        key_name,
        key_value,
        location: ApiKeyLocation::Header,
    } = &request.auth
    {
        let name = HeaderName::from_bytes(key_name.as_bytes()).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("API key header name is invalid"))
        })?;
        let value = HeaderValue::from_str(key_value).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("API key header value is invalid"))
        })?;
        headers.insert(name, value);
    }

    if matches!(request.body, PreparedBody::Multipart { .. }) && headers.contains_key(CONTENT_TYPE)
    {
        return Err(EngineError::InvalidHeader(ErrorDetail::from(
            "Content-Type must be generated by the multipart encoder",
        )));
    }
    if matches!(
        request.body,
        PreparedBody::Multipart { .. } | PreparedBody::Stream(_)
    ) && headers.contains_key(CONTENT_LENGTH)
    {
        return Err(EngineError::InvalidHeader(ErrorDetail::from(
            "Content-Length must be generated by the streaming encoder",
        )));
    }
    if !headers.contains_key(CONTENT_TYPE) {
        let content_type = match &request.body {
            PreparedBody::Json(_) => Some("application/json"),
            PreparedBody::Form(_) => Some("application/x-www-form-urlencoded"),
            PreparedBody::Multipart { .. } => None,
            PreparedBody::Stream(stream) => stream.content_type.as_deref(),
            PreparedBody::Raw(_) | PreparedBody::None => None,
        };
        if let Some(content_type) = content_type {
            let content_type = HeaderValue::from_str(content_type)
                .map_err(|_| EngineError::InvalidHeader(ErrorDetail::from("content-type")))?;
            headers.insert(CONTENT_TYPE, content_type);
        }
    }
    Ok(headers)
}

pub(super) fn apply_query_auth(url: &mut Url, auth: &AuthConfig) {
    if let AuthConfig::ApiKey {
        key_name,
        key_value,
        location: ApiKeyLocation::Query,
    } = auth
    {
        url.query_pairs_mut().append_pair(key_name, key_value);
    }
}

pub(super) fn method(method: &HttpMethod) -> Result<Method, EngineError> {
    match method {
        HttpMethod::Get => Ok(Method::GET),
        HttpMethod::Head => Ok(Method::HEAD),
        HttpMethod::Post => Ok(Method::POST),
        HttpMethod::Put => Ok(Method::PUT),
        HttpMethod::Patch => Ok(Method::PATCH),
        HttpMethod::Delete => Ok(Method::DELETE),
        HttpMethod::Options => Ok(Method::OPTIONS),
        HttpMethod::Custom(value) => Method::from_bytes(value.as_bytes()).map_err(|_| {
            EngineError::InvalidInput(ErrorDetail::from("invalid custom HTTP method"))
        }),
    }
}

pub(super) fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        || octets[0] >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}
