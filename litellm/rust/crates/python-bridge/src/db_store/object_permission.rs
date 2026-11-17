use std::sync::Arc;

use litellm_auth_store::{ObjectPermissionStore, Side};
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The object permission store. `find` answers an object permission as JSON text, or `None`.
#[pyclass(frozen)]
pub(crate) struct RustObjectPermissionStore(Arc<ObjectPermissionStore>);

#[pymethods]
impl RustObjectPermissionStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(ObjectPermissionStore::new(
            database.store_database()?,
        ))))
    }

    #[pyo3(signature = (object_permission_id, *, use_writer=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        object_permission_id: String,
        use_writer: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        let side = if use_writer { Side::Writer } else { Side::Read };
        crate::execution::run_async_value(py, async move {
            let permission = store
                .find(&object_permission_id, side)
                .await
                .map_err(map_error)?;
            super::json(permission.as_ref())
        })
    }
}
