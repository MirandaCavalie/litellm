use litellm_callbacks_spend_log::{RowConfig, RowError, SpendLogRowBuilder, finalize_ledger_row};
use litellm_host_python::release_gil;
use litellm_python_compat::json;
use litellm_tracing::TokenRedactor;
use pyo3::{exceptions::PyValueError, prelude::*};

fn row_error(error: RowError) -> PyErr {
    PyValueError::new_err(error.to_string())
}

#[pyclass(frozen, name = "NativeSpendLogRowBuilder")]
pub(crate) struct NativeSpendLogRowBuilder(SpendLogRowBuilder);

#[pymethods]
impl NativeSpendLogRowBuilder {
    #[new]
    fn new(
        settings_json: &str,
        minimum_custom_key_length: usize,
        word_ranges: Vec<(u32, u32)>,
    ) -> PyResult<Self> {
        let settings = json::loads(settings_json).map_err(|error| row_error(error.into()))?;
        let config = RowConfig::from_settings(&settings).map_err(row_error)?;
        Ok(Self(SpendLogRowBuilder::new(
            config,
            TokenRedactor::new(minimum_custom_key_length, word_ranges),
        )))
    }

    fn build_row(&self, py: Python<'_>, input_json: &str) -> PyResult<String> {
        release_gil(py, || self.0.build_row_json(input_json)).map_err(row_error)
    }

    #[staticmethod]
    fn finalize_ledger_row(py: Python<'_>, row_json: &str, input_json: &str) -> PyResult<String> {
        release_gil(py, || -> Result<String, RowError> {
            let row = json::loads(row_json)?;
            let input = json::loads(input_json)?;
            Ok(json::dumps(&finalize_ledger_row(&row, &input)?)?)
        })
        .map_err(row_error)
    }
}
