use std::sync::Arc;

use litellm_auth_store::ProjectStore;
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The project store. `find` answers a project with its budget as JSON text, or `None`.
#[pyclass(frozen)]
pub(crate) struct RustProjectStore(Arc<ProjectStore>);

#[pymethods]
impl RustProjectStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(ProjectStore::new(
            database.store_database()?,
        ))))
    }

    fn find<'py>(&self, py: Python<'py>, project_id: String) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        crate::execution::run_async_value(py, async move {
            let project = store.find(&project_id).await.map_err(map_error)?;
            super::json(project.as_ref())
        })
    }
}
