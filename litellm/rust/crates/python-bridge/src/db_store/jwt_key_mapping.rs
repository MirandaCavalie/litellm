use std::sync::Arc;

use litellm_auth_store::JwtKeyMappingStore;
use pyo3::prelude::*;

use crate::db::{NativeDatabase, map_error};

/// The JWT key mapping store.
#[pyclass(frozen)]
pub(crate) struct RustJwtKeyMappingStore(Arc<JwtKeyMappingStore>);

#[pymethods]
impl RustJwtKeyMappingStore {
    #[new]
    fn new(database: &NativeDatabase) -> PyResult<Self> {
        Ok(Self(Arc::new(JwtKeyMappingStore::new(
            database.store_database()?,
        ))))
    }

    /// The hashed key mapped to the claim in exactly the scope of `jwt_issuer`, or `None`.
    #[pyo3(signature = (jwt_claim_name, jwt_claim_value, jwt_issuer=None))]
    fn mapped_token<'py>(
        &self,
        py: Python<'py>,
        jwt_claim_name: String,
        jwt_claim_value: String,
        jwt_issuer: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        crate::execution::run_async_value(py, async move {
            store
                .mapped_token(&jwt_claim_name, &jwt_claim_value, jwt_issuer.as_deref())
                .await
                .map_err(map_error)
        })
    }

    /// `(jwt_issuer, jwt_claim_name, jwt_claim_value)` of every mapping to any of `tokens`.
    fn mapped_claims<'py>(
        &self,
        py: Python<'py>,
        tokens: Vec<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let store = Arc::clone(&self.0);
        crate::execution::run_async_value(py, async move {
            let claims = store.mapped_claims(&tokens).await.map_err(map_error)?;
            Ok(claims
                .into_iter()
                .map(|claim| {
                    (
                        claim.jwt_issuer,
                        claim.jwt_claim_name,
                        claim.jwt_claim_value,
                    )
                })
                .collect::<Vec<_>>())
        })
    }
}
