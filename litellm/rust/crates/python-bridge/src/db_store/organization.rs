use std::sync::Arc;

use litellm_auth_store::OrganizationStore;
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The organization store. `find` answers an organization, with its budget on request, as JSON text, or
/// `None`; `find_by_alias` answers up to two organizations sharing an alias, without budgets, as JSON text each.
#[pyclass(frozen)]
pub(crate) struct RustOrganizationStore(Arc<OrganizationStore>);

#[pymethods]
impl RustOrganizationStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(OrganizationStore::new(
            database.store_database()?,
        ))))
    }

    #[pyo3(signature = (organization_id, *, include_budget_table=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        organization_id: String,
        include_budget_table: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        crate::execution::run_async_value(py, async move {
            let organization = store
                .find(&organization_id, include_budget_table)
                .await
                .map_err(map_error)?;
            super::json(organization.as_ref())
        })
    }

    fn find_by_alias<'py>(&self, py: Python<'py>, alias: String) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        crate::execution::run_async_value(py, async move {
            let found = store.find_by_alias(&alias).await.map_err(map_error)?;
            super::json_list(&found)
        })
    }
}
