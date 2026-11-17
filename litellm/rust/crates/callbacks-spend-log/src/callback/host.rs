//! The calls the spend log callback makes back into the Python proxy, for state only Python
//! holds. The alerts and the proxy spend bump are fire and forget: Python schedules them and
//! answers at once. Enrichment and the accounting context are awaited because the row needs them.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use litellm_callbacks::HostCall;

/// `_enrich_failure_metadata_with_key_info`: the key's and team's identity from the auth cache
/// or DB, for a request whose key carried no user.
#[derive(Debug, Serialize)]
pub struct EnrichKeyIdentity {
    pub api_key: String,
    pub resolve_missing_key_identity: bool,
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
    pub team_alias: Option<String>,
}

/// The enriched `user_api_key_*` metadata, each `None` when still unknown.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct EnrichedKeyIdentity {
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
    pub team_alias: Option<String>,
}

impl HostCall for EnrichKeyIdentity {
    const NAME: &'static str = "enrich_key_identity";
    type Reply = EnrichedKeyIdentity;
}

/// The accounting inputs that need the auth cache, the DB and the pricing map: the budget
/// windows of the key, team, user and customer (`accounting_window_keys`), the savings priced
/// from the built row (`_savings_or_zero`), and the model access groups the served model debits.
#[derive(Debug, Serialize)]
pub struct AccountingContext {
    /// The built row, as JSON text.
    pub row: String,
    pub token: Option<String>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub windows: bool,
    pub customer: Option<Map<String, Value>>,
    pub customer_scopes: Option<Vec<Value>>,
    pub model_access_groups: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Accounting {
    pub windows: Vec<Value>,
    pub savings: Value,
    pub model_access_groups: Vec<String>,
}

impl HostCall for AccountingContext {
    const NAME: &'static str = "accounting_context";
    type Reply = Accounting;
}

/// `_add_spend_to_global_proxy_spend`, which the proxy `max_budget` check reads.
#[derive(Debug, Serialize)]
pub struct IncrementProxySpend {
    pub response_cost: f64,
}

impl HostCall for IncrementProxySpend {
    const NAME: &'static str = "increment_proxy_spend";
    type Reply = ();
}

/// The cached key's projected soft budget alert from `update_cache`.
#[derive(Debug, Serialize)]
pub struct SoftBudgetAlert {
    pub token: String,
    pub response_cost: f64,
}

impl HostCall for SoftBudgetAlert {
    const NAME: &'static str = "soft_budget_alert";
    type Reply = ();
}

/// `slack_alerting_instance.customer_spend_alert`.
#[derive(Debug, Serialize)]
pub struct CustomerSpendAlert {
    pub token: Option<String>,
    pub key_alias: Option<String>,
    pub end_user_id: Option<String>,
    pub response_cost: f64,
    pub max_budget: Option<f64>,
}

impl HostCall for CustomerSpendAlert {
    const NAME: &'static str = "customer_spend_alert";
    type Reply = ();
}
