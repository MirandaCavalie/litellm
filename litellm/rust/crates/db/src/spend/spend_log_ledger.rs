//! The spend log ledger transaction: one commit records each request's accounting
//! delivery in `LiteLLM_SpendLogDelivery` and its request log in `LiteLLM_SpendLogs`,
//! so a replayed flush can neither charge a request twice nor lose its log.
//!
//! The statements mirror the Python ledger writer (`SpendLogsRepository._insert_ledger`):
//! deliveries go in first with `ON CONFLICT DO NOTHING`, a batch or video job that a
//! pre-ledger cost row already charged loses its delivery again, and only the rows whose
//! delivery this commit accepted are logged.

use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};

use crate::{Error, Statements};

/// The `LiteLLM_SpendLogs` columns a request log stores as `jsonb`, which callers
/// hand over as JSON text.
pub const SPEND_LOG_JSON_COLUMNS: [&str; 5] = [
    "metadata",
    "request_tags",
    "messages",
    "response",
    "proxy_server_request",
];

fn empty_text() -> Option<String> {
    Some(String::new())
}

fn empty_object() -> Option<Value> {
    Some(json!({}))
}

fn empty_array() -> Option<Value> {
    Some(json!([]))
}

/// Reads a request log timestamp as the proxy writes it: RFC 3339 with an offset (made
/// UTC), or a naive `T`- or space-separated time taken as UTC.
pub fn parse_spend_log_timestamp(text: &str) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(text)
        .map(|timestamp| timestamp.naive_utc())
        .or_else(|_| {
            DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%:z").map(|t| t.naive_utc())
        })
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()
}

fn timestamp<'de, D: Deserializer<'de>>(deserializer: D) -> Result<NaiveDateTime, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_spend_log_timestamp(&text)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid timestamp {text:?}")))
}

fn optional_timestamp<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NaiveDateTime>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|text| {
            parse_spend_log_timestamp(&text)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid timestamp {text:?}")))
        })
        .transpose()
}

fn timestamp_text<S: Serializer>(value: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&value.format("%Y-%m-%dT%H:%M:%S%.f"))
}

fn optional_timestamp_text<S: Serializer>(
    value: &Option<NaiveDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => timestamp_text(value, serializer),
        None => serializer.serialize_none(),
    }
}

/// One `LiteLLM_SpendLogs` row, parsed from the request log the proxy queued.
///
/// A column the log leaves out takes the schema default, as a Prisma insert would; an
/// explicit `null` stays `NULL`; a column the table does not have refuses the row.
/// `created_at` and `updated_at` default to the commit time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpendLogRecord {
    pub request_id: String,
    pub call_type: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub spend: f64,
    #[serde(default)]
    pub total_tokens: i32,
    #[serde(default)]
    pub prompt_tokens: i32,
    #[serde(default)]
    pub completion_tokens: i32,
    #[serde(
        rename = "startTime",
        deserialize_with = "timestamp",
        serialize_with = "timestamp_text"
    )]
    pub start_time: NaiveDateTime,
    #[serde(
        rename = "endTime",
        deserialize_with = "timestamp",
        serialize_with = "timestamp_text"
    )]
    pub end_time: NaiveDateTime,
    #[serde(default)]
    pub request_duration_ms: Option<i32>,
    #[serde(
        rename = "completionStartTime",
        default,
        deserialize_with = "optional_timestamp",
        serialize_with = "optional_timestamp_text"
    )]
    pub completion_start_time: Option<NaiveDateTime>,
    #[serde(default)]
    pub model: String,
    #[serde(default = "empty_text")]
    pub model_id: Option<String>,
    #[serde(default = "empty_text")]
    pub model_group: Option<String>,
    #[serde(default = "empty_text")]
    pub custom_llm_provider: Option<String>,
    #[serde(default = "empty_text")]
    pub api_base: Option<String>,
    #[serde(default = "empty_text")]
    pub user: Option<String>,
    #[serde(default = "empty_object")]
    pub metadata: Option<Value>,
    #[serde(default = "empty_text")]
    pub cache_hit: Option<String>,
    #[serde(default = "empty_text")]
    pub cache_key: Option<String>,
    #[serde(default = "empty_array")]
    pub request_tags: Option<Value>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default)]
    pub end_user: Option<String>,
    #[serde(default)]
    pub requester_ip_address: Option<String>,
    #[serde(default = "empty_object")]
    pub messages: Option<Value>,
    #[serde(default = "empty_object")]
    pub response: Option<Value>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub mcp_namespaced_tool_name: Option<String>,
    #[serde(default)]
    pub billing_agent_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default = "empty_object")]
    pub proxy_server_request: Option<Value>,
    #[serde(default)]
    pub litellm_call_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_timestamp",
        serialize_with = "optional_timestamp_text"
    )]
    pub created_at: Option<NaiveDateTime>,
    #[serde(
        default,
        deserialize_with = "optional_timestamp",
        serialize_with = "optional_timestamp_text"
    )]
    pub updated_at: Option<NaiveDateTime>,
}

impl SpendLogRecord {
    /// Parses one request log; a log the table cannot hold is a rejected row, so the
    /// flush drops it alone and keeps the rest of its batch.
    pub fn from_columns(columns: Map<String, Value>) -> Result<Self, Error> {
        serde_json::from_value(Value::Object(columns))
            .map_err(|error| Error::RejectedSpendLogRow(error.to_string()))
    }

    fn stamped(&self, now: NaiveDateTime) -> Self {
        Self {
            created_at: self.created_at.or(Some(now)),
            updated_at: self.updated_at.or(Some(now)),
            ..self.clone()
        }
    }
}

/// Inserts request logs, skipping any `request_id` already stored, and returns the ids
/// it inserted.
pub async fn insert_spend_log_rows(
    executor: impl PgExecutor<'_>,
    rows: &[SpendLogRecord],
    now: NaiveDateTime,
    named: bool,
) -> Result<Vec<String>, Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let records = serde_json::to_value(rows.iter().map(|row| row.stamped(now)).collect::<Vec<_>>())
        .map_err(|error| Error::RejectedSpendLogRow(error.to_string()))?;
    Ok(
        sqlx::query_file_scalar!("queries/spend/insert_spend_log_rows.sql", records)
            .fetch_all(Statements::new(executor, named))
            .await?,
    )
}

/// The accounting work one request leaves for the ledger worker. `accounting_payload` is
/// the worker's own JSON contract and is stored as given; `is_batch` marks a batch or
/// video job that settles once under its request id.
#[macro_rules_attribute::apply(crate::db_input)]
pub struct SpendLogDelivery {
    pub delivery_id: String,
    pub request_id: String,
    pub accounting_payload: Map<String, Value>,
    pub is_batch: bool,
}

/// A delivery with the request log it brings, if the request is logged at all.
#[macro_rules_attribute::apply(crate::db_input)]
pub struct SpendLogLedgerEntry {
    pub delivery: SpendLogDelivery,
    pub row: Option<SpendLogRecord>,
}

fn first_entry_per_delivery(entries: &[SpendLogLedgerEntry]) -> Vec<&SpendLogLedgerEntry> {
    let mut by_delivery: BTreeMap<&str, &SpendLogLedgerEntry> = BTreeMap::new();
    for entry in entries {
        by_delivery
            .entry(entry.delivery.delivery_id.as_str())
            .or_insert(entry);
    }
    by_delivery.into_values().collect()
}

/// Writes `entries` inside `transaction`: the first entry per delivery id wins, a
/// delivery already stored is skipped, and only the rows of accepted deliveries are
/// logged. Returns how many deliveries this call accepted.
pub async fn insert_spend_log_ledger(
    transaction: &mut Transaction<'_, Postgres>,
    entries: &[SpendLogLedgerEntry],
    write_spend_logs: bool,
    now: NaiveDateTime,
    named: bool,
) -> Result<usize, Error> {
    let ledger = first_entry_per_delivery(entries);
    if ledger.is_empty() {
        return Ok(0);
    }
    let payloads = Value::Array(
        ledger
            .iter()
            .map(|entry| {
                json!({
                    "delivery_id": entry.delivery.delivery_id,
                    "request_id": entry.delivery.request_id,
                    "accounting_payload": entry.delivery.accounting_payload,
                    "is_batch": entry.delivery.is_batch,
                })
            })
            .collect(),
    );
    let accepted: Vec<String> =
        sqlx::query_file_scalar!("queries/spend/insert_spend_log_deliveries.sql", payloads)
            .fetch_all(Statements::new(&mut **transaction, named))
            .await?;
    let settled_once: Vec<String> = ledger
        .iter()
        .filter(|entry| entry.delivery.is_batch)
        .map(|entry| entry.delivery.delivery_id.clone())
        .filter(|delivery_id| accepted.contains(delivery_id))
        .collect();
    let charged_by_legacy: Vec<String> = if settled_once.is_empty() {
        Vec::new()
    } else {
        sqlx::query_file_scalar!(
            "queries/spend/select_spend_log_jobs_charged_by_legacy.sql",
            &settled_once
        )
        .fetch_all(Statements::new(&mut **transaction, named))
        .await?
    };
    if !charged_by_legacy.is_empty() {
        sqlx::query_file!(
            "queries/spend/delete_spend_log_deliveries.sql",
            &charged_by_legacy
        )
        .execute(Statements::new(&mut **transaction, named))
        .await?;
    }
    let kept = |delivery_id: &String| {
        accepted.contains(delivery_id) && !charged_by_legacy.contains(delivery_id)
    };
    let rows: Vec<SpendLogRecord> = ledger
        .iter()
        .filter(|entry| kept(&entry.delivery.delivery_id))
        .filter_map(|entry| entry.row.clone())
        .collect();
    if write_spend_logs {
        insert_spend_log_rows(&mut **transaction, &rows, now, named).await?;
    }
    Ok(accepted.iter().filter(|id| kept(id)).count())
}

/// Commits `entries` as one transaction, giving up with `TransactionTimedOut` after
/// `timeout` so a stuck commit cannot hold the flush forever.
pub async fn write_spend_log_ledger(
    pool: &PgPool,
    entries: &[SpendLogLedgerEntry],
    write_spend_logs: bool,
    timeout: Duration,
    named: bool,
) -> Result<usize, Error> {
    let now = Utc::now().naive_utc();
    let commit = async {
        let mut transaction = pool.begin().await?;
        let accepted =
            insert_spend_log_ledger(&mut transaction, entries, write_spend_logs, now, named)
                .await?;
        transaction.commit().await?;
        Ok(accepted)
    };
    tokio::time::timeout(timeout, commit)
        .await
        .unwrap_or(Err(Error::TransactionTimedOut))
}

#[cfg(test)]
mod tests {
    use std::{error::Error as StdError, time::Duration};

    use rstest::{fixture, rstest};
    use serde_json::{Map, Value, json};
    use sqlx::{PgExecutor, PgPool};

    use chrono::NaiveDateTime;

    use super::{
        SpendLogDelivery, SpendLogLedgerEntry, SpendLogRecord, insert_spend_log_ledger,
        insert_spend_log_rows, parse_spend_log_timestamp, write_spend_log_ledger,
    };
    use crate::{Error, ErrorKind, test_support::database_pool};

    const SPEND_LOG_COLUMNS: [&str; 36] = [
        "request_id",
        "call_type",
        "api_key",
        "spend",
        "total_tokens",
        "prompt_tokens",
        "completion_tokens",
        "startTime",
        "endTime",
        "request_duration_ms",
        "completionStartTime",
        "model",
        "model_id",
        "model_group",
        "custom_llm_provider",
        "api_base",
        "user",
        "metadata",
        "cache_hit",
        "cache_key",
        "request_tags",
        "team_id",
        "organization_id",
        "end_user",
        "requester_ip_address",
        "messages",
        "response",
        "session_id",
        "status",
        "mcp_namespaced_tool_name",
        "billing_agent_id",
        "agent_id",
        "proxy_server_request",
        "litellm_call_id",
        "created_at",
        "updated_at",
    ];

    fn now() -> NaiveDateTime {
        parse_spend_log_timestamp("2026-11-02T12:00:00+00:00").expect("timestamp parses")
    }

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn record(columns: Value) -> SpendLogRecord {
        SpendLogRecord::from_columns(object(columns)).expect("record parses")
    }

    fn row(request_id: &str) -> SpendLogRecord {
        record(json!({
            "request_id": request_id,
            "call_type": "acompletion",
            "api_key": "hashed",
            "spend": 0.5,
            "startTime": "2026-11-02T12:00:00+00:00",
            "endTime": "2026-11-02 12:00:01+00:00",
            "metadata": {"accounting_savings": {"compression": 0.2}},
            "request_tags": ["a"],
            "messages": [{"role": "user", "content": "hi"}],
            "status": "success",
        }))
    }

    #[rstest]
    #[case::rfc3339_offset("2026-11-02T14:00:00+02:00", "2026-11-02T12:00:00.000")]
    #[case::python_str_of_an_aware_datetime(
        "2026-11-02 12:00:00.250000+00:00",
        "2026-11-02T12:00:00.250"
    )]
    #[case::naive_iso("2026-11-02T12:00:00.5", "2026-11-02T12:00:00.500")]
    #[case::naive_space("2026-11-02 12:00:00", "2026-11-02T12:00:00.000")]
    fn reads_the_timestamps_the_proxy_writes(#[case] text: &str, #[case] expected: &str) {
        assert_eq!(
            parse_spend_log_timestamp(text)
                .map(|parsed| parsed.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()),
            Some(expected.to_owned())
        );
    }

    #[rstest]
    #[case::unknown_column(json!({"request_id": "bad", "call_type": "x", "startTime": "2026-11-02 12:00:00", "endTime": "2026-11-02 12:00:00", "unexpected": 1}))]
    #[case::missing_start(json!({"request_id": "bad", "call_type": "x", "endTime": "2026-11-02 12:00:00"}))]
    #[case::not_a_number(json!({"request_id": "bad", "call_type": "x", "startTime": "2026-11-02 12:00:00", "endTime": "2026-11-02 12:00:00", "spend": "abc"}))]
    #[case::not_a_time(json!({"request_id": "bad", "call_type": "x", "startTime": "soon", "endTime": "2026-11-02 12:00:00"}))]
    #[case::null_in_a_required_column(json!({"request_id": "bad", "call_type": "x", "startTime": "2026-11-02 12:00:00", "endTime": "2026-11-02 12:00:00", "model": null}))]
    fn a_log_the_table_cannot_hold_is_a_rejected_row(#[case] columns: Value) {
        let error = SpendLogRecord::from_columns(object(columns)).expect_err("refused");

        assert_eq!(error.kind(), ErrorKind::DataRejected);
    }

    #[rstest]
    #[case::missing_columns_take_defaults(
        json!({"request_id": "r", "call_type": "x", "startTime": "2026-11-02 12:00:00", "endTime": "2026-11-02 12:00:00"}),
        json!({"api_key": "", "model_id": "", "metadata": {}, "request_tags": [], "team_id": null, "created_at": null})
    )]
    #[case::explicit_nulls_stay_null(
        json!({"request_id": "r", "call_type": "x", "startTime": "2026-11-02 12:00:00", "endTime": "2026-11-02 12:00:00", "model_id": null, "metadata": null}),
        json!({"api_key": "", "model_id": null, "metadata": null, "request_tags": [], "team_id": null, "created_at": null})
    )]
    fn parses_a_log_into_its_columns(#[case] columns: Value, #[case] expected: Value) {
        let stored = serde_json::to_value(record(columns)).expect("serializes");

        for (column, value) in object(expected) {
            assert_eq!(stored.get(&column), Some(&value), "{column}");
        }
    }

    fn entry(delivery_id: &str, request_id: &str, logged: bool) -> SpendLogLedgerEntry {
        SpendLogLedgerEntry {
            delivery: SpendLogDelivery {
                delivery_id: delivery_id.into(),
                request_id: request_id.into(),
                accounting_payload: object(json!({"request_id": request_id, "spend": 0.5})),
                is_batch: false,
            },
            row: logged.then(|| row(request_id)),
        }
    }

    #[fixture]
    fn ledger() -> Vec<SpendLogLedgerEntry> {
        vec![
            entry("delivery-b", "ledger-b", true),
            entry("delivery-a", "ledger-a", true),
            entry("delivery-a", "ledger-a-replayed", true),
            entry("delivery-c", "ledger-c", false),
        ]
    }

    async fn ids(
        executor: impl PgExecutor<'_>,
        query: &'static str,
        ids: &[&str],
    ) -> Result<Vec<String>, sqlx::Error> {
        #[expect(
            clippy::disallowed_methods,
            reason = "Test assertions are outside the compile-checked query set"
        )]
        sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(query))
            .bind(ids)
            .fetch_all(executor)
            .await
    }

    const DELIVERIES: &str = r#"SELECT delivery_id FROM "LiteLLM_SpendLogDelivery" WHERE delivery_id = ANY($1) ORDER BY 1"#;
    const LOGS: &str =
        r#"SELECT request_id FROM "LiteLLM_SpendLogs" WHERE request_id = ANY($1) ORDER BY 1"#;

    #[rstest]
    #[case::write_spend_logs(true, vec!["ledger-a", "ledger-b"])]
    #[case::deliveries_only(false, vec![])]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn writes_each_delivery_once_with_its_logged_row(
        #[case] write_spend_logs: bool,
        #[case] expected_logs: Vec<&str>,
        ledger: Vec<SpendLogLedgerEntry>,
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let mut tx = pool.begin().await?;
        let accepted =
            insert_spend_log_ledger(&mut tx, &ledger, write_spend_logs, now(), true).await?;
        assert_eq!(accepted, 3);
        let replay =
            insert_spend_log_ledger(&mut tx, &ledger, write_spend_logs, now(), true).await?;
        assert_eq!(replay, 0);
        let every_id = [
            "delivery-a",
            "delivery-b",
            "delivery-c",
            "ledger-a",
            "ledger-a-replayed",
            "ledger-b",
            "ledger-c",
        ];
        assert_eq!(
            ids(&mut *tx, DELIVERIES, &every_id).await?,
            ["delivery-a", "delivery-b", "delivery-c"]
        );
        assert_eq!(ids(&mut *tx, LOGS, &every_id).await?, expected_logs);
        tx.rollback().await?;
        Ok(())
    }

    #[rstest]
    #[case::charged_before_the_ledger(0.25, "success", vec![])]
    #[case::left_a_zero_cost_row(0.0, "success", vec!["job-1"])]
    #[case::left_a_failed_row(0.25, "failure", vec!["job-1"])]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn a_settle_once_job_keeps_its_delivery_unless_a_legacy_row_charged_it(
        #[case] legacy_spend: f64,
        #[case] legacy_status: &str,
        #[case] expected_deliveries: Vec<&str>,
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let mut tx = pool.begin().await?;
        let legacy = SpendLogRecord {
            spend: legacy_spend,
            status: Some(legacy_status.to_owned()),
            ..row("job-1")
        };
        insert_spend_log_rows(&mut *tx, &[legacy], now(), true).await?;
        let job = SpendLogLedgerEntry {
            delivery: SpendLogDelivery {
                delivery_id: "job-1".into(),
                request_id: "job-1".into(),
                accounting_payload: object(json!({"spend": 0.25})),
                is_batch: true,
            },
            row: Some(row("job-1")),
        };
        let accepted = insert_spend_log_ledger(&mut tx, &[job], true, now(), true).await?;
        assert_eq!(accepted, expected_deliveries.len());
        assert_eq!(
            ids(&mut *tx, DELIVERIES, &["job-1"]).await?,
            expected_deliveries
        );
        tx.rollback().await?;
        Ok(())
    }

    #[rstest]
    #[case::only_required_columns()]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn a_missing_column_takes_the_table_default(
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let mut tx = pool.begin().await?;
        #[expect(
            clippy::disallowed_methods,
            reason = "The reference row is inserted the way Prisma omits a column"
        )]
        sqlx::query(
            r#"INSERT INTO "LiteLLM_SpendLogs" (request_id, call_type, "startTime", "endTime", created_at, updated_at)
               VALUES ('defaults-sql', 'acompletion', '2026-11-02 12:00:00', '2026-11-02 12:00:01', $1, $1)"#,
        )
        .bind(now())
        .execute(&mut *tx)
        .await?;
        let minimal = record(json!({
            "request_id": "defaults-native",
            "call_type": "acompletion",
            "startTime": "2026-11-02 12:00:00",
            "endTime": "2026-11-02 12:00:01",
        }));
        insert_spend_log_rows(&mut *tx, &[minimal], now(), true).await?;
        let columns = SPEND_LOG_COLUMNS
            .iter()
            .filter(|column| **column != "request_id")
            .map(|column| format!("to_jsonb(\"{column}\")"))
            .collect::<Vec<_>>()
            .join(", ");
        #[expect(
            clippy::disallowed_methods,
            reason = "Test assertions compare every column at runtime"
        )]
        let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            r#"SELECT jsonb_build_array({columns}) FROM "LiteLLM_SpendLogs" WHERE request_id = ANY($1) ORDER BY request_id"#
        )))
        .bind(["defaults-native", "defaults-sql"].as_slice())
        .fetch_all(&mut *tx)
        .await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], rows[1]);
        tx.rollback().await?;
        Ok(())
    }

    #[rstest]
    #[case::non_finite_spend(SpendLogRecord { spend: f64::NAN, ..row("bad") }, Some("23502"))]
    #[case::nul_in_json(SpendLogRecord { response: Some(json!({"text": "a\u{0}b"})), ..row("bad") }, Some("22P05"))]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn a_row_postgres_cannot_store_rolls_back_the_whole_ledger(
        #[case] bad: SpendLogRecord,
        #[case] sqlstate: Option<&str>,
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let mut outer = pool.begin().await?;
        let mut tx = sqlx::Acquire::begin(&mut *outer).await?;
        let mut poisoned = entry("delivery-bad", "bad", true);
        poisoned.row = Some(bad);
        let result = insert_spend_log_ledger(
            &mut tx,
            &[entry("delivery-good", "good", true), poisoned],
            true,
            now(),
            true,
        )
        .await;
        let error = result.expect_err("the poisoned row must fail the ledger");
        assert_eq!(error.kind(), ErrorKind::DataRejected);
        assert_eq!(error.sqlstate().as_deref(), sqlstate);
        tx.rollback().await?;
        assert!(
            ids(&mut *outer, DELIVERIES, &["delivery-good", "delivery-bad"])
                .await?
                .is_empty()
        );
        outer.rollback().await?;
        Ok(())
    }

    #[rstest]
    #[case::expired(Duration::ZERO)]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn a_ledger_past_its_timeout_commits_nothing(
        #[case] timeout: Duration,
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let result = write_spend_log_ledger(
            &pool,
            &[entry("delivery-timeout", "timeout", true)],
            true,
            timeout,
            true,
        )
        .await;
        assert!(matches!(result, Err(Error::TransactionTimedOut)));
        assert!(
            ids(&pool, DELIVERIES, &["delivery-timeout"])
                .await?
                .is_empty()
        );
        Ok(())
    }
}
