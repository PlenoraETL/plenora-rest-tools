//! Remote job polling, recovery and best-effort cancellation.

use super::{
    ACTIVE_ASYNC_JOBS, ASYNC_JOB_RECOVERY_CONTRACT, ActiveAsyncJob, ActiveRemoteCancel, Arc,
    AsyncJobRecovery, BTreeMap, CachePolicy, ConnectionConfig, CredentialScope, Duration, Engine,
    EngineError, ErrorDetail, ExecutionMetrics, HttpMethod, Instant, Map, Mutex, PollCompletion,
    PollingCancelConfig, PollingConfig, PreparedBody, PreparedRequest, REMOTE_CANCEL_BUDGET,
    REMOTE_CANCEL_CONCURRENCY, RemoteCancelTrigger, ResponseData, StreamExt, Url, Value, json_path,
    render_template, same_origin, sleep, stream, value_as_text,
};
impl Engine {
    pub(super) async fn cancel_active_jobs(
        &self,
        jobs: &Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>,
        trigger: RemoteCancelTrigger,
    ) {
        let requests = {
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.iter_mut()
                .filter_map(|(key, job)| {
                    let cancel = job.cancel.as_ref()?;
                    if !cancel_enabled(cancel, trigger) {
                        return None;
                    }
                    if let Some(recovery) = job.recovery.as_mut() {
                        recovery.cancel_requested = true;
                    }
                    // Re-applied here, not only when the job was registered: a
                    // later hop may have revoked the authorization since.
                    let mut request = cancel.request.clone();
                    cancel.scope.apply(&mut request, None);
                    Some((key.clone(), request))
                })
                .collect::<Vec<_>>()
        };
        // Best-effort cleanup after a deadline or a cancellation must not extend
        // the very deadline it is reacting to: the remote cancellations run with
        // bounded concurrency inside a single global budget, and whatever does
        // not finish in time is simply left unreported.
        let cleanup = stream::iter(requests)
            .map(|(key, request)| {
                let jobs = jobs.clone();
                async move {
                    let accepted = self
                        .transport
                        .execute(request)
                        .await
                        .is_ok_and(|response| (200..300).contains(&response.status));
                    let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(recovery) = jobs.get_mut(&key).and_then(|job| job.recovery.as_mut())
                    {
                        recovery.cancel_accepted = Some(accepted);
                    }
                }
            })
            .buffer_unordered(REMOTE_CANCEL_CONCURRENCY)
            .collect::<Vec<()>>();
        let _ = tokio::time::timeout(REMOTE_CANCEL_BUDGET, cleanup).await;
    }

    async fn cancel_active_job(&self, key: &str, trigger: RemoteCancelTrigger) {
        let Some(jobs) = active_jobs_handle() else {
            return;
        };
        let request = {
            let mut locked = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(job) = locked.get_mut(key) else {
                return;
            };
            let Some(cancel) = job.cancel.as_ref() else {
                return;
            };
            if !cancel_enabled(cancel, trigger) {
                return;
            }
            if let Some(recovery) = job.recovery.as_mut() {
                recovery.cancel_requested = true;
            }
            let mut request = cancel.request.clone();
            cancel.scope.apply(&mut request, None);
            request
        };
        // Same budget as the deadline and cancellation paths: a poll that has
        // already exhausted `max_wait_ms` must not then block indefinitely on a
        // best-effort remote cancellation. On expiry the recovery handle keeps
        // `cancel_accepted = None`, which is exactly "requested, outcome
        // unknown".
        let Ok(accepted) =
            tokio::time::timeout(REMOTE_CANCEL_BUDGET, self.transport.execute(request)).await
        else {
            return;
        };
        let accepted = accepted.is_ok_and(|response| (200..300).contains(&response.status));
        let mut locked = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(recovery) = locked.get_mut(key).and_then(|job| job.recovery.as_mut()) {
            recovery.cancel_accepted = Some(accepted);
        }
    }
}

impl Engine {
    pub(super) async fn poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        initial_value: Value,
        initial_response: ResponseData,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_poll_completion(
                connection,
                polling,
                initial_value,
                initial_response,
                credential_scope,
                metrics,
            )
            .await?;
        let active_key = completion.active_key.clone();
        let result = self
            .complete_poll(
                connection,
                polling,
                completion.value,
                completion.response,
                completion.job_id.as_ref(),
                credential_scope,
                metrics,
            )
            .await;
        if result.is_ok()
            && let Some(key) = active_key
        {
            remove_active_job(&key);
        }
        result
    }

    pub(super) async fn resume_poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        let completion = self
            .await_resumed_poll_completion(connection, polling, credential_scope, metrics)
            .await?;
        let active_key = completion.active_key.clone();
        let result = self
            .complete_poll(
                connection,
                polling,
                completion.value,
                completion.response,
                completion.job_id.as_ref(),
                credential_scope,
                metrics,
            )
            .await;
        if result.is_ok()
            && let Some(key) = active_key
        {
            remove_active_job(&key);
        }
        result
    }

    pub(super) async fn await_poll_completion(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        initial_value: Value,
        initial_response: ResponseData,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        let job_id = polling_job_id(&initial_value, &initial_response, polling);
        match poll_state(&initial_value, polling)? {
            Some(PollState::Success) => {
                return Ok(PollCompletion {
                    value: initial_value,
                    response: initial_response,
                    job_id,
                    active_key: None,
                });
            }
            Some(PollState::Failure) => {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "asynchronous operation reported a failure status",
                )));
            }
            Some(PollState::Pending) | None => {}
        }
        let poll_url = poll_url(&initial_value, &initial_response, polling)?;
        if !polling.allow_cross_origin && !same_origin(&initial_response.final_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling is blocked",
            )));
        }

        // Narrowed in place, so everything derived afterwards inherits the
        // restriction: the remote cancellation, the result URL, and — because
        // the caller shares this scope — any later pagination request as well.
        credential_scope.narrow(&poll_url);
        let active_key = self.register_polled_job(
            connection,
            polling,
            &poll_url,
            job_id.as_ref(),
            credential_scope,
        )?;
        self.await_poll_url(
            connection,
            polling,
            poll_url,
            job_id,
            active_key,
            credential_scope,
            metrics,
        )
        .await
    }

    pub(super) async fn await_resumed_poll_completion(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        let base_url = &credential_scope.origin.clone();
        let resume = polling.resume.as_ref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("polling resume configuration is missing"))
        })?;
        validate_job_id(&resume.job_id)?;
        let job_id = Value::String(resume.job_id.clone());
        let template = polling.url_template.as_deref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from(
                "polling resume requires url_template and cannot infer a prior Location URL",
            ))
        })?;
        let target = render_poll_template_from_base(template, base_url, Some(&job_id))?;
        let poll_url = base_url.join(&target).map_err(|_| {
            EngineError::InvalidUrl(ErrorDetail::from("polling resume URL is invalid"))
        })?;
        if !polling.allow_cross_origin && !same_origin(base_url, &poll_url) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling is blocked",
            )));
        }
        credential_scope.narrow(&poll_url);
        let active_key = self.register_polled_job(
            connection,
            polling,
            &poll_url,
            Some(&job_id),
            credential_scope,
        )?;
        self.await_poll_url(
            connection,
            polling,
            poll_url,
            Some(job_id),
            active_key,
            credential_scope,
            metrics,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn await_poll_url(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: Url,
        job_id: Option<Value>,
        active_key: String,
        credential_scope: &CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<PollCompletion, EngineError> {
        if polling.max_attempts == 0 {
            remove_active_job(&active_key);
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "polling max_attempts must be greater than zero",
            )));
        }
        let poll_started = Instant::now();
        let budget = polling.max_wait_ms.map(Duration::from_millis);
        // `max_wait_ms` is a hard bound: it caps the backoff sleep *and* the
        // request itself, so a slow poll cannot run past the configured limit.
        let remaining = |elapsed: Duration| budget.map(|budget| budget.saturating_sub(elapsed));
        let mut interval_ms = polling.interval_ms;
        // The factor is validated (finite, at least one) before any request.
        // The first wait is interval_ms itself; the backoff then gives
        // min(max_interval_ms, interval_ms · interval_backoff^n), see
        // crate::exact::Backoff.
        let mut backoff = crate::exact::Backoff::new(
            polling.interval_ms,
            polling.interval_backoff,
            polling.max_interval_ms,
        )
        .ok_or_else(|| {
            EngineError::Runtime(ErrorDetail::from("polling interval backoff is invalid"))
        })?;
        backoff.next_ms();
        let mut attempts = 0_u32;
        for _ in 0..polling.max_attempts {
            if remaining(poll_started.elapsed()).is_some_and(|left| left.is_zero()) {
                break;
            }
            #[cfg(test)]
            crate::injection::visit(crate::injection::Point::Polling);
            if interval_ms > 0 {
                let delay = Duration::from_millis(interval_ms);
                let delay = remaining(poll_started.elapsed()).map_or(delay, |left| delay.min(left));
                sleep(delay).await;
            }
            let mut request = self.prepare_followup_request(
                connection,
                poll_url.clone(),
                polling.method.clone(),
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
                CachePolicy::default(),
            );
            credential_scope.apply(&mut request, None);
            let attempt = self.execute_prepared_json(connection, request, metrics, true);
            // Counted before awaiting: once the request is issued it has been
            // sent, whether or not the remaining budget lets us read the reply.
            let (value, response) = match remaining(poll_started.elapsed()) {
                Some(left) if left.is_zero() => break,
                Some(left) => {
                    attempts = attempts.saturating_add(1);
                    match tokio::time::timeout(left, attempt).await {
                        Ok(result) => result?,
                        Err(_) => break,
                    }
                }
                None => {
                    attempts = attempts.saturating_add(1);
                    attempt.await?
                }
            };
            match poll_state(&value, polling)? {
                Some(PollState::Success) => {
                    return Ok(PollCompletion {
                        value,
                        response,
                        job_id,
                        active_key: Some(active_key),
                    });
                }
                Some(PollState::Failure) => {
                    remove_active_job(&active_key);
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "asynchronous operation reported a failure status",
                    )));
                }
                Some(PollState::Pending) => {}
                None => {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "poll response has no status at status_path",
                    )));
                }
            }
            interval_ms = backoff.next_ms();
        }

        self.cancel_active_job(&active_key, RemoteCancelTrigger::PollTimeout)
            .await;
        // Report the attempts actually issued: `max_wait_ms` can end the loop
        // before `max_attempts` is reached.
        Err(EngineError::PollingTimeout { attempts })
    }

    fn register_polled_job(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        poll_url: &Url,
        job_id: Option<&Value>,
        credential_scope: &CredentialScope,
    ) -> Result<String, EngineError> {
        let key = poll_url.as_str().to_owned();
        let recovery = job_id
            .and_then(public_job_id)
            .map(|job_id| AsyncJobRecovery {
                contract: ASYNC_JOB_RECOVERY_CONTRACT.to_owned(),
                job_id,
                cancel_requested: false,
                cancel_accepted: None,
            });
        let cancel = polling
            .cancel
            .as_ref()
            .map(|cancel| {
                self.prepare_remote_cancel(
                    connection,
                    polling,
                    cancel,
                    poll_url,
                    job_id,
                    credential_scope,
                )
            })
            .transpose()?;
        register_active_job(key.clone(), ActiveAsyncJob { recovery, cancel });
        Ok(key)
    }

    fn prepare_remote_cancel(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        cancel: &PollingCancelConfig,
        poll_url: &Url,
        job_id: Option<&Value>,
        credential_scope: &CredentialScope,
    ) -> Result<ActiveRemoteCancel, EngineError> {
        if cancel.timeout_ms == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "polling cancel timeout_ms must be greater than zero",
            )));
        }
        let target = match cancel.url_template.as_deref() {
            Some(template) => {
                let rendered = render_poll_template_from_base(template, poll_url, job_id)?;
                poll_url.join(&rendered).map_err(|_| {
                    EngineError::InvalidUrl(ErrorDetail::from("polling cancel URL is invalid"))
                })?
            }
            None => poll_url.clone(),
        };
        if !polling.allow_cross_origin && !same_origin(poll_url, &target) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling cancellation is blocked",
            )));
        }
        let mut request = self.prepare_followup_request(
            connection,
            target,
            cancel.method.clone(),
            cancel.timeout_ms,
            CachePolicy::default(),
        );
        credential_scope.apply(&mut request, None);
        request.retry.max_attempts = 1;
        Ok(ActiveRemoteCancel {
            request,
            scope: credential_scope.clone(),
            on_cancellation: cancel.on_cancellation,
            on_deadline: cancel.on_deadline,
            on_poll_timeout: cancel.on_poll_timeout,
        })
    }

    fn prepare_followup_request(
        &self,
        connection: &ConnectionConfig,
        url: Url,
        method: HttpMethod,
        timeout_ms: u64,
        cache: CachePolicy,
    ) -> PreparedRequest {
        PreparedRequest {
            url,
            method,
            headers: connection.headers.clone(),
            auth: connection.auth.clone(),
            body: PreparedBody::None,
            timeout: Duration::from_millis(timeout_ms),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry: connection.retry.clone(),
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache,
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_poll(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        status_value: Value,
        status_response: ResponseData,
        job_id: Option<&Value>,
        credential_scope: &mut CredentialScope,
        metrics: &mut ExecutionMetrics,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>, u16), EngineError> {
        if polling.result_url_path.is_none() && polling.result_url_template.is_none() {
            return Ok((
                poll_result(status_value, polling)?,
                status_response.final_url,
                status_response.attempts,
                status_response.headers,
                status_response.status,
            ));
        }
        let request = self.prepare_poll_result_request(
            connection,
            polling,
            &status_value,
            &status_response,
            job_id,
            credential_scope,
        )?;
        let (value, response) = self
            .execute_prepared_json(connection, request, metrics, true)
            .await?;
        Ok((
            poll_result(value, polling)?,
            response.final_url,
            response.attempts,
            response.headers,
            response.status,
        ))
    }

    pub(super) fn prepare_poll_result_request(
        &self,
        connection: &ConnectionConfig,
        polling: &PollingConfig,
        status_value: &Value,
        status_response: &ResponseData,
        job_id: Option<&Value>,
        credential_scope: &mut CredentialScope,
    ) -> Result<PreparedRequest, EngineError> {
        let target = polling_result_url(status_value, status_response, polling, job_id)?;
        if !polling.allow_cross_origin && !same_origin(&status_response.final_url, &target) {
            return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                "cross-origin polling result URL is blocked",
            )));
        }
        let mut request = PreparedRequest {
            url: target,
            method: polling.result_method.clone(),
            headers: connection.headers.clone(),
            auth: connection.auth.clone(),
            body: PreparedBody::None,
            timeout: Duration::from_millis(
                connection
                    .request
                    .timeout_ms
                    .unwrap_or(self.config.request_timeout_ms),
            ),
            allow_redirects: connection.request.allow_redirects,
            max_redirects: connection.request.max_redirects,
            retry: connection.retry.clone(),
            cookies: connection.cookies.clone(),
            admitted_jar: None,
            caller_session: connection.cookies.session.clone(),
            cache: connection.cache.clone(),
            circuit_breaker: connection.circuit_breaker.clone(),
            requests_per_second: connection.requests_per_second,
            tls: connection.tls.clone(),
            proxy: connection.proxy.clone(),
        };
        credential_scope.narrow(&request.url);
        credential_scope.apply(&mut request, None);
        Ok(request)
    }
}

pub(super) enum PollState {
    Pending,
    Success,
    /// The remote status is not kept: it is third-party text.
    Failure,
}

fn poll_state(response: &Value, polling: &PollingConfig) -> Result<Option<PollState>, EngineError> {
    let Some(value) = json_path::get(response, &polling.status_path) else {
        return Ok(None);
    };
    // A null status is not the empty string: it matches no configured value,
    // even an empty one, and is reported instead of being guessed.
    if value.is_null() {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "asynchronous status is null",
        )));
    }
    let status = value_as_text(value);
    if polling
        .pending_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Pending));
    }
    if polling
        .success_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Success));
    }
    if polling
        .failure_values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(&status))
    {
        return Ok(Some(PollState::Failure));
    }
    Err(EngineError::InvalidResponse(ErrorDetail::from(
        "unknown asynchronous status",
    )))
}

pub(super) fn evaluate_application_success(
    response: &Value,
    connection: &ConnectionConfig,
) -> Result<(), EngineError> {
    if let Some(path) = &connection.response.error_path
        && json_path::get(response, path)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        // The remote message at error_path is not captured: it is
        // third-party text, and the error contract carries none.
        return Err(EngineError::Application(ErrorDetail::from(
            "the response reports an error at error_path",
        )));
    }
    if let Some(condition) = &connection.response.success_when
        && !matches_success_condition(response, condition)
    {
        return Err(EngineError::Application(ErrorDetail::from(
            "success_when condition was not satisfied",
        )));
    }
    Ok(())
}

fn matches_success_condition(response: &Value, condition: &Value) -> bool {
    static NULL_VALUE: Value = Value::Null;
    if let Some(conditions) = condition.as_array() {
        return conditions
            .iter()
            .all(|condition| matches_success_condition(response, condition));
    }
    let Some(condition) = condition.as_object() else {
        return condition.as_bool().unwrap_or(!condition.is_null());
    };
    let path = condition.get("path").and_then(Value::as_str);
    let value = path
        .map(|path| json_path::get(response, path).unwrap_or(&NULL_VALUE))
        .unwrap_or(response);
    if let Some(expected) = condition.get("exists").and_then(Value::as_bool) {
        let exists = condition
            .get("path")
            .and_then(Value::as_str)
            .and_then(|path| json_path::get(response, path))
            .is_some();
        return exists == expected;
    }
    if let Some(expected) = condition.get("truthy").and_then(Value::as_bool) {
        return json_truthy(value) == expected;
    }
    if let Some(expected) = condition.get("equals") {
        return value == expected;
    }
    if let Some(expected) = condition.get("not_equals") {
        return value != expected;
    }
    if let Some(values) = condition.get("in").and_then(Value::as_array) {
        return values.contains(value);
    }
    if let Some(values) = condition.get("not_in").and_then(Value::as_array) {
        return !values.contains(value);
    }
    json_truthy(value)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn poll_result(response: Value, polling: &PollingConfig) -> Result<Value, EngineError> {
    match &polling.result_path {
        Some(path) => json_path::get(&response, path).cloned().ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("polling result path was not found"))
        }),
        None => Ok(response),
    }
}

fn poll_url(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
) -> Result<Url, EngineError> {
    let target = if let Some(path) = &polling.url_path {
        json_path::get(response, path)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from("polling URL path was not found"))
            })?
            .to_owned()
    } else if let Some(template) = &polling.url_template {
        let job_id = polling_job_id(response, response_data, polling).ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from("polling job id was not found"))
        })?;
        render_poll_template(template, response_data, Some(&job_id))?
    } else if let Some(header) = &polling.location_header {
        response_data
            .headers
            .get(&header.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from(
                    "polling response has no location header",
                ))
            })?
    } else {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling requires url_path, url_template, or location_header",
        )));
    };

    response_data
        .final_url
        .join(&target)
        .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("polling URL is invalid")))
}

fn polling_job_id(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
) -> Option<Value> {
    if let Some(header) = &polling.id_header {
        response_data
            .headers
            .get(&header.to_ascii_lowercase())
            .cloned()
            .map(Value::String)
    } else {
        // A null id is no id: rendered into a URL it would become "".
        json_path::get(response, &polling.id_path)
            .filter(|value| !value.is_null())
            .cloned()
    }
}

fn polling_result_url(
    response: &Value,
    response_data: &ResponseData,
    polling: &PollingConfig,
    job_id: Option<&Value>,
) -> Result<Url, EngineError> {
    let target = if let Some(template) = &polling.result_url_template {
        render_poll_template(template, response_data, job_id)?
    } else {
        let path = polling.result_url_path.as_deref().ok_or_else(|| {
            EngineError::InvalidInput(ErrorDetail::from("polling result URL is not configured"))
        })?;
        let template = json_path::get(response, path)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                EngineError::InvalidResponse(ErrorDetail::from(
                    "polling result URL path was not found",
                ))
            })?;
        render_poll_template(template, response_data, job_id)?
    };
    response_data
        .final_url
        .join(&target)
        .map_err(|_| EngineError::InvalidUrl(ErrorDetail::from("polling result URL is invalid")))
}

fn render_poll_template(
    template: &str,
    response_data: &ResponseData,
    job_id: Option<&Value>,
) -> Result<String, EngineError> {
    render_poll_template_from_base(template, &response_data.final_url, job_id)
}

fn render_poll_template_from_base(
    template: &str,
    base_url: &Url,
    job_id: Option<&Value>,
) -> Result<String, EngineError> {
    let base = base_url.origin().ascii_serialization();
    let template = template.replace("{base}", &base);
    if !template.contains("{id}") && !template.contains("{job_id}") {
        return Ok(template);
    }
    let job_id = job_id.ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("polling result URL requires a job id"))
    })?;
    let parameters = Map::from_iter([
        ("id".to_owned(), job_id.clone()),
        ("job_id".to_owned(), job_id.clone()),
    ]);
    Ok(render_template(&template, &parameters, true).0)
}

fn public_job_id(value: &Value) -> Option<String> {
    let value = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    validate_job_id(&value).ok().map(|_| value)
}

pub(super) fn validate_job_id(value: &str) -> Result<(), EngineError> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(|character| character.is_control())
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling job_id must contain 1 to 512 non-control characters",
        )));
    }
    Ok(())
}

fn cancel_enabled(cancel: &ActiveRemoteCancel, trigger: RemoteCancelTrigger) -> bool {
    match trigger {
        RemoteCancelTrigger::Cancellation => cancel.on_cancellation,
        RemoteCancelTrigger::Deadline => cancel.on_deadline,
        RemoteCancelTrigger::PollTimeout => cancel.on_poll_timeout,
    }
}

fn active_jobs_handle() -> Option<Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>> {
    ACTIVE_ASYNC_JOBS.try_with(Arc::clone).ok()
}

fn register_active_job(key: String, job: ActiveAsyncJob) {
    let Some(jobs) = active_jobs_handle() else {
        return;
    };
    jobs.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, job);
}

pub(super) fn remove_active_job(key: &str) {
    let Some(jobs) = active_jobs_handle() else {
        return;
    };
    jobs.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

pub(super) fn active_recoveries() -> RecoveryHandles {
    active_jobs_handle()
        .map(|jobs| recoveries_from(&jobs))
        .unwrap_or_default()
}

/// Contract bound on `recoveries` in the execution and file transfer results.
pub(super) const MAX_RECOVERIES: usize = 128;

/// The recovery handles a result can carry, and how many did not fit.
#[derive(Default)]
pub(super) struct RecoveryHandles {
    pub(super) handles: Vec<AsyncJobRecovery>,
    pub(super) omitted: usize,
}

pub(super) fn recoveries_from(
    jobs: &Arc<Mutex<BTreeMap<String, ActiveAsyncJob>>>,
) -> RecoveryHandles {
    // The map is keyed by poll URL, so the cut at the contract bound is
    // deterministic rather than dependent on iteration order.
    let mut handles = jobs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|job| job.recovery.clone())
        .collect::<Vec<_>>();
    let omitted = handles.len().saturating_sub(MAX_RECOVERIES);
    handles.truncate(MAX_RECOVERIES);
    RecoveryHandles { handles, omitted }
}
