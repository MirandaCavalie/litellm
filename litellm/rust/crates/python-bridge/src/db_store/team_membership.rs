use std::sync::Arc;

use litellm_auth_store::{Side, TeamMembershipStore};
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The team membership store. `find` answers a membership with its member budget as JSON text, or `None`.
#[pyclass(frozen)]
pub(crate) struct RustTeamMembershipStore(Arc<TeamMembershipStore>);

#[pymethods]
impl RustTeamMembershipStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(TeamMembershipStore::new(
            database.store_database()?,
        ))))
    }

    #[pyo3(signature = (user_id, team_id, *, use_writer=false))]
    fn find<'py>(
        &self,
        py: Python<'py>,
        user_id: String,
        team_id: String,
        use_writer: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        let side = if use_writer { Side::Writer } else { Side::Read };
        crate::execution::run_async_value(py, async move {
            let membership = store
                .find(&user_id, &team_id, side)
                .await
                .map_err(map_error)?;
            super::json(membership.as_ref())
        })
    }
}
