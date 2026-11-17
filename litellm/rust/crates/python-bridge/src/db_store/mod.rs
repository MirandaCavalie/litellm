//! Python handles on the Rust auth stores. Each wraps one store built over a `NativeDatabase`'s pools and
//! hands its answers to Python as plain values or JSON text for `model_validate_json`.

mod jwt_key_mapping;
mod key;
mod model_access_group;
mod organization;
mod team;
mod team_membership;

pub(crate) use jwt_key_mapping::RustJwtKeyMappingStore;
pub(crate) use key::RustKeyStore;
pub(crate) use model_access_group::RustModelAccessGroupStore;
pub(crate) use organization::RustOrganizationStore;
pub(crate) use team::RustTeamStore;
pub(crate) use team_membership::RustTeamMembershipStore;

fn json<T: serde::Serialize>(value: Option<&T>) -> pyo3::PyResult<Option<String>> {
    value.map(litellm_host_python::json_dumps).transpose()
}

fn json_list<T: serde::Serialize>(values: &[T]) -> pyo3::PyResult<Vec<String>> {
    values.iter().map(litellm_host_python::json_dumps).collect()
}
