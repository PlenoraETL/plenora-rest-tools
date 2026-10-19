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

#[test]
fn capabilities_match_the_adopted_catalog_without_claiming_optional_cli() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!(
        "../../../contracts/upstream/catalogs/rest-tools-v1.json"
    ))
    .unwrap();
    let capabilities = serde_json::to_value(capabilities()).unwrap();
    let actual = capabilities["operations"].as_array().unwrap();
    let target = catalog["operations"].as_array().unwrap();
    assert_eq!(actual.len(), target.len());
    for expected in target {
        let operation = actual
            .iter()
            .find(|operation| {
                operation["id"] == expected["id"] && operation["version"] == expected["version"]
            })
            .unwrap();
        for field in ["side_effect", "controls"] {
            assert_eq!(operation[field], expected[field]);
        }
        for direction in ["input", "output"] {
            for field in ["contract", "content_types"] {
                assert_eq!(operation[direction][field], expected[direction][field]);
            }
        }
        let surfaces = operation["surfaces"].as_array().unwrap();
        for surface in expected["surfaces"].as_array().unwrap() {
            if catalog["target_surfaces"][surface.as_str().unwrap()] == "required" {
                assert!(surfaces.contains(surface));
            }
        }
        assert!(!surfaces.contains(&serde_json::json!("cli")));
        assert!(
            operation["attributes"]
                .get("plenora.surface_side_effects")
                .is_none()
        );
    }
}
