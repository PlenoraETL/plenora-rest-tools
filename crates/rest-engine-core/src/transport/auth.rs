//! Resolve authentication and manage OAuth token acquisition.

type TokenRequestParts = (String, BTreeMap<String, String>, AuthConfig, bool, u64);
use super::{
    AuthConfig, BTreeMap, CachePolicy, CachedToken, CircuitBreakerPolicy, CookiePolicy, Duration,
    EngineError, ErrorDetail, ErrorPhase, HttpMethod, Instant, MAX_CACHED_TOKENS, OAuthClientAuth,
    PreparedBody, PreparedRequest, RequestStats, RetryPolicy, SystemTime, TokenKey, Transport,
    UNIX_EPOCH, Url, Value, enter_phase, fingerprint, number_as_u64,
};
impl Transport {
    pub(super) async fn resolve_auth(
        &self,
        request: &mut PreparedRequest,
    ) -> Result<RequestStats, EngineError> {
        if matches!(
            &request.auth,
            AuthConfig::OAuth2ClientCredentials { .. }
                | AuthConfig::OAuth2Password { .. }
                | AuthConfig::ArcgisToken { .. }
        ) {
            enter_phase(ErrorPhase::Connect);
            let (token, stats) = self.oauth_token(request).await?;
            request.auth = AuthConfig::Bearer { token };
            Ok(stats)
        } else {
            Ok(RequestStats::default())
        }
    }
}

impl Transport {
    async fn oauth_token(
        &self,
        original: &PreparedRequest,
    ) -> Result<(String, RequestStats), EngineError> {
        #[cfg(test)]
        crate::injection::visit(crate::injection::Point::OAuth);
        let (token_url, form, request_auth, is_arcgis, fallback_ttl) =
            token_request_parts(&original.auth)?;
        let key = TokenKey {
            token_url: token_url.clone(),
            auth_fingerprint: fingerprint(&original.auth),
            transport_fingerprint: fingerprint(&(&original.tls, &original.proxy)),
        };
        if let Some(token) = self
            .tokens
            .lock()
            .await
            .get(&key)
            .filter(|token| token.expires_at > Instant::now())
            .map(|token| token.token.clone())
        {
            #[cfg(test)]
            crate::injection::visit(crate::injection::Point::OAuthCached);
            return Ok((token, RequestStats::default()));
        }
        let _refresh = self.token_refresh.lock().await;
        if let Some(token) = self
            .tokens
            .lock()
            .await
            .get(&key)
            .filter(|token| token.expires_at > Instant::now())
            .map(|token| token.token.clone())
        {
            return Ok((token, RequestStats::default()));
        }

        let url = Url::parse(&token_url).map_err(|_| {
            EngineError::InvalidUrl(ErrorDetail::from("OAuth token URL is invalid"))
        })?;
        let token_request = PreparedRequest {
            url,
            method: HttpMethod::Post,
            headers: BTreeMap::new(),
            auth: request_auth,
            body: PreparedBody::Form(form.into_iter().collect()),
            timeout: Duration::from_millis(self.config.request_timeout_ms),
            allow_redirects: false,
            max_redirects: 0,
            retry: RetryPolicy {
                max_attempts: 2,
                retry_non_idempotent: true,
                ..RetryPolicy::default()
            },
            cookies: CookiePolicy::default(),
            admitted_jar: None,
            caller_session: None,
            cache: CachePolicy::default(),
            circuit_breaker: CircuitBreakerPolicy::default(),
            requests_per_second: original.requests_per_second,
            tls: original.tls.clone(),
            proxy: original.proxy.clone(),
        };
        let response = self.execute_authenticated(&token_request).await?;
        #[cfg(test)]
        crate::injection::visit(crate::injection::Point::OAuthRead);
        let stats = RequestStats {
            network_requests: response.network_requests,
            retries: u64::from(response.attempts.saturating_sub(1)),
            rate_limit_wait_ms: response.rate_limit_wait_ms,
        };
        if !(200..300).contains(&response.status) {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "token endpoint returned an unsuccessful HTTP status",
            )));
        }

        crate::control_json::validate(&response.body).map_err(|error| {
            EngineError::Authentication(ErrorDetail::at(error.message, error.line, error.column))
        })?;
        let payload: Value = serde_json::from_slice(&response.body).map_err(|_| {
            EngineError::Authentication(ErrorDetail::from("token endpoint returned invalid JSON"))
        })?;
        if is_arcgis && payload.get("error").is_some() {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "ArcGIS token endpoint returned an error",
            )));
        }
        let token_field = if is_arcgis { "token" } else { "access_token" };
        let token = payload
            .get(token_field)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                EngineError::Authentication(ErrorDetail::from("token response has no token field"))
            })?
            .to_owned();
        if !is_arcgis
            && payload
                .get("token_type")
                .and_then(Value::as_str)
                .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("bearer"))
        {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "only bearer OAuth tokens are supported",
            )));
        }
        let expires_in = if is_arcgis {
            payload
                .get("expires")
                .and_then(number_as_u64)
                .and_then(|expires_ms| {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()?
                        .as_millis() as u64;
                    expires_ms.checked_sub(now_ms).map(|value| value / 1_000)
                })
                .unwrap_or(fallback_ttl)
        } else {
            payload
                .get("expires_in")
                .and_then(number_as_u64)
                .unwrap_or(fallback_ttl)
        };
        let refresh_margin = (expires_in / 10).clamp(1, 30);
        let ttl = expires_in.saturating_sub(refresh_margin).max(1);
        let sequence = self.next_sequence();
        {
            let mut tokens = self.tokens.lock().await;
            let now = Instant::now();
            tokens.retain(|_, cached| cached.expires_at > now);
            while tokens.len() >= MAX_CACHED_TOKENS {
                let oldest = tokens
                    .iter()
                    .min_by_key(|(_, cached)| (cached.expires_at, cached.sequence))
                    .map(|(key, _)| key.clone());
                let Some(oldest) = oldest else {
                    break;
                };
                tokens.remove(&oldest);
            }
            tokens.insert(
                key,
                CachedToken {
                    token: token.clone(),
                    expires_at: now + Duration::from_secs(ttl),
                    sequence,
                },
            );
        }
        Ok((token, stats))
    }
}

fn token_request_parts(auth: &AuthConfig) -> Result<TokenRequestParts, EngineError> {
    match auth {
        AuthConfig::OAuth2ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
            audience,
            extra_params,
            client_auth,
        } => {
            let mut form = extra_params.clone();
            form.insert("grant_type".to_owned(), "client_credentials".to_owned());
            if let Some(scope) = scope {
                form.insert("scope".to_owned(), scope.clone());
            }
            if let Some(audience) = audience {
                form.insert("audience".to_owned(), audience.clone());
            }
            let request_auth = match client_auth {
                OAuthClientAuth::Basic => AuthConfig::Basic {
                    username: client_id.clone(),
                    password: client_secret.clone(),
                },
                OAuthClientAuth::Body => {
                    form.insert("client_id".to_owned(), client_id.clone());
                    form.insert("client_secret".to_owned(), client_secret.clone());
                    AuthConfig::None
                }
            };
            Ok((token_url.clone(), form, request_auth, false, 300))
        }
        AuthConfig::OAuth2Password {
            token_url,
            username,
            password,
            client_id,
            client_secret,
            scope,
            extra_params,
        } => {
            let mut form = extra_params.clone();
            form.insert("grant_type".to_owned(), "password".to_owned());
            form.insert("username".to_owned(), username.clone());
            form.insert("password".to_owned(), password.clone());
            if let Some(client_id) = client_id {
                form.insert("client_id".to_owned(), client_id.clone());
            }
            if let Some(client_secret) = client_secret {
                form.insert("client_secret".to_owned(), client_secret.clone());
            }
            if let Some(scope) = scope {
                form.insert("scope".to_owned(), scope.clone());
            }
            Ok((token_url.clone(), form, AuthConfig::None, false, 300))
        }
        AuthConfig::ArcgisToken {
            token_url,
            username,
            password,
            client,
            referer,
            ip,
            expiration,
        } => {
            if !matches!(client.as_str(), "requestip" | "referer" | "ip") {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "ArcGIS client must be requestip, referer, or ip",
                )));
            }
            let mut form = BTreeMap::from([
                ("username".to_owned(), username.clone()),
                ("password".to_owned(), password.clone()),
                ("client".to_owned(), client.clone()),
                ("expiration".to_owned(), expiration.to_string()),
                ("f".to_owned(), "json".to_owned()),
            ]);
            if client == "referer" {
                form.insert(
                    "referer".to_owned(),
                    referer.clone().ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "ArcGIS referer client requires referer",
                        ))
                    })?,
                );
            } else if client == "ip" {
                form.insert(
                    "ip".to_owned(),
                    ip.clone().ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from("ArcGIS ip client requires ip"))
                    })?,
                );
            }
            Ok((
                token_url.clone(),
                form,
                AuthConfig::None,
                true,
                u64::from(*expiration).saturating_mul(60),
            ))
        }
        _ => Err(EngineError::Runtime(ErrorDetail::from(
            "token requested for non-token authentication",
        ))),
    }
}
