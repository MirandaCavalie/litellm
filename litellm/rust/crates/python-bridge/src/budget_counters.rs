//! The post-call spend counter fan-out from `litellm-callbacks-budgets`, exposed as plain functions
//! that take and return Python dicts and lists. The proxy validates each reply into typed models.

use litellm_callbacks_budgets::{
    Charge, CustomerScopeSpend, WindowedEntity, committed_counter_keys, customer_settlement, plan,
    post_call_counter_keys, window_counters,
};
use litellm_host_python::{from_py_argument, to_py};
use pyo3::prelude::*;

#[pyfunction]
pub(crate) fn spend_counter_plan(
    py: Python<'_>,
    #[pyo3(from_py_with = from_py_argument)] charge: Charge,
) -> PyResult<Py<PyAny>> {
    to_py(py, &plan(&charge))
}

#[pyfunction]
pub(crate) fn spend_counter_keys(
    py: Python<'_>,
    #[pyo3(from_py_with = from_py_argument)] charge: Charge,
) -> PyResult<Py<PyAny>> {
    to_py(py, &post_call_counter_keys(&charge))
}

#[pyfunction]
pub(crate) fn committed_spend_counter_keys(
    tracked: Vec<String>,
    response_cost: Option<f64>,
) -> Vec<String> {
    committed_counter_keys(&tracked, response_cost)
}

#[pyfunction]
pub(crate) fn budget_window_counters(
    py: Python<'_>,
    #[pyo3(from_py_with = from_py_argument)] entity_type: WindowedEntity,
    entity_id: &str,
    durations: Vec<String>,
    reserved_counter_keys: Vec<String>,
) -> PyResult<Py<PyAny>> {
    to_py(
        py,
        &window_counters(entity_type, entity_id, &durations, &reserved_counter_keys),
    )
}

#[pyfunction]
pub(crate) fn customer_spend_settlement(
    py: Python<'_>,
    #[pyo3(from_py_with = from_py_argument)] scopes: Vec<CustomerScopeSpend>,
    readings: Vec<Option<f64>>,
    cost: f64,
) -> PyResult<Py<PyAny>> {
    to_py(py, &customer_settlement(&scopes, &readings, cost))
}
