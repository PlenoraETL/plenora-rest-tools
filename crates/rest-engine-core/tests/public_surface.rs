use plenora_rest_core::{
    CancellationToken, Engine, EngineConfig, ExecutionControl, ExecutionOperation,
    ExecutionRequest, REST_DOWNLOAD, REST_ENRICH, REST_GENERATE, REST_TEST, REST_UPLOAD,
    RuntimeBinding, capabilities,
};

#[test]
fn documented_rust_exports_map_every_catalog_operation() {
    let operation_ids = capabilities()
        .operations
        .into_iter()
        .map(|operation| operation.id)
        .collect::<Vec<_>>();
    assert_eq!(
        operation_ids,
        [
            REST_TEST,
            REST_GENERATE,
            REST_ENRICH,
            REST_DOWNLOAD,
            REST_UPLOAD,
        ]
    );

    let _engine_constructor: fn(EngineConfig) -> Engine = Engine::new;
    let _request_type: Option<ExecutionRequest> = None;
    let _operation_type: Option<ExecutionOperation> = None;
    let _control_type: Option<ExecutionControl> = None;
    let _cancellation_type: Option<CancellationToken> = None;
    let _runtime_type = std::any::type_name::<RuntimeBinding<'static, 'static, NoResources>>();
}

struct NoResources;

impl plenora_rest_core::RuntimeResources for NoResources {
    fn resolve_credentials(
        &self,
        _reference: &str,
    ) -> Result<plenora_rest_core::AuthConfig, plenora_rest_core::EngineError> {
        Ok(plenora_rest_core::AuthConfig::None)
    }

    fn resolve_artifact_source(
        &self,
        _reference: &str,
    ) -> Result<std::path::PathBuf, plenora_rest_core::EngineError> {
        unreachable!()
    }

    fn resolve_artifact_sink(
        &self,
        _reference: &str,
    ) -> Result<std::path::PathBuf, plenora_rest_core::EngineError> {
        unreachable!()
    }
}
