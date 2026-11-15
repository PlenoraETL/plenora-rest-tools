//! Execute prepared JSON requests and project public response metadata.

use super::{
    BTreeMap, ConnectionConfig, CredentialScope, Engine, EngineError, ExecutionMetrics,
    HttpResponseMetadata, JsonObject, PreparedRequest, ResponseData, Url, Value,
    evaluate_application_success, idempotency_header_withheld, is_sensitive_header_name,
    preserved_idempotency_header, public_response_url, response_body,
};
impl Engine {
    pub(super) async fn request_json(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        let request = self.prepare_request(
            connection,
            parameters,
            url_override,
            options.idempotency_key.as_deref(),
        )?;
        self.request_prepared_json(connection, request, None, metrics, responses, options)
            .await
    }

    /// Pagination step.
    ///
    /// `credential_scope` is fixed by the URL the first page is *sent to* and
    /// then reused unchanged for every later page and for every follow-up
    /// derived from them. It is deliberately not taken from a response: the URL
    /// a page finishes on may already be a polling or result URL the remote
    /// service chose, and letting that become the owning origin would hand the
    /// credentials to it on the next page.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn request_page(
        &self,
        connection: &ConnectionConfig,
        parameters: &JsonObject,
        url_override: Option<&str>,
        credential_scope: &mut Option<CredentialScope>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        let mut request = self.prepare_request(
            connection,
            parameters,
            url_override,
            options.idempotency_key.as_deref(),
        )?;
        // The owning origin is the URL the first page is sent to, and the
        // authorization only ever narrows: a page served by another origin
        // revokes it for every later page as well.
        let scope =
            credential_scope.get_or_insert_with(|| CredentialScope::new(request.url.clone()));
        scope.narrow(&request.url);
        scope.apply(
            &mut request,
            preserved_idempotency_header(connection, options),
        );
        // The idempotency key is what switched on retries for a non-idempotent
        // method. When its header had to be withheld from this origin, the
        // request is no longer protected, so it is retried only if the caller
        // asked for that independently of the key.
        if idempotency_header_withheld(connection, options, &request) {
            request.retry.retry_non_idempotent = connection.retry.retry_non_idempotent;
        }
        self.request_prepared_json(
            connection,
            request,
            Some(scope),
            metrics,
            responses,
            options,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn request_prepared_json(
        &self,
        connection: &ConnectionConfig,
        request: PreparedRequest,
        credential_scope: Option<&mut CredentialScope>,
        metrics: &mut ExecutionMetrics,
        responses: &mut Vec<HttpResponseMetadata>,
        options: &crate::ExecutionOptions,
    ) -> Result<(Value, Url, u32, BTreeMap<String, String>), EngineError> {
        // Origin that owns the credentials carried by this request; every
        // follow-up leaving it must not take them along.
        //
        // An explicit origin always wins: deriving it from `request.url` would
        // re-anchor the scope to a URL a remote service chose, so a page already
        // stripped of credentials could hand them back to its own origin through
        // a polling follow-up.
        // An explicit scope is borrowed, not copied, so a restriction applied
        // while following the polling chain is visible to the caller afterwards.
        let mut owned_scope;
        let credential_scope = match credential_scope {
            Some(scope) => scope,
            None => {
                owned_scope = CredentialScope::new(request.url.clone());
                &mut owned_scope
            }
        };
        let result = match &connection.polling {
            Some(polling) if polling.resume.is_some() => {
                self.resume_poll(connection, polling, credential_scope, metrics)
                    .await
            }
            polling => {
                let (initial_value, initial_response) = self
                    .execute_prepared_json(connection, request, metrics, false)
                    .await?;
                match polling {
                    Some(polling) => {
                        self.poll(
                            connection,
                            polling,
                            initial_value,
                            initial_response,
                            credential_scope,
                            metrics,
                        )
                        .await
                    }
                    None => Ok((
                        initial_value,
                        initial_response.final_url,
                        initial_response.attempts,
                        initial_response.headers,
                        initial_response.status,
                    )),
                }
            }
        }?;
        evaluate_application_success(&result.0, connection)?;
        if options.capture_response_metadata {
            responses.push(HttpResponseMetadata {
                status: result.4,
                final_url: public_response_url(&result.1),
                attempts: result.2,
                headers: selected_response_headers(&result.3, &options.response_headers),
            });
        }
        Ok((result.0, result.1, result.2, result.3))
    }

    pub(super) async fn execute_prepared_json(
        &self,
        connection: &ConnectionConfig,
        request: PreparedRequest,
        metrics: &mut ExecutionMetrics,
        is_poll: bool,
    ) -> Result<(Value, ResponseData), EngineError> {
        let response = self.transport.execute(request).await?;
        metrics.requests = metrics.requests.saturating_add(response.network_requests);
        metrics.retries = metrics
            .retries
            .saturating_add(u64::from(response.attempts.saturating_sub(1)))
            .saturating_add(response.auth_retries);
        metrics.auth_requests = metrics.auth_requests.saturating_add(response.auth_requests);
        metrics.cache_hits = metrics.cache_hits.saturating_add(response.cache_hits);
        metrics.cache_revalidations = metrics
            .cache_revalidations
            .saturating_add(response.cache_revalidations);
        metrics.rate_limit_wait_ms = metrics
            .rate_limit_wait_ms
            .saturating_add(response.rate_limit_wait_ms);
        if is_poll {
            metrics.poll_requests = metrics.poll_requests.saturating_add(
                response
                    .network_requests
                    .saturating_sub(response.auth_requests),
            );
        }
        ensure_success(connection, &response)?;

        let value = if response.body.is_empty() {
            Value::Null
        } else {
            response_body::parse(&response.body, &connection.response)?
        };
        Ok((value, response))
    }
}

pub(super) fn selected_response_headers(
    headers: &BTreeMap<String, String>,
    selected: &[String],
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            !is_sensitive_header_name(name)
                && (selected.iter().any(|name| name == "*")
                    || selected
                        .iter()
                        .any(|selected| selected.eq_ignore_ascii_case(name)))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn ensure_success(
    connection: &ConnectionConfig,
    response: &ResponseData,
) -> Result<(), EngineError> {
    if (200..300).contains(&response.status)
        || connection.success_statuses.contains(&response.status)
    {
        return Ok(());
    }

    Err(EngineError::HttpStatus {
        status: response.status,
    })
}
