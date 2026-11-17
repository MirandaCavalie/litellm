use std::sync::Arc;

use litellm_auth_store::{AccessGroupStore, Side};
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The access group store. `find` answers an access group as JSON text, or `None`.
#[pyclass(frozen)]
pub(crate) struct RustAccessGroupStore(Arc<AccessGroupStore>);

#[pymethods]
impl RustAccessGroupStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(AccessGroupStore::new(
            database.store_database()?,
        ))))
    }

    #[pyo3(signature = (access_group_id, *, use_writer=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        access_group_id: String,
        use_writer: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        let side = if use_writer { Side::Writer } else { Side::Read };
        crate::execution::run_async_value(py, async move {
            let group = store
                .find(&access_group_id, side)
                .await
                .map_err(map_error)?;
            super::json(group.as_ref())
        })
    }
}
