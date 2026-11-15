//! Validate execution configuration before entering an executor.

use super::{
    ConnectionConfig, EngineConfig, EngineError, ErrorDetail, ExecutionOperation, ExecutionRequest,
    PaginationConfig, json_path, validate_job_id, validate_transforms,
};

pub(super) fn validate_idempotency_key(key: &str) -> Result<(), EngineError> {
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "idempotency_key must contain 1 to 255 visible ASCII bytes",
        )));
    }
    Ok(())
}

/// Numeric settings of the engine that have no meaningful value at zero.
///
/// `Engine::new` cannot fail, so an unusable configuration is refused by
/// every execution instead of being quietly replaced: a zero concurrency used
/// to become one, and a zero rate became a wait of centuries.
pub(super) fn validate_engine_configuration(config: &EngineConfig) -> Result<(), EngineError> {
    let invalid = |reason: &'static str| Err(EngineError::InvalidInput(ErrorDetail::from(reason)));
    validate_timer_fields(&[
        config.connect_timeout_ms,
        config.request_timeout_ms,
        config.pool_idle_timeout_ms,
    ])?;
    if config.connect_timeout_ms == 0 {
        return invalid("engine connect_timeout_ms must be greater than zero");
    }
    if config.request_timeout_ms == 0 {
        return invalid("engine request_timeout_ms must be greater than zero");
    }
    if config.max_concurrent_requests == 0
        || config.max_concurrent_requests > tokio::sync::Semaphore::MAX_PERMITS
    {
        return invalid("engine max_concurrent_requests is zero or above the supported maximum");
    }
    if config
        .requests_per_second
        .is_some_and(|rate| crate::temporal::rate_interval(f64::from(rate)).is_none())
    {
        return invalid("engine requests_per_second must be between 1 and 10^9");
    }
    Ok(())
}

/// Numeric settings of a request that the engine used to replace silently:
/// a rate that is not finite or not positive fell back to the engine rate,
/// zero retry attempts became one, a backoff factor below one or not finite
/// became one, and zero rows or pages paginated nothing while reporting
/// success. Each is now refused before any request.
fn validate_numeric_settings(connection: &ConnectionConfig) -> Result<(), EngineError> {
    let invalid = |reason: &'static str| Err(EngineError::InvalidInput(ErrorDetail::from(reason)));
    if connection
        .requests_per_second
        .is_some_and(|rate| crate::temporal::rate_interval(rate).is_none())
    {
        return invalid("requests_per_second must give an interval between 1 ns and u64::MAX ns");
    }
    validate_timer_fields(&[
        connection.request.timeout_ms.unwrap_or(0),
        connection.retry.backoff_base_ms,
        connection.retry.max_backoff_ms,
    ])?;
    if connection.request.timeout_ms == Some(0) {
        return invalid("request timeout_ms must be greater than zero");
    }
    if connection.retry.max_attempts == 0 {
        return invalid("retry max_attempts must be greater than zero");
    }
    if !connection.retry.backoff_factor.is_finite() || connection.retry.backoff_factor < 1.0 {
        return invalid("retry backoff_factor must be finite and at least one");
    }
    let (max_rows, max_pages) = match connection.pagination.as_ref() {
        Some(
            PaginationConfig::Offset { max_rows, .. } | PaginationConfig::Page { max_rows, .. },
        ) => (Some(*max_rows), None),
        Some(
            PaginationConfig::Cursor {
                max_rows,
                max_pages,
                ..
            }
            | PaginationConfig::Link {
                max_rows,
                max_pages,
                ..
            }
            | PaginationConfig::HeaderLink {
                max_rows,
                max_pages,
                ..
            },
        ) => (Some(*max_rows), Some(*max_pages)),
        None => (None, None),
    };
    if max_rows == Some(0) {
        return invalid("pagination max_rows must be greater than zero");
    }
    if max_pages == Some(0) {
        return invalid("pagination max_pages must be greater than zero");
    }
    Ok(())
}

pub(super) fn validate_execution_configuration(
    request: &ExecutionRequest,
) -> Result<(), EngineError> {
    validate_transforms(&request.connection.response)?;
    validate_json_paths(&request.connection)?;
    validate_numeric_settings(&request.connection)?;
    let Some(polling) = request.connection.polling.as_ref() else {
        return Ok(());
    };
    validate_timer_fields(&[
        polling.interval_ms,
        polling.max_interval_ms,
        polling.max_wait_ms.unwrap_or(0),
        polling
            .cancel
            .as_ref()
            .map_or(0, |cancel| cancel.timeout_ms),
    ])?;
    if polling.max_attempts == 0 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling max_attempts must be greater than zero",
        )));
    }
    if polling.max_wait_ms == Some(0) {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling max_wait_ms must be greater than zero when configured",
        )));
    }
    if !polling.interval_backoff.is_finite() || polling.interval_backoff < 1.0 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling interval_backoff must be finite and at least one",
        )));
    }
    if polling
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.timeout_ms == 0)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling cancel timeout_ms must be greater than zero",
        )));
    }

    let Some(resume) = polling.resume.as_ref() else {
        return Ok(());
    };
    validate_job_id(&resume.job_id)?;
    if polling.url_template.is_none() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume requires url_template and cannot infer a prior Location URL",
        )));
    }
    if request.connection.pagination.is_some() {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume cannot be combined with pagination",
        )));
    }
    if request
        .connection
        .batch
        .as_ref()
        .is_some_and(|batch| batch.enabled)
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume cannot be combined with batch execution",
        )));
    }
    if request.operation == ExecutionOperation::Enrich && request.input.records.len() != 1 {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "polling resume for enrichment requires exactly one input record",
        )));
    }
    Ok(())
}

/// Every JSON path a request reads from a response must be well formed.
///
/// At run time a path that does not resolve means "the response does not have
/// it", which the operation may legitimately turn into null or a default. A
/// malformed path never resolves, so without this check a typo in the
/// configuration would be indistinguishable from a response that lacks the
/// field.
fn validate_json_paths(connection: &ConnectionConfig) -> Result<(), EngineError> {
    let response = &connection.response;
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(response.records_path.as_deref());
    paths.extend(response.error_path.as_deref());
    paths.extend(
        response
            .output_mapping
            .iter()
            .map(|mapping| mapping.path.as_str()),
    );
    paths.extend(
        response
            .iterate_on
            .iter()
            .map(|iteration| iteration.path.as_str()),
    );
    if let Some(batch) = connection.batch.as_ref() {
        paths.push(&batch.output_path);
    }
    if let Some(polling) = connection.polling.as_ref() {
        paths.push(&polling.id_path);
        paths.push(&polling.status_path);
        paths.extend(polling.url_path.as_deref());
        paths.extend(polling.result_path.as_deref());
        paths.extend(polling.result_url_path.as_deref());
    }
    match connection.pagination.as_ref() {
        Some(PaginationConfig::Cursor { cursor_path, .. }) => paths.push(cursor_path),
        Some(PaginationConfig::Link { link_path, .. }) => paths.push(link_path),
        _ => {}
    }
    if paths.into_iter().all(json_path::is_valid) {
        Ok(())
    } else {
        Err(EngineError::InvalidInput(ErrorDetail::from(
            "connection contains a malformed JSON path",
        )))
    }
}

fn validate_timer_fields(values: &[u64]) -> Result<(), EngineError> {
    if values
        .iter()
        .any(|value| crate::temporal::timer_millis(*value).is_none())
    {
        return Err(EngineError::InvalidInput(ErrorDetail::from(
            "timer exceeds the supported horizon",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod temporal_tests {
    use super::*;
    use crate::temporal::MAX_TIMER_MS;
    use serde_json::json;

    #[test]
    fn remote_time_wire_domain_is_not_truncated() {
        for literal in ["18446744073709551616", "-1", "1.5", "NaN", "Infinity"] {
            for name in [
                "connect_timeout_ms",
                "request_timeout_ms",
                "pool_idle_timeout_ms",
            ] {
                let input = format!("{{\"{name}\":{literal}}}");
                assert!(serde_json::from_str::<EngineConfig>(&input).is_err());
            }
            for (section, field) in [
                ("request", "timeout_ms"),
                ("retry", "backoff_base_ms"),
                ("retry", "max_backoff_ms"),
                ("retry", "max_retry_after_ms"),
                ("cache", "fresh_for_ms"),
                ("circuit_breaker", "recovery_timeout_ms"),
                ("polling", "interval_ms"),
                ("polling", "max_interval_ms"),
                ("polling", "max_wait_ms"),
            ] {
                let input = format!(
                    "{{\"url\":\"https://example.com\",\"{section}\":{{\"{field}\":{literal}}}}}"
                );
                assert!(
                    serde_json::from_str::<ConnectionConfig>(&input).is_err(),
                    "{section}.{field}: {literal}"
                );
            }
        }
    }

    #[test]
    fn remote_time_engine_fields() {
        for name in [
            "connect_timeout_ms",
            "request_timeout_ms",
            "pool_idle_timeout_ms",
        ] {
            for value in [0, 1, MAX_TIMER_MS, MAX_TIMER_MS + 1, u64::MAX] {
                let mut input = serde_json::to_value(EngineConfig::default()).unwrap();
                input[name] = json!(value);
                let config = serde_json::from_value(input).unwrap();
                assert_eq!(
                    validate_engine_configuration(&config).is_ok(),
                    value <= MAX_TIMER_MS && (value != 0 || name == "pool_idle_timeout_ms"),
                    "{name}: {value}"
                );
            }
        }
    }

    #[test]
    fn remote_time_request_fields() {
        for (section, field, zero_allowed) in [
            ("request", "timeout_ms", false),
            ("retry", "backoff_base_ms", true),
            ("retry", "max_backoff_ms", true),
            ("polling", "interval_ms", true),
            ("polling", "max_interval_ms", true),
            ("polling", "max_wait_ms", false),
            ("cancel", "timeout_ms", false),
        ] {
            for value in [0, 1, MAX_TIMER_MS, MAX_TIMER_MS + 1, u64::MAX] {
                let mut input = json!({"schema_version":1,"operation":"test","connection":{"url":"https://example.com"}});
                if section == "cancel" {
                    input["connection"]["polling"] = json!({"cancel":{"timeout_ms":value}});
                } else {
                    input["connection"][section] = json!({field:value});
                }
                let request: ExecutionRequest = serde_json::from_value(input).unwrap();
                assert_eq!(
                    validate_execution_configuration(&request).is_ok(),
                    value <= MAX_TIMER_MS && (zero_allowed || value != 0),
                    "{section}.{field}: {value}"
                );
            }
        }
    }
}
