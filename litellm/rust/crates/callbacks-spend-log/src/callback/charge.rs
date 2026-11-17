//! The success and failure flows and `update_database`'s ledger half: gate, reconcile, build,
//! claim or enqueue, then the counters and the host calls, in the cost callback's order.

use litellm_callbacks::{BudgetReservation, Host};
use litellm_callbacks_budgets::post_call::{self, PostCallCharge};
use litellm_python_compat::{
    Value,
    dict::{Dict, get, get_or_none},
    json,
};
use litellm_spend_log_writer::{LedgerStore, SpendLogRow, jsonified, retain_metadata};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};

use super::{
    SpendLogCallback,
    hook_input::{Disposition, Enrich, Failure, Identity, RowRequest, Settings, Success},
    host::{
        AccountingContext, CustomerSpendAlert, EnrichKeyIdentity, EnrichedKeyIdentity,
        IncrementProxySpend, SoftBudgetAlert,
    },
};
use crate::finalize_ledger_row;

const ACCOUNTING_ONLY_KEYS: [&str; 4] = [
    "request_id",
    "call_type",
    "accounting_payload",
    "accounting_delivery_id",
];
const VIDEO_STATUS_CALL_TYPES: [&str; 2] = ["avideo_status", "video_status"];
const RESPONSES_ID_CALL_TYPES: [&str; 2] = ["aget_responses", "acancel_responses"];

/// What `update_database` did with one row: whether the request is charged, and the model
/// access groups the served model debits, which the counters reuse.
struct Written {
    charged: bool,
    model_access_groups: Vec<String>,
}

impl Written {
    const fn charged() -> Self {
        Self {
            charged: true,
            model_access_groups: Vec::new(),
        }
    }
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `hash_token` for a raw `sk-` key; anything else is already the logged form.
fn hashed_token(token: Option<&str>) -> Option<String> {
    token.map(|token| {
        if token.starts_with("sk-") {
            sha256_hex(token)
        } else {
            token.to_owned()
        }
    })
}

/// `_persistable_api_key`: a key the row did not log as is, and that is no hash, is stored
/// hashed with any bearer prefix dropped.
fn persistable_api_key(token: Option<&str>, logged: &str) -> Option<String> {
    let token = token?;
    if token == logged || is_sha256_hex(token) {
        return Some(token.to_owned());
    }
    let stripped = token
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .map_or(token, |_| &token[7..]);
    (!stripped.is_empty()).then(|| sha256_hex(stripped))
}

fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    match get(row, key) {
        Some(Value::Str(value)) => Some(value),
        _ => None,
    }
}

fn settles_once(row: &Value) -> bool {
    if text(row, "status") != Some("success") {
        return false;
    }
    let request_id = text(row, "request_id").unwrap_or_default();
    match text(row, "call_type") {
        Some("aretrieve_batch") => request_id.ends_with("_batch_cost"),
        Some(kind) if VIDEO_STATUS_CALL_TYPES.contains(&kind) => {
            request_id.ends_with("_video_cost")
        }
        _ => false,
    }
}

fn spend(row: &Value) -> f64 {
    match get_or_none(row, "spend") {
        Value::Float(value) => *value,
        _ => 0.0,
    }
}

fn accounting_only(row: &Value) -> Value {
    let mut kept = Dict::default();
    for key in ACCOUNTING_ONLY_KEYS {
        if let Some(value) = get(row, key) {
            kept.set(key, value.clone());
        }
    }
    kept.set("accounting_log_disabled", Value::Bool(true));
    kept.into_value()
}

fn with_entry(row: &Value, key: &str, value: Value) -> Value {
    let Value::Dict(entries) = row else {
        return row.clone();
    };
    let mut dict = Dict::from_entries(entries);
    dict.set(key, value);
    dict.into_value()
}

fn queued_row(row: Value) -> Option<SpendLogRow> {
    let Value::Dict(entries) = row else {
        return None;
    };
    Some(SpendLogRow::new(
        entries
            .into_iter()
            .filter_map(|(key, value)| match key {
                Value::Str(key) => Some((key, value)),
                _ => None,
            })
            .collect(),
    ))
}

/// `_write_spend_metadata_to_kwargs` on the row input: the enriched `user_api_key_*` values fill
/// the metadata the row reads wherever it holds none.
fn enrich_row(row: &mut RowRequest, enriched: &EnrichedKeyIdentity) {
    let Some(metadata) = row.row.get_mut("metadata").and_then(Json::as_object_mut) else {
        return;
    };
    for (key, value) in [
        ("user_api_key_alias", &enriched.key_alias),
        ("user_api_key_user_id", &enriched.user_id),
        ("user_api_key_team_id", &enriched.team_id),
        ("user_api_key_org_id", &enriched.org_id),
        ("user_api_key_project_id", &enriched.project_id),
        ("user_api_key_team_alias", &enriched.team_alias),
    ] {
        let Some(value) = value else { continue };
        if metadata.get(key).is_none_or(Json::is_null) {
            metadata.insert(key.to_owned(), Json::String(value.clone()));
        }
    }
}

fn fill(slot: &mut Option<String>, value: &Option<String>) {
    if slot.is_none() {
        slot.clone_from(value);
    }
}

/// `_charge_request_cost`'s second charge: the row of a repeat poll carries only what the poll
/// still pays for.
fn repriced(row: &RowRequest, cost: f64) -> RowRequest {
    let mut repriced = row.clone();
    repriced.response_cost = Some(cost);
    for key in ["kwargs", "standard_logging_object"] {
        if let Some(section) = repriced.row.get_mut(key).and_then(Json::as_object_mut)
            && section.contains_key("response_cost")
        {
            section.insert("response_cost".to_owned(), json!(cost));
        }
    }
    repriced
}

impl<S: LedgerStore + 'static> SpendLogCallback<S> {
    async fn enrich(
        &self,
        host: &Host,
        identity: &Identity,
        enrich: &Enrich,
    ) -> Option<EnrichedKeyIdentity> {
        let api_key = identity.token.clone()?;
        match host
            .call(EnrichKeyIdentity {
                api_key,
                resolve_missing_key_identity: enrich.resolve_missing_key_identity,
                key_alias: identity.key_alias.clone(),
                user_id: identity.user_id.clone(),
                team_id: identity.team_id.clone(),
                org_id: identity.org_id.clone(),
                project_id: identity.project_id.clone(),
                team_alias: None,
            })
            .await
        {
            Ok(enriched) => Some(enriched),
            Err(error) => {
                tracing::debug!(%error, "Failed to enrich spend log metadata with key info");
                None
            }
        }
    }

    async fn release(
        &self,
        reservation: &mut Option<BudgetReservation>,
        window_resets: &[Option<String>],
    ) {
        if let Some(reservation) = reservation.as_mut() {
            post_call::release_or_invalidate(&self.budgets, reservation, window_resets).await;
        }
    }

    pub(super) async fn on_success(
        &self,
        success: Success,
        reservation: Option<BudgetReservation>,
        host: &Host,
    ) {
        let mut success = success;
        let mut reservation = reservation;
        if let Some(enrich) = success.enrich.clone()
            && let Some(enriched) = self.enrich(host, &success.identity, &enrich).await
        {
            enrich_row(&mut success.row, &enriched);
            let identity = &mut success.identity;
            fill(&mut identity.key_alias, &enriched.key_alias);
            fill(&mut identity.user_id, &enriched.user_id);
            fill(&mut identity.team_id, &enriched.team_id);
            fill(&mut identity.org_id, &enriched.org_id);
            fill(&mut identity.project_id, &enriched.project_id);
        }
        let resets = success.window_resets.clone();
        match (success.disposition, success.row.response_cost) {
            (Disposition::InteractionInProgress, _) => {}
            (Disposition::Charge, Some(cost)) => {
                if !tracked(&success) {
                    self.release(&mut reservation, &resets).await;
                    return;
                }
                if let Some(charged) = self
                    .charge_request(&success, cost, &mut reservation, host)
                    .await
                {
                    after_charge(&success, charged, host).await;
                }
            }
            (Disposition::Unpriced, _) => {
                self.release(&mut reservation, &resets).await;
                tracing::error!(
                    model = ?success.row.row.get("kwargs").and_then(|kwargs| kwargs.get("model")),
                    "Error in tracking cost callback - Cost tracking failed, add custom pricing - https://docs.litellm.ai/docs/proxy/custom_pricing"
                );
            }
            _ => self.release(&mut reservation, &resets).await,
        }
    }

    /// `_charge_request_cost`: what the request charged, trying the repeat poll's own cost when
    /// an earlier poll already charged the job.
    async fn charge_request(
        &self,
        success: &Success,
        cost: f64,
        reservation: &mut Option<BudgetReservation>,
        host: &Host,
    ) -> Option<f64> {
        let tags = success.tags.as_deref();
        if self
            .update_and_count(
                &success.settings,
                &success.identity,
                &success.row,
                cost,
                reservation,
                &success.window_resets,
                tags,
                host,
            )
            .await
        {
            return Some(cost);
        }
        let repeat = success.repeat_poll_cost;
        if repeat <= 0.0 {
            return None;
        }
        self.update_and_count(
            &success.settings,
            &success.identity,
            &repriced(&success.row, repeat),
            repeat,
            &mut None,
            &[],
            tags,
            host,
        )
        .await
        .then_some(repeat)
    }

    /// `_update_database_and_spend_counters`: reconcile the reservation, write the row, release
    /// the reservation when nothing was charged, then the counters.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per value the Python function takes"
    )]
    async fn update_and_count(
        &self,
        settings: &Settings,
        identity: &Identity,
        row: &RowRequest,
        cost: f64,
        reservation: &mut Option<BudgetReservation>,
        window_resets: &[Option<String>],
        tags: Option<&[String]>,
        host: &Host,
    ) -> bool {
        if let Some(held) = reservation.as_mut()
            && let Err(error) =
                post_call::reconcile(&self.budgets, held, window_resets, cost, false, false).await
        {
            tracing::warn!(%error, "Failed to reconcile budget reservation before persisting spend; invalidating reserved counters");
            post_call::invalidate(self.budgets.store(), held).await;
            held.finalized = Some(true);
        }
        let written = self
            .update_database(settings, identity, row, Some(cost), host)
            .await;
        if !written.charged {
            self.release(reservation, window_resets).await;
            return false;
        }
        let charge = PostCallCharge {
            token: identity.token.clone(),
            user_id: identity.user_id.clone(),
            team_id: identity.team_id.clone(),
            org_id: identity.org_id.clone(),
            project_id: identity.project_id.clone(),
            end_user_id: identity.end_user_id.clone(),
            tags: tags.map(<[String]>::to_vec),
            model_access_groups: written.model_access_groups,
            response_cost: cost,
        };
        if let Err(error) =
            post_call::post_call_counters(&self.budgets, &charge, reservation.as_mut()).await
        {
            tracing::error!(%error, "Error in tracking cost callback - the spend counter update failed");
            if let Some(held) = reservation.as_mut() {
                post_call::invalidate(self.budgets.store(), held).await;
                held.finalized = Some(true);
            }
        }
        true
    }

    /// `DBSpendUpdateWriter.update_database` on the ledger path. A failure is logged and still
    /// counts as charged, as the Python writer does.
    async fn update_database(
        &self,
        settings: &Settings,
        identity: &Identity,
        row: &RowRequest,
        response_cost: Option<f64>,
        host: &Host,
    ) -> Written {
        if settings.disable_spend_updates {
            return Written::charged();
        }
        match self
            .write_row(settings, identity, row, response_cost, host)
            .await
        {
            Ok(written) => written,
            Err(error) => {
                tracing::error!(
                    %error,
                    response_cost,
                    user_id = identity.user_id,
                    team_id = identity.team_id,
                    "Spend tracking - update_database failed. Spend log insertion may not have completed for this request."
                );
                Written::charged()
            }
        }
    }

    async fn write_row(
        &self,
        settings: &Settings,
        identity: &Identity,
        request: &RowRequest,
        response_cost: Option<f64>,
        host: &Host,
    ) -> Result<Written, Box<dyn std::error::Error + Send + Sync>> {
        let builder = self.builders.for_settings(&settings.row_config)?;
        let row = builder.build_row(&json::from_json(request.row.clone()))?;
        let billable = response_cost.filter(|cost| cost.is_finite());
        if response_cost.is_some() && billable.is_none() {
            tracing::warn!(
                ?response_cost,
                "Spend tracking - recording a non-finite response_cost as 0"
            );
        }
        if settings.disable_responses_id_security
            && text(&row, "call_type").is_some_and(|kind| RESPONSES_ID_CALL_TYPES.contains(&kind))
            && text(&row, "status") == Some("success")
            && billable.is_none_or(|cost| cost == 0.0)
        {
            return Ok(Written::charged());
        }
        let token = hashed_token(identity.token.as_deref());
        let charges = billable.is_some_and(|cost| cost != 0.0);
        let accounting = host
            .call(AccountingContext {
                row: json::dumps(&row)?,
                token: token.clone(),
                team_id: identity.team_id.clone(),
                user_id: identity.user_id.clone(),
                windows: charges,
                customer: request.customer_entities.clone(),
                customer_scopes: request.customer_scopes.clone(),
                model_access_groups: request.model_access_groups.clone(),
            })
            .await?;
        let mut entities = Map::new();
        entities.insert(
            "api_key".to_owned(),
            json!(persistable_api_key(
                token.as_deref(),
                text(&row, "api_key").unwrap_or_default()
            )),
        );
        for (key, value) in [
            ("user", &identity.user_id),
            ("end_user", &identity.end_user_id),
            ("team_id", &identity.team_id),
            ("organization_id", &identity.org_id),
            ("project_id", &identity.project_id),
        ] {
            entities.insert(key.to_owned(), json!(value));
        }
        if let Some(customer) = &request.customer_entities {
            entities.extend(customer.clone());
        }
        entities.insert(
            "agent_id".to_owned(),
            json::to_json(get_or_none(&row, "agent_id"))?,
        );
        entities.insert(
            "proxy_budget_name".to_owned(),
            json!(settings.proxy_budget_name),
        );
        let ledger = json!({
            "response_cost": billable,
            "org_id": identity.org_id,
            "team_id": identity.team_id,
            "delivery_id": uuid::Uuid::new_v4().simple().to_string(),
            "entities": entities,
            "model_access_groups": accounting.model_access_groups,
            "windows": if charges { accounting.windows } else { Vec::new() },
            "savings": accounting.savings,
        });
        let finalized = finalize_ledger_row(&row, &json::from_json(ledger))?;
        let keep = !settings.disable_spend_logs
            && !(text(&finalized, "status") == Some("failure") && settings.disable_error_logs);
        Ok(Written {
            charged: self.record(finalized, keep).await,
            model_access_groups: accounting.model_access_groups,
        })
    }

    /// `_record_spend_log`: a settle-once job row claims its charge now, every other row is
    /// queued, as an accounting-only row when request logs are off.
    async fn record(&self, row: Value, keep: bool) -> bool {
        if settles_once(&row) {
            return self.claim(row, keep).await;
        }
        let stored = if keep {
            with_entry(&row, "accounting_log_disabled", Value::Bool(false))
        } else {
            accounting_only(&row)
        };
        self.enqueue(stored);
        true
    }

    fn enqueue(&self, row: Value) {
        let Some(row) = queued_row(row) else { return };
        let budget = self.writer.settings().queue_max_bytes;
        self.writer.enqueue(vec![row], false, budget);
    }

    /// `_claim_settle_once_cost_row` for a ledger row: the job's delivery row is the claim, so
    /// the poll whose insert lands owns the charge. A $0 poll records nothing.
    async fn claim(&self, row: Value, keep: bool) -> bool {
        let charged = spend(&row) > 0.0;
        let stored = if keep { row } else { accounting_only(&row) };
        if !charged {
            return true;
        }
        let Some(queued) = queued_row(stored) else {
            return true;
        };
        let max_depth = self.writer.settings().max_depth;
        let retained = match retain_metadata(&queued, self.retention.as_ref(), max_depth) {
            Ok(retained) => retained,
            Err(error) => {
                tracing::warn!(%error, "Could not claim a job's cost row, queueing it");
                self.writer
                    .enqueue(vec![queued], false, self.writer.settings().queue_max_bytes);
                return true;
            }
        };
        match self
            .writer
            .flusher()
            .store()
            .insert_with_deliveries(&[jsonified(&retained)])
            .await
        {
            Ok(accepted) => accepted == 1,
            Err(error) => {
                tracing::warn!(%error, "Could not claim a job's cost row, queueing it");
                self.writer.enqueue(
                    vec![retained],
                    false,
                    self.writer.settings().queue_max_bytes,
                );
                true
            }
        }
    }

    pub(super) async fn on_failure(
        &self,
        failure: Failure,
        reservation: Option<BudgetReservation>,
        host: &Host,
    ) {
        let mut failure = failure;
        let mut reservation = reservation;
        let disabled = failure.settings.disable_spend_updates;
        let resets = failure.window_resets.clone();
        if !failure.completed_cost || disabled {
            self.release(&mut reservation, &resets).await;
        }
        if (failure.completed_cost && disabled) || !failure.track {
            return;
        }
        if let Some(enrich) = failure.enrich.clone()
            && let Some(enriched) = self.enrich(host, &failure.identity, &enrich).await
        {
            for row in &mut failure.rows {
                enrich_row(row, &enriched);
            }
        }
        let tags = failure.tags.as_deref();
        if failure.completed_cost {
            for row in &failure.rows {
                let cost = row.response_cost.unwrap_or(0.0);
                self.update_and_count(
                    &failure.settings,
                    &failure.identity,
                    row,
                    cost,
                    &mut reservation,
                    &resets,
                    tags,
                    host,
                )
                .await;
            }
            return;
        }
        let mut counted = Vec::new();
        for row in &failure.rows {
            let written = self
                .update_database(
                    &failure.settings,
                    &failure.identity,
                    row,
                    row.response_cost,
                    host,
                )
                .await;
            let cost = row.response_cost.unwrap_or(0.0);
            if written.charged && cost > 0.0 {
                counted.push((cost, written.model_access_groups));
            }
        }
        if disabled {
            return;
        }
        for (cost, model_access_groups) in counted {
            let identity = &failure.identity;
            let charge = PostCallCharge {
                token: identity.token.clone(),
                user_id: identity.user_id.clone(),
                team_id: identity.team_id.clone(),
                org_id: identity.org_id.clone(),
                project_id: identity.project_id.clone(),
                end_user_id: identity.end_user_id.clone(),
                tags: tags.map(<[String]>::to_vec),
                model_access_groups,
                response_cost: cost,
            };
            if let Err(error) = post_call::post_call_counters(&self.budgets, &charge, None).await {
                tracing::error!(%error, "Failed to raise spend counters for a failure spend row");
            }
        }
    }
}

/// `_should_track_cost_callback`.
fn tracked(success: &Success) -> bool {
    if success.settings.disable_spend_updates {
        return false;
    }
    let identity = &success.identity;
    identity.agent_id.is_some()
        || identity.token.is_some()
        || identity.user_id.is_some()
        || identity.team_id.is_some()
        || identity.end_user_id.is_some()
        || success.tracks_unattributed
}

/// The work `update_cache` and the cost callback leave to Python once a request is charged.
async fn after_charge(success: &Success, charged: f64, host: &Host) {
    let identity = &success.identity;
    if let Some(token) = &identity.token
        && let Err(error) = host
            .call(SoftBudgetAlert {
                token: token.clone(),
                response_cost: charged,
            })
            .await
    {
        tracing::debug!(%error, "the soft budget alert host call failed");
    }
    if success.settings.proxy_budget_name.is_some()
        && let Err(error) = host
            .call(IncrementProxySpend {
                response_cost: charged,
            })
            .await
    {
        tracing::warn!(%error, "Spend tracking - failed to add spend to the global proxy spend");
    }
    if let Err(error) = host
        .call(CustomerSpendAlert {
            token: identity.token.clone(),
            key_alias: identity.key_alias.clone(),
            end_user_id: identity.end_user_id.clone(),
            response_cost: charged,
            max_budget: success.end_user_max_budget,
        })
        .await
    {
        tracing::debug!(%error, "the customer spend alert host call failed");
    }
}
