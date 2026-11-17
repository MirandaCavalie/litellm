//! The `spend_log_input` field, typed. The Python derivation
//! (`litellm/proxy/spend_tracking/rust_spend_log_inputs.py`) reads what only Python holds (the
//! response object, the auth object, the router, the settings) and hands over one document per
//! hook: the proxy flags the writer reads, the identity the request is charged to, and each
//! row's builder input and cost.

use serde::Deserialize;
use serde_json::Value;

/// The proxy settings the cost callback and `update_database` read on each request.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub disable_spend_updates: bool,
    pub disable_spend_logs: bool,
    pub disable_error_logs: bool,
    pub disable_responses_id_security: bool,
    /// `spend_log_row_config()`, the row builder's settings.
    pub row_config: Value,
    /// `litellm_proxy_budget_name` when a proxy `max_budget` is set.
    pub proxy_budget_name: Option<String>,
}

/// Who a request is charged to, from the proxy-stamped `user_api_key_*` metadata (success) or
/// the auth object (failure).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Identity {
    /// The key as logged: a hash, or a raw `sk-` key the writer hashes.
    pub token: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
    pub end_user_id: Option<String>,
    pub key_alias: Option<String>,
    pub agent_id: Option<String>,
}

/// One spend log row to write: the row builder's input and what the row charges.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RowRequest {
    pub row: Value,
    pub response_cost: Option<f64>,
    /// `get_request_model_access_groups`, before the router narrows them to the served model.
    pub model_access_groups: Vec<String>,
    /// `customer_hierarchy_ids_from_request(...).entities()`.
    pub customer_entities: Option<serde_json::Map<String, Value>>,
}

/// How the success callback treats a request before any charge.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// The cost is known: gate, then charge.
    #[default]
    Charge,
    /// `defers_batch_cost`: a batch whose cost is not final yet.
    DeferBatch,
    /// An in-progress background interaction the poll task settles later.
    InteractionInProgress,
    /// An interaction create with no usage that no poll task settles.
    InteractionUnbilled,
    /// No cost and nothing to defer.
    Unpriced,
}

/// Key identity enrichment from the key and team cache when the key carries no user.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Enrich {
    pub resolve_missing_key_identity: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Success {
    pub settings: Settings,
    pub identity: Identity,
    pub enrich: Option<Enrich>,
    pub disposition: Disposition,
    pub call_type: Option<String>,
    /// `call_type` is one the proxy tracks with no identity (pass-through, MCP, a batch's cost).
    pub tracks_unattributed: bool,
    pub tags: Option<Vec<String>>,
    pub end_user_max_budget: Option<f64>,
    /// `repeat_poll_cost`: what a poll of a job an earlier poll charged still pays for.
    pub repeat_poll_cost: f64,
    /// The `window_reset_at` of each reservation entry, which the typed reservation lacks.
    pub window_resets: Vec<Option<String>>,
    pub row: RowRequest,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Failure {
    pub settings: Settings,
    pub identity: Identity,
    pub enrich: Option<Enrich>,
    /// The standard logging payload says `cost_finalized`.
    pub completed_cost: bool,
    /// The failure is one the proxy writes rows for (error logs on, a trackable route).
    pub track: bool,
    pub tags: Option<Vec<String>>,
    pub window_resets: Vec<Option<String>>,
    pub rows: Vec<RowRequest>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpendLogInput {
    Success(Box<Success>),
    Failure(Box<Failure>),
}
