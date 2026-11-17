//! Rust stores for the auth objects the proxy reads from Postgres when its auth cache misses.
//!
//! Each store owns its queries, the shaping of their rows into what the auth flow consumes, and any memory
//! tier private to that object. The shared auth cache in front of them stays with the host.

mod access_group;
mod database;
mod jwt_key_mapping;
mod key;
mod model_access_group;
mod object_permission;
mod project;
mod tag;

pub use access_group::AccessGroupStore;
pub use database::{Database, Side};
pub use jwt_key_mapping::JwtKeyMappingStore;
pub use key::{KeyStore, KeyView};
pub use model_access_group::{ModelAccessGroupBudget, ModelAccessGroupStore};
pub use object_permission::ObjectPermissionStore;
pub use project::ProjectStore;
pub use tag::TagStore;
