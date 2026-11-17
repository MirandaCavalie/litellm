use std::sync::Arc;

use litellm_auth_store::{KeyStore, Side};
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The virtual key store. `find` answers the `combined_view` read for a hashed key as JSON text, or `None`.
#[pyclass(frozen)]
pub(crate) struct RustKeyStore(Arc<KeyStore>);

#[pymethods]
impl RustKeyStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(KeyStore::new(database.store_database()?))))
    }

    #[pyo3(signature = (token, *, use_writer=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        token: String,
        use_writer: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        let side = if use_writer { Side::Writer } else { Side::Read };
        crate::execution::run_async_value(py, async move {
            let view = store.find(&token, side).await.map_err(map_error)?;
            super::json(view.as_ref())
        })
    }
}
