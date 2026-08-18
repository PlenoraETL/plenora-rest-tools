use pyo3::{create_exception, exceptions::PyException, prelude::*, types::PyModule};
use rest_engine_core::{Engine, EngineConfig, EngineError};

create_exception!(_native, NativeRestEngineError, PyException);

#[pyclass(name = "NativeEngine")]
struct NativeEngine {
    engine: Engine,
    runtime: tokio::runtime::Runtime,
}

#[pymethods]
impl NativeEngine {
    #[new]
    #[pyo3(signature = (config_json=None))]
    fn new(config_json: Option<&str>) -> PyResult<Self> {
        let config = match config_json {
            Some(value) => serde_json::from_str::<EngineConfig>(value)
                .map_err(|error| to_python_error(EngineError::InvalidInput(error.to_string())))?,
            None => EngineConfig::default(),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| to_python_error(EngineError::Runtime(error.to_string())))?;
        Ok(Self {
            engine: Engine::new(config),
            runtime,
        })
    }

    fn execute(&self, py: Python<'_>, request_json: &str) -> PyResult<String> {
        let request_json = request_json.to_owned();
        py.detach(|| {
            self.runtime
                .block_on(self.engine.execute_json(&request_json))
                .map_err(to_python_error)
        })
    }
}

fn to_python_error(error: EngineError) -> PyErr {
    let payload = serde_json::to_string(&error.payload())
        .unwrap_or_else(|_| format!(r#"{{"code":"runtime_error","message":"{error}"}}"#));
    NativeRestEngineError::new_err(payload)
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeEngine>()?;
    module.add(
        "NativeRestEngineError",
        module.py().get_type::<NativeRestEngineError>(),
    )?;
    module.add("SCHEMA_VERSION", rest_engine_core::SCHEMA_VERSION)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
