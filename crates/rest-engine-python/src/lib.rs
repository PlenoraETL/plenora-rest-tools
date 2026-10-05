use plenora_rest_core::{
    CancellationToken, CookieSession, Engine, EngineConfig, EngineError, capabilities,
};
use pyo3::{create_exception, exceptions::PyException, prelude::*, types::PyModule};

create_exception!(_native, NativePlenoraError, PyException);

#[pyclass(name = "NativeCancellationToken")]
struct NativeCancellationToken {
    token: CancellationToken,
}

#[pymethods]
impl NativeCancellationToken {
    #[new]
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    fn cancel(&self) {
        self.token.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

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
            Some(value) => serde_json::from_str::<EngineConfig>(value).map_err(|_| {
                to_python_error(EngineError::InvalidInput(
                    "engine configuration is not valid".into(),
                ))
            })?,
            None => EngineConfig::default(),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                to_python_error(EngineError::Runtime("runtime could not be started".into()))
            })?;
        Ok(Self {
            engine: Engine::new(config),
            runtime,
        })
    }

    #[pyo3(signature = (request_json, cancellation=None))]
    fn execute(
        &self,
        py: Python<'_>,
        request_json: &str,
        cancellation: Option<PyRef<'_, NativeCancellationToken>>,
    ) -> PyResult<String> {
        let request_json = request_json.to_owned();
        let cancellation = cancellation
            .map(|token| token.token.clone())
            .unwrap_or_default();
        py.detach(|| {
            self.runtime
                .block_on(
                    self.engine
                        .execute_json_with_cancellation(&request_json, cancellation),
                )
                .map_err(to_python_error)
        })
    }

    fn capabilities(&self) -> PyResult<String> {
        serde_json::to_string(&capabilities()).map_err(|_| {
            to_python_error(EngineError::Runtime(
                "capabilities could not be serialized".into(),
            ))
        })
    }

    /// Opens a cookie session and returns its opaque handle.
    fn open_cookie_session(&self, py: Python<'_>) -> PyResult<String> {
        py.detach(|| {
            self.runtime
                .block_on(self.engine.open_cookie_session())
                .map(|session| session.to_token())
                .map_err(to_python_error)
        })
    }

    /// Closes the cookie session `handle` names.
    fn close_cookie_session(&self, py: Python<'_>, handle: &str) -> PyResult<()> {
        let session = CookieSession::from_token(handle).ok_or_else(|| {
            to_python_error(EngineError::InvalidInput(
                "cookie session handle is not well formed".into(),
            ))
        })?;
        py.detach(|| {
            self.runtime
                .block_on(self.engine.close_cookie_session(&session))
                .map_err(to_python_error)
        })
    }

    fn close(&self) {
        self.engine.close();
    }

    fn is_closed(&self) -> bool {
        self.engine.is_closed()
    }
}

fn to_python_error(error: EngineError) -> PyErr {
    let payload = serde_json::to_string(&error.payload()).unwrap_or_else(|_| {
        r#"{"category":"internal","phase":"cleanup","remote_effect":"none","retry":{"kind":"never"},"code":"RUNTIME_ERROR","message":"REST engine failed internally","details":{}}"#
            .to_owned()
    });
    NativePlenoraError::new_err(payload)
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeEngine>()?;
    module.add_class::<NativeCancellationToken>()?;
    module.add(
        "NativePlenoraError",
        module.py().get_type::<NativePlenoraError>(),
    )?;
    module.add("SCHEMA_VERSION", plenora_rest_core::SCHEMA_VERSION)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
