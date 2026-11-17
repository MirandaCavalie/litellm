use std::sync::Arc;

use litellm_auth_store::{Side, TeamStore};
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The team store. `find` answers a team with its model aliases as JSON text, or `None`; `find_by_alias` answers
/// up to two teams sharing an alias, as JSON text each.
#[pyclass(frozen)]
pub(crate) struct RustTeamStore(Arc<TeamStore>);

#[pymethods]
impl RustTeamStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(TeamStore::new(database.store_database()?))))
    }

    #[pyo3(signature = (team_id, *, use_writer=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        team_id: String,
        use_writer: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        let side = if use_writer { Side::Writer } else { Side::Read };
        crate::execution::run_async_value(py, async move {
            let team = store.find(&team_id, side).await.map_err(map_error)?;
            super::json(team.as_ref())
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
