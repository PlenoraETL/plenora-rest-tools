//! Validated enrichment plan, shared record transformation and three executors.
use std::num::NonZeroUsize;

use super::*;

#[derive(Clone, Copy)]
enum Executor<'a> {
    Serial,
    Concurrent(NonZeroUsize),
    Batch(&'a BatchConfig, NonZeroUsize),
}

/// Construction validates scheduling once; resolving record values stays lazy
/// so a fail-fast serial execution never touches a later record.
struct RecordPlan<'a> {
    records: &'a [JsonObject],
    params: &'a JsonObject,
    options: &'a crate::ExecutionOptions,
    connection: Cow<'a, ConnectionConfig>,
    executor: Executor<'a>,
}

impl<'a> RecordPlan<'a> {
    fn new(request: &'a ExecutionRequest) -> Result<Self, EngineError> {
        let mut connection = Cow::Borrowed(&request.connection);
        let executor = if let Some(batch) = request.connection.batch.as_ref().filter(|b| b.enabled)
        {
            let size = NonZeroUsize::new(batch.max_size).ok_or_else(|| {
                EngineError::InvalidInput("batch max_size must be greater than zero".into())
            })?;
            let connection = connection.to_mut();
            connection.batch = None;
            connection.pagination = None;
            if let Some(url) = &batch.endpoint_override {
                connection.url = url.clone();
            }
            if let Some(method) = &batch.method_override {
                connection.method = method.clone();
            }
            Executor::Batch(batch, size)
        } else {
            let concurrency = NonZeroUsize::new(request.options.enrichment_concurrency)
                .ok_or_else(|| {
                    EngineError::InvalidInput(
                        "enrichment_concurrency must be greater than zero".into(),
                    )
                })?;
            if concurrency.get() > 1 && request.options.continue_on_error {
                Executor::Concurrent(concurrency)
            } else {
                Executor::Serial
            }
        };
        Ok(Self {
            records: &request.input.records,
            params: &request.input.params,
            options: &request.options,
            connection,
            executor,
        })
    }

    fn resolve(&self, record: &JsonObject) -> Result<JsonObject, EngineError> {
        let mut source = self.params.clone();
        source.extend(record.clone());
        let parameters = resolve_parameters(&self.connection, &source)?;
        if let Executor::Batch(batch, _) = self.executor
            && batch.input_format == BatchInputFormat::FlatArray
        {
            flat_batch_item(&parameters)?;
        }
        Ok(parameters)
    }
}

struct RecordOutcome {
    index: usize,
    rows: Vec<JsonObject>,
    error: Option<ExecutionError>,
    succeeded: usize,
}

/// The only transformation from a record result to public rows/error/count.
/// A shared batch failure can be borrowed for each affected input record.
fn complete_record(
    index: usize,
    record: &JsonObject,
    result: Result<Vec<JsonObject>, &EngineError>,
) -> RecordOutcome {
    let (rows, error, succeeded) = match result {
        Ok(additions) if additions.is_empty() => (vec![record.clone()], None, 1),
        Ok(additions) => {
            let succeeded = additions.len();
            let rows = additions
                .into_iter()
                .map(|additions| {
                    let mut enriched = record.clone();
                    enriched.extend(additions);
                    enriched
                })
                .collect();
            (rows, None, succeeded)
        }
        Err(error) => (
            vec![record.clone()],
            Some(error.execution_error(Some(index))),
            0,
        ),
    };
    RecordOutcome {
        index,
        rows,
        error,
        succeeded,
    }
}

#[derive(Default)]
struct RecordOutput {
    rows: Vec<JsonObject>,
    errors: Vec<ExecutionError>,
    succeeded: usize,
}

impl RecordOutput {
    fn push(&mut self, outcome: RecordOutcome) {
        self.rows.extend(outcome.rows);
        self.errors.extend(outcome.error);
        self.succeeded = self.succeeded.saturating_add(outcome.succeeded);
    }

    fn finish(self) -> OperationResult {
        OperationResult {
            output: ExecutionOutput::Records { records: self.rows },
            errors: self.errors,
            succeeded: self.succeeded,
        }
    }
}

pub(super) async fn execute(
    engine: &Engine,
    request: &ExecutionRequest,
    metrics: &mut ExecutionMetrics,
    responses: &mut Vec<HttpResponseMetadata>,
) -> Result<OperationResult, EngineError> {
    let plan = RecordPlan::new(request)?;
    match plan.executor {
        Executor::Serial => execute_serial(engine, &plan, metrics, responses).await,
        Executor::Concurrent(concurrency) => {
            execute_concurrent(engine, &plan, concurrency, metrics, responses).await
        }
        Executor::Batch(batch, size) => {
            execute_batch(engine, &plan, batch, size, metrics, responses).await
        }
    }
}

async fn request_record(
    engine: &Engine,
    plan: &RecordPlan<'_>,
    index: usize,
    record: &JsonObject,
    metrics: &mut ExecutionMetrics,
    responses: &mut Vec<HttpResponseMetadata>,
) -> RecordOutcome {
    let options = scoped_execution_options(plan.options, index);
    let result = RECORD_SCOPE
        .scope(Arc::from([index]), async {
            let parameters = plan.resolve(record)?;
            let (value, _, _, _) = engine
                .request_json(
                    &plan.connection,
                    &parameters,
                    None,
                    metrics,
                    responses,
                    &options,
                )
                .await?;
            map_records(
                response_records(&plan.connection, &value)?,
                &plan.connection.response,
            )
        })
        .await;
    match result {
        Ok(additions) => complete_record(index, record, Ok(additions)),
        Err(error) => complete_record(index, record, Err(&error)),
    }
}

async fn execute_serial(
    engine: &Engine,
    plan: &RecordPlan<'_>,
    metrics: &mut ExecutionMetrics,
    responses: &mut Vec<HttpResponseMetadata>,
) -> Result<OperationResult, EngineError> {
    let mut output = RecordOutput::default();
    for (index, record) in plan.records.iter().enumerate() {
        let outcome = request_record(engine, plan, index, record, metrics, responses).await;
        let failed = outcome.error.is_some();
        output.push(outcome);
        if failed && !plan.options.continue_on_error {
            break;
        }
    }
    Ok(output.finish())
}

struct ConcurrentOutcome {
    record: RecordOutcome,
    metrics: ExecutionMetrics,
    responses: Vec<HttpResponseMetadata>,
}

async fn execute_concurrent(
    engine: &Engine,
    plan: &RecordPlan<'_>,
    concurrency: NonZeroUsize,
    metrics: &mut ExecutionMetrics,
    responses: &mut Vec<HttpResponseMetadata>,
) -> Result<OperationResult, EngineError> {
    let outcomes = stream::iter(plan.records.iter().cloned().enumerate().map(
        |(index, record)| async move {
            let mut local_metrics = ExecutionMetrics::default();
            let mut local_responses = Vec::new();
            let record = request_record(
                engine,
                plan,
                index,
                &record,
                &mut local_metrics,
                &mut local_responses,
            )
            .await;
            ConcurrentOutcome {
                record,
                metrics: local_metrics,
                responses: local_responses,
            }
        },
    ))
    .buffer_unordered(concurrency.get())
    .collect::<Vec<_>>()
    .await;
    // Refuse a missing, duplicate or out-of-range result instead of dropping rows.
    let mut ordered: Vec<Option<ConcurrentOutcome>> =
        (0..plan.records.len()).map(|_| None).collect();
    for outcome in outcomes {
        let slot = ordered.get_mut(outcome.record.index).ok_or_else(|| {
            EngineError::Runtime("enrichment outcome index is out of range".into())
        })?;
        if slot.replace(outcome).is_some() {
            return Err(EngineError::Runtime(
                "enrichment record produced more than one outcome".into(),
            ));
        }
    }
    let ordered = ordered
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| EngineError::Runtime("enrichment record produced no outcome".into()))?;
    let mut output = RecordOutput::default();
    for outcome in ordered {
        merge_execution_metrics(metrics, &outcome.metrics);
        responses.extend(outcome.responses);
        output.push(outcome.record);
    }
    Ok(output.finish())
}

async fn execute_batch(
    engine: &Engine,
    plan: &RecordPlan<'_>,
    batch: &BatchConfig,
    size: NonZeroUsize,
    metrics: &mut ExecutionMetrics,
    responses: &mut Vec<HttpResponseMetadata>,
) -> Result<OperationResult, EngineError> {
    let mut output = RecordOutput::default();
    for (chunk_index, records) in plan.records.chunks(size.get()).enumerate() {
        let options = scoped_execution_options(plan.options, chunk_index);
        let base = chunk_index.saturating_mul(size.get());
        let mut parameters = Vec::new();
        let mut valid = Vec::new();
        let mut resolution_errors = BTreeMap::new();
        for (offset, record) in records.iter().enumerate() {
            match plan.resolve(record) {
                Ok(value) => {
                    parameters.push(value);
                    valid.push(base + offset);
                }
                Err(error) => {
                    resolution_errors.insert(offset, error);
                }
            }
        }
        let result = if parameters.is_empty() {
            Ok(Vec::new())
        } else {
            let payload = batch_payload(batch, parameters);
            RECORD_SCOPE
                .scope(
                    Arc::from(valid.clone()),
                    engine.request_json(
                        &plan.connection,
                        &payload,
                        None,
                        metrics,
                        responses,
                        &options,
                    ),
                )
                .await
                .and_then(|(value, _, _, _)| {
                    batch_response(&value, batch, &plan.connection.response)
                })
        }
        .and_then(|mapped| {
            if mapped.len() == valid.len() {
                Ok(mapped)
            } else {
                Err(EngineError::InvalidResponse(
                    "batch returned a different number of results than input records".into(),
                ))
            }
        });
        let (mut mapped, batch_error) = match result {
            Ok(mapped) => (mapped.into_iter(), None),
            Err(error) => (Vec::new().into_iter(), Some(error)),
        };
        let mut chunk = RecordOutput::default();
        let mut batch_errors = Vec::new();
        for (offset, record) in records.iter().enumerate() {
            let resolution_error = resolution_errors.get(&offset);
            let result = match resolution_error.or(batch_error.as_ref()) {
                Some(error) => Err(error),
                None => Ok(vec![mapped.next().ok_or_else(|| {
                    EngineError::Runtime("validated batch result is missing a record".into())
                })?]),
            };
            let mut outcome = complete_record(base + offset, record, result);
            // Frozen batch order: shared request errors precede resolution errors,
            // independently of row order. The record transformation is unchanged.
            if resolution_error.is_none() && batch_error.is_some() {
                batch_errors.extend(outcome.error.take());
            }
            chunk.push(outcome);
        }
        output.rows.extend(chunk.rows);
        output.errors.extend(batch_errors);
        output.errors.extend(chunk.errors);
        output.succeeded = output.succeeded.saturating_add(chunk.succeeded);
        if !plan.options.continue_on_error && !output.errors.is_empty() {
            break;
        }
    }
    Ok(output.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(options: Value, batch: Value) -> ExecutionRequest {
        serde_json::from_value(json!({"schema_version":1,"operation":"enrich",
            "connection":{"url":"https://example.com", "batch":batch}, "options":options}))
        .unwrap()
    }

    #[test]
    fn plan_validation_preserves_dispatch_precedence() {
        let serial = request(
            json!({"enrichment_concurrency":7,"continue_on_error":false}),
            Value::Null,
        );
        assert!(matches!(
            RecordPlan::new(&serial).unwrap().executor,
            Executor::Serial
        ));
        let concurrent = request(
            json!({"enrichment_concurrency":7,"continue_on_error":true}),
            Value::Null,
        );
        assert!(matches!(
            RecordPlan::new(&concurrent).unwrap().executor,
            Executor::Concurrent(_)
        ));
        let invalid = request(json!({"enrichment_concurrency":0}), Value::Null);
        assert!(matches!(
            RecordPlan::new(&invalid),
            Err(EngineError::InvalidInput(_))
        ));
        let batch = request(
            json!({"enrichment_concurrency":0}),
            json!({"enabled":true,"max_size":2}),
        );
        let plan = RecordPlan::new(&batch).unwrap();
        assert!(matches!(plan.executor, Executor::Batch(_, _)));
        assert!(plan.connection.batch.is_none());
        let invalid_batch = request(json!({}), json!({"enabled":true,"max_size":0}));
        assert!(matches!(
            RecordPlan::new(&invalid_batch),
            Err(EngineError::InvalidInput(_))
        ));
    }

    #[test]
    fn record_outcomes_preserve_empty_expanded_and_failed_rows() {
        let record: JsonObject = serde_json::from_value(json!({"id":7,"name":"local"})).unwrap();
        let empty = complete_record(3, &record, Ok(Vec::new()));
        assert_eq!(empty.rows, vec![record.clone()]);
        assert_eq!(empty.succeeded, 1);
        let additions = serde_json::from_value(json!([{"name":"remote"},{"extra":2}])).unwrap();
        let expanded = complete_record(3, &record, Ok(additions));
        assert_eq!(
            serde_json::to_value(expanded.rows).unwrap(),
            json!([{"id":7,"name":"remote"},{"id":7,"name":"local","extra":2}])
        );
        assert_eq!(expanded.succeeded, 2);
        let failed = complete_record(3, &record, Err(&EngineError::HttpStatus { status: 400 }));
        assert_eq!(failed.rows, vec![record]);
        assert_eq!(failed.succeeded, 0);
        assert_eq!(failed.error.unwrap().input_index, Some(3));
    }
}
