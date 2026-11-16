use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};

use crate::{Error, Statements};

pub const SPEND_LOG_COLUMNS: [&str; 36] = [
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

pub const SPEND_LOG_JSON_COLUMNS: [&str; 5] = [
    "metadata",
    "request_tags",
    "messages",
    "response",
    "proxy_server_request",
];

fn column_default(column: &str, now: &str) -> Value {
    match column {
        "api_key"
        | "model"
        | "model_id"
        | "model_group"
        | "custom_llm_provider"
        | "api_base"
        | "user"
        | "cache_hit"
        | "cache_key" => json!(""),
        "spend" => json!(0.0),
        "total_tokens" | "prompt_tokens" | "completion_tokens" => json!(0),
        "metadata" | "messages" | "response" | "proxy_server_request" => json!({}),
        "request_tags" => json!([]),
        "created_at" | "updated_at" => json!(now),
        _ => Value::Null,
    }
}

pub fn spend_log_timestamp(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

pub fn spend_log_record(row: &Map<String, Value>, now: &str) -> Result<Map<String, Value>, Error> {
    if let Some(unknown) = row
        .keys()
        .find(|key| !SPEND_LOG_COLUMNS.contains(&key.as_str()))
    {
        return Err(Error::RejectedSpendLogRow(format!(
            "unknown spend log column {unknown}"
        )));
    }
    Ok(SPEND_LOG_COLUMNS
        .iter()
        .map(|column| {
            let value = row
                .get(*column)
                .cloned()
                .unwrap_or_else(|| column_default(column, now));
            ((*column).to_owned(), value)
        })
        .collect())
}

pub async fn insert_spend_log_rows(
    executor: impl PgExecutor<'_>,
    rows: &[Map<String, Value>],
    now: &str,
    named: bool,
) -> Result<Vec<String>, Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let records = Value::Array(
        rows.iter()
            .map(|row| spend_log_record(row, now).map(Value::Object))
            .collect::<Result<_, _>>()?,
    );
    Ok(
        sqlx::query_file_scalar!("queries/spend/insert_spend_log_rows.sql", records)
            .fetch_all(Statements::new(executor, named))
            .await?,
    )
}

#[macro_rules_attribute::apply(crate::db_input)]
pub struct SpendLogDelivery {
    pub delivery_id: String,
    pub request_id: Value,
    pub accounting_payload: Map<String, Value>,
    pub is_batch: bool,
}

#[macro_rules_attribute::apply(crate::db_input)]
pub struct SpendLogLedgerEntry {
    pub delivery: SpendLogDelivery,
    pub row: Option<Map<String, Value>>,
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

pub async fn insert_spend_log_ledger(
    transaction: &mut Transaction<'_, Postgres>,
    entries: &[SpendLogLedgerEntry],
    write_spend_logs: bool,
    now: &str,
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
    let rows: Vec<Map<String, Value>> = ledger
        .iter()
        .filter(|entry| kept(&entry.delivery.delivery_id))
        .filter_map(|entry| entry.row.clone())
        .collect();
    if write_spend_logs {
        insert_spend_log_rows(&mut **transaction, &rows, now, named).await?;
    }
    Ok(accepted.iter().filter(|id| kept(id)).count())
}

pub async fn write_spend_log_ledger(
    pool: &PgPool,
    entries: &[SpendLogLedgerEntry],
    write_spend_logs: bool,
    timeout: Duration,
    named: bool,
) -> Result<usize, Error> {
    let now = spend_log_timestamp(Utc::now());
    let commit = async {
        let mut transaction = pool.begin().await?;
        let accepted =
            insert_spend_log_ledger(&mut transaction, entries, write_spend_logs, &now, named)
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

    use super::{
        SPEND_LOG_COLUMNS, SpendLogDelivery, SpendLogLedgerEntry, insert_spend_log_ledger,
        insert_spend_log_rows, write_spend_log_ledger,
    };
    use crate::{Error, ErrorKind, test_support::database_pool};

    const NOW: &str = "2026-11-02T12:00:00.000000+00:00";

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn row(request_id: &str) -> Map<String, Value> {
        object(json!({
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

    fn entry(delivery_id: &str, request_id: &str, logged: bool) -> SpendLogLedgerEntry {
        SpendLogLedgerEntry {
            delivery: SpendLogDelivery {
                delivery_id: delivery_id.into(),
                request_id: json!(request_id),
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
            insert_spend_log_ledger(&mut tx, &ledger, write_spend_logs, NOW, true).await?;
        assert_eq!(accepted, 3);
        let replay = insert_spend_log_ledger(&mut tx, &ledger, write_spend_logs, NOW, true).await?;
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
        let mut legacy = row("job-1");
        legacy.insert("spend".into(), json!(legacy_spend));
        legacy.insert("status".into(), json!(legacy_status));
        insert_spend_log_rows(&mut *tx, &[legacy], NOW, true).await?;
        let job = SpendLogLedgerEntry {
            delivery: SpendLogDelivery {
                delivery_id: "job-1".into(),
                request_id: json!("job-1"),
                accounting_payload: object(json!({"spend": 0.25})),
                is_batch: true,
            },
            row: Some(row("job-1")),
        };
        let accepted = insert_spend_log_ledger(&mut tx, &[job], true, NOW, true).await?;
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
               VALUES ('defaults-sql', 'acompletion', '2026-11-02 12:00:00', '2026-11-02 12:00:01', $1::timestamp, $1::timestamp)"#,
        )
        .bind(NOW)
        .execute(&mut *tx)
        .await?;
        let minimal = object(json!({
            "request_id": "defaults-native",
            "call_type": "acompletion",
            "startTime": "2026-11-02 12:00:00",
            "endTime": "2026-11-02 12:00:01",
        }));
        insert_spend_log_rows(&mut *tx, &[minimal], NOW, true).await?;
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
    #[case::unknown_column(json!({"request_id": "bad", "unexpected": 1}), ErrorKind::DataRejected, None)]
    #[case::invalid_number(json!({"request_id": "bad", "call_type": "x", "startTime": "2026-11-02", "endTime": "2026-11-02", "spend": "abc"}), ErrorKind::DataRejected, Some("22P02"))]
    #[case::missing_start(json!({"request_id": "bad", "call_type": "x", "endTime": "2026-11-02"}), ErrorKind::DataRejected, Some("23502"))]
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing to a Prisma-migrated PostgreSQL database"]
    async fn a_row_postgres_cannot_store_rolls_back_the_whole_ledger(
        #[case] bad: Value,
        #[case] kind: ErrorKind,
        #[case] sqlstate: Option<&str>,
        #[future(awt)] database_pool: Result<PgPool, Box<dyn StdError>>,
    ) -> Result<(), Box<dyn StdError>> {
        let pool = database_pool?;
        let mut outer = pool.begin().await?;
        let mut tx = sqlx::Acquire::begin(&mut *outer).await?;
        let mut poisoned = entry("delivery-bad", "bad", true);
        poisoned.row = Some(object(bad));
        let result = insert_spend_log_ledger(
            &mut tx,
            &[entry("delivery-good", "good", true), poisoned],
            true,
            NOW,
            true,
        )
        .await;
        let error = result.expect_err("the poisoned row must fail the ledger");
        assert_eq!(error.kind(), kind);
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
