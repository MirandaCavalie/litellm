use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use litellm_callbacks::{
    BudgetReservation, Callback, Field, FieldSet, FieldValues, Hook, HookInput, HostRequest,
    ReservationEntry, hosted,
};
use litellm_callbacks_budgets::Budgets;
use litellm_callbacks_spend_log::{SpendLogCallback, SpendLogParts};
use litellm_coroutine::CoroutineState;
use litellm_python_compat::{Value, json as pyjson};
use litellm_redis_store::{
    Number,
    spend_counters::{SpendCounter, SpendCounters},
};
use litellm_spend_log_writer::{
    LedgerStore, SpendLogFlusher, SpendLogRow, SpendLogWriter, WriterSettings,
};
use rstest::{fixture, rstest};
use serde_json::{Value as Json, json};

const KEY_COUNTER: &str = "spend:key:hashed";

#[derive(Clone, Default)]
struct Store {
    written: Arc<Mutex<Vec<SpendLogRow>>>,
    accepts: Option<usize>,
}

impl LedgerStore for Store {
    async fn insert_with_deliveries(
        &self,
        rows: &[SpendLogRow],
    ) -> Result<usize, litellm_db::Error> {
        self.written.lock().unwrap().extend_from_slice(rows);
        Ok(self.accepts.unwrap_or(rows.len()))
    }
}

struct Quiet;

impl litellm_db::Observer for Quiet {
    fn record(&self, _event: litellm_db::QueryEvent) {}
}

struct Harness {
    callback: Arc<SpendLogCallback<Store>>,
    counters: Arc<SpendCounters>,
    store: Store,
}

fn harness(store: Store) -> Harness {
    let counters = Arc::new(SpendCounters::new(None));
    counters
        .set_local(
            &SpendCounter::parse(KEY_COUNTER).unwrap(),
            Number::Float(5.0),
            None,
        )
        .unwrap();
    let writer = Arc::new(SpendLogWriter::new(SpendLogFlusher::new(
        store.clone(),
        Arc::new(Quiet),
        WriterSettings::default(),
    )));
    Harness {
        callback: Arc::new(SpendLogCallback::new(SpendLogParts {
            writer,
            budgets: Arc::new(Budgets::new(Arc::clone(&counters))),
            minimum_custom_key_length: 16,
            word_ranges: Vec::new(),
            retention: None,
        })),
        counters,
        store,
    }
}

#[fixture]
fn golden() -> (Json, Json) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/rows.jsonl");
    let text = std::fs::read_to_string(path).unwrap();
    let record: Json = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Json>(line).ok())
        .find(|record| {
            record["input"]["call_type"] == "acompletion"
                && record["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("\"status\": \"success\""))
                && record["config"]["turn_off_message_logging"] == false
        })
        .unwrap();
    (record["config"].clone(), record["input"].clone())
}

fn settings(config: &Json) -> Json {
    json!({
        "disable_spend_updates": false,
        "disable_spend_logs": false,
        "disable_error_logs": false,
        "disable_responses_id_security": false,
        "row_config": config,
        "proxy_budget_name": "litellm-proxy-budget",
    })
}

fn identity() -> Json {
    json!({"token": "sk-raw-key-value", "user_id": "user-1", "team_id": "team-1", "end_user_id": null, "key_alias": "alias"})
}

fn reservation() -> BudgetReservation {
    BudgetReservation {
        reserved_cost: Some(2.0),
        entries: Some(vec![Some(ReservationEntry {
            counter_key: Some(KEY_COUNTER.to_owned()),
            entity_type: Some("key".to_owned()),
            entity_id: Some("hashed".to_owned()),
            reserved_cost: Some(2.0),
            applied_adjustment: Some(0.0),
            ..ReservationEntry::default()
        })]),
        ..BudgetReservation::default()
    }
}

fn input(hook: Hook, spend_log_input: Json, reservation: Option<BudgetReservation>) -> HookInput {
    let mut fields = FieldValues::new(FieldSet::of(&[
        Field::SpendLogInput,
        Field::BudgetReservation,
        Field::Outcome,
    ]))
    .with(Field::SpendLogInput, spend_log_input);
    if let Some(reservation) = reservation {
        fields.insert(
            Field::BudgetReservation,
            serde_json::to_value(reservation).unwrap(),
        );
    }
    HookInput {
        hook,
        fields,
        call: None,
    }
}

/// Runs one hook with a host that answers every call and records them.
async fn run(harness: &Harness, input: HookInput) -> Vec<(String, Json)> {
    let callback = Arc::clone(&harness.callback);
    let mut body = hosted(|host| async move {
        let _ = callback.handle_hosted(&input, None, host).await;
    });
    let mut calls = Vec::new();
    loop {
        match body.resume().await.unwrap() {
            CoroutineState::Complete(()) => return calls,
            CoroutineState::Yielded(HostRequest {
                call,
                request,
                reply,
            }) => {
                let answer = match call {
                    "accounting_context" => json!({
                        "windows": [],
                        "savings": {"compression": 0.0, "prompt_caching": 0.0},
                        "model_access_groups": [],
                    }),
                    "enrich_key_identity" => {
                        json!({"user_id": "enriched-user", "team_alias": "enriched-team"})
                    }
                    _ => Json::Null,
                };
                calls.push((call.to_owned(), request));
                reply.send(Ok(answer));
            }
        }
    }
}

fn success(config: &Json, row: Json, cost: Option<f64>, disposition: &str) -> Json {
    json!({
        "kind": "success",
        "settings": settings(config),
        "identity": identity(),
        "disposition": disposition,
        "call_type": "acompletion",
        "row": {"row": row, "response_cost": cost},
    })
}

fn queued(harness: &Harness) -> Vec<Value> {
    harness
        .callback
        .writer()
        .queue()
        .dequeue(100)
        .iter()
        .map(|row| pyjson::loads(&row.to_json()).unwrap())
        .collect()
}

fn get<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    litellm_python_compat::dict::get(row, key)
}

fn counter(harness: &Harness) -> Option<f64> {
    harness
        .counters
        .get_local(&SpendCounter::parse(KEY_COUNTER).unwrap())
        .unwrap()
        .map(Number::as_f64)
}

fn names(calls: &[(String, Json)]) -> Vec<&str> {
    calls.iter().map(|(name, _)| name.as_str()).collect()
}

#[rstest]
#[tokio::test]
async fn a_priced_success_queues_one_ledger_row_and_calls_the_host_in_order(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());

    let calls = run(
        &harness,
        input(
            Hook::AttemptEnded,
            success(&config, row, Some(0.25), "charge"),
            None,
        ),
    )
    .await;

    assert_eq!(
        names(&calls),
        [
            "accounting_context",
            "soft_budget_alert",
            "increment_proxy_spend",
            "customer_spend_alert"
        ]
    );
    let rows = queued(&harness);
    assert_eq!(rows.len(), 1);
    assert_eq!(get(&rows[0], "spend"), Some(&Value::Float(0.25)));
    assert_eq!(
        get(&rows[0], "accounting_log_disabled"),
        Some(&Value::Bool(false))
    );
    assert!(get(&rows[0], "accounting_payload").is_some());
    assert!(get(&rows[0], "accounting_delivery_id").is_some());
}

#[rstest]
#[tokio::test]
async fn spend_logs_off_queue_only_the_charge(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut request = success(&config, row, Some(0.25), "charge");
    request["settings"]["disable_spend_logs"] = json!(true);

    run(&harness, input(Hook::AttemptEnded, request, None)).await;

    let rows = queued(&harness);
    assert_eq!(
        get(&rows[0], "accounting_log_disabled"),
        Some(&Value::Bool(true))
    );
    assert!(get(&rows[0], "metadata").is_none());
}

#[rstest]
#[tokio::test]
async fn disabled_spend_updates_skip_the_row_and_release_the_reservation(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut request = success(&config, row, Some(0.25), "charge");
    request["settings"]["disable_spend_updates"] = json!(true);

    let calls = run(
        &harness,
        input(Hook::AttemptEnded, request, Some(reservation())),
    )
    .await;

    assert!(calls.is_empty());
    assert!(queued(&harness).is_empty());
    assert_eq!(counter(&harness), Some(3.0));
}

#[rstest]
#[case::deferred_batch("defer_batch", Some(3.0))]
#[case::unbilled_interaction("interaction_unbilled", Some(3.0))]
#[case::interaction_still_running("interaction_in_progress", Some(5.0))]
#[case::unpriced("unpriced", Some(3.0))]
#[tokio::test]
async fn an_uncharged_success_releases_or_keeps_the_reservation(
    golden: (Json, Json),
    #[case] disposition: &str,
    #[case] counter_after: Option<f64>,
) {
    let (config, row) = golden;
    let harness = harness(Store::default());

    run(
        &harness,
        input(
            Hook::AttemptEnded,
            success(&config, row, None, disposition),
            Some(reservation()),
        ),
    )
    .await;

    assert!(queued(&harness).is_empty());
    assert_eq!(counter(&harness), counter_after);
}

#[rstest]
#[tokio::test]
async fn a_request_with_no_identity_is_not_charged(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut request = success(&config, row, Some(0.25), "charge");
    request["identity"] = json!({});

    let calls = run(&harness, input(Hook::AttemptEnded, request, None)).await;

    assert!(calls.is_empty());
    assert!(queued(&harness).is_empty());
}

#[rstest]
#[tokio::test]
async fn a_failed_attempt_is_left_to_the_fallback(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut hook = input(
        Hook::AttemptEnded,
        success(&config, row, Some(0.25), "charge"),
        None,
    );
    hook.fields.insert(Field::Outcome, json!("failure"));

    run(&harness, hook).await;

    assert!(queued(&harness).is_empty());
}

#[rstest]
#[tokio::test]
async fn enrichment_fills_the_row_metadata_the_key_left_empty(golden: (Json, Json)) {
    let (config, mut row) = golden;
    row["metadata"]["user_api_key_user_id"] = Json::Null;
    let harness = harness(Store::default());
    let mut request = success(&config, row, Some(0.25), "charge");
    request["identity"]["user_id"] = Json::Null;
    request["enrich"] = json!({"resolve_missing_key_identity": true});

    let calls = run(&harness, input(Hook::AttemptEnded, request, None)).await;

    assert_eq!(names(&calls)[0], "enrich_key_identity");
    assert_eq!(calls[1].1["user_id"], json!("enriched-user"));
    let rows = queued(&harness);
    let Some(Value::Str(metadata)) = get(&rows[0], "metadata") else {
        panic!("metadata is a JSON string")
    };
    assert!(metadata.contains("enriched-user"), "{metadata}");
}

fn job_row(row: &Json) -> Json {
    let mut job = row.clone();
    job["call_type"] = json!("aretrieve_batch");
    job["response_id"] = json!("batch_abc");
    job
}

#[rstest]
#[case::first_poll_owns_the_charge(Some(1), 1)]
#[case::a_later_poll_charges_nothing(Some(0), 0)]
#[tokio::test]
async fn a_settle_once_job_row_is_claimed_through_the_ledger_store(
    golden: (Json, Json),
    #[case] accepts: Option<usize>,
    #[case] alerts: usize,
) {
    let (config, row) = golden;
    let harness = harness(Store {
        accepts,
        ..Store::default()
    });
    let mut request = success(&config, job_row(&row), Some(0.5), "charge");
    request["call_type"] = json!("aretrieve_batch");

    let calls = run(&harness, input(Hook::AttemptEnded, request, None)).await;

    assert_eq!(harness.store.written.lock().unwrap().len(), 1);
    assert!(queued(&harness).is_empty());
    assert_eq!(
        names(&calls)
            .iter()
            .filter(|name| **name == "customer_spend_alert")
            .count(),
        alerts
    );
}

fn failure(config: &Json, rows: Vec<Json>, completed: bool) -> Json {
    json!({
        "kind": "failure",
        "settings": settings(config),
        "identity": identity(),
        "completed_cost": completed,
        "track": true,
        "rows": rows.into_iter().map(|row| json!({"row": row, "response_cost": 0.0})).collect::<Vec<_>>(),
    })
}

#[rstest]
#[tokio::test]
async fn a_failure_releases_the_reservation_and_writes_every_billed_row(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut attempt = row.clone();
    attempt["response_id"] = json!("chatcmpl-attempt-1");

    let calls = run(
        &harness,
        input(
            Hook::PostCallFailure,
            failure(&config, vec![attempt, row], false),
            Some(reservation()),
        ),
    )
    .await;

    assert_eq!(counter(&harness), Some(3.0));
    assert_eq!(queued(&harness).len(), 2);
    assert_eq!(names(&calls), ["accounting_context", "accounting_context"]);
}

#[rstest]
#[tokio::test]
async fn an_untracked_failure_only_releases(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    let mut request = failure(&config, vec![row], false);
    request["track"] = json!(false);

    run(
        &harness,
        input(Hook::PostCallFailure, request, Some(reservation())),
    )
    .await;

    assert_eq!(counter(&harness), Some(3.0));
    assert!(queued(&harness).is_empty());
}

#[rstest]
#[tokio::test]
async fn flush_drains_the_queue_into_the_store(golden: (Json, Json)) {
    let (config, row) = golden;
    let harness = harness(Store::default());
    run(
        &harness,
        input(
            Hook::AttemptEnded,
            success(&config, row, Some(0.25), "charge"),
            None,
        ),
    )
    .await;

    harness.callback.flush().await;

    assert!(harness.callback.writer().queue().is_empty());
    assert_eq!(harness.store.written.lock().unwrap().len(), 1);
}
