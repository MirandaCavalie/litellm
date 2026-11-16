use litellm_db::{Error, SPEND_LOG_JSON_COLUMNS, SpendLogDelivery, SpendLogLedgerEntry};
use litellm_python_compat::{
    Value,
    json::{dumps, loads},
    repr::to_str,
};
use serde_json::{Map, Number};

use crate::{
    SpendLogRow,
    row::{ACCOUNTING_DELIVERY_ID, ACCOUNTING_LOG_DISABLED, ACCOUNTING_PAYLOAD},
};

const MAX_DEPTH_EXCEEDED: &str = "MaxDepthExceeded";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub always_kept: Vec<String>,
}

impl Retention {
    pub fn keeps(&self, name: &str) -> bool {
        if self.always_kept.iter().any(|kept| kept == name) {
            return true;
        }
        match (&self.include, &self.exclude) {
            (Some(include), _) => include.iter().any(|field| field == name),
            (None, exclude) => !exclude.iter().flatten().any(|field| field == name),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("spend log metadata is not a JSON object")]
pub struct InvalidMetadata;

pub fn retain_metadata(
    row: &SpendLogRow,
    retention: Option<&Retention>,
    max_depth: usize,
) -> Result<SpendLogRow, InvalidMetadata> {
    let (Some(retention), Some(Value::Str(metadata))) = (retention, row.get("metadata")) else {
        return Ok(row.clone());
    };
    let Ok(Value::Dict(entries)) = loads(metadata) else {
        return Err(InvalidMetadata);
    };
    let retained = entries
        .into_iter()
        .map(|(key, value)| match key {
            Value::Str(key) => Ok((key, value)),
            _ => Err(InvalidMetadata),
        })
        .filter(|entry| match entry {
            Ok((key, _)) => retention.keeps(key),
            Err(_) => true,
        })
        .map(|entry| entry.map(|(key, value)| (Value::Str(key), value)))
        .collect::<Result<Vec<_>, _>>()?;
    let text = sanitized(&Value::Dict(retained), 0, max_depth).to_string();
    let reparsed = loads(&text).map_err(|_| InvalidMetadata)?;
    Ok(row.with(
        "metadata",
        Value::Str(dumps(&reparsed).map_err(|_| InvalidMetadata)?),
    ))
}

pub fn jsonified(row: &SpendLogRow) -> SpendLogRow {
    SpendLogRow::new(
        row.entries()
            .iter()
            .map(|(key, value)| match value {
                Value::Dict(_) => (
                    key.clone(),
                    Value::Str(
                        dumps(value).unwrap_or_else(|_| "failed-to-serialize-json".to_owned()),
                    ),
                ),
                _ => (key.clone(), value.clone()),
            })
            .collect(),
    )
}

fn strip_nul(text: &str) -> String {
    text.replace('\0', "")
}

pub fn sanitized(value: &Value, depth: usize, max_depth: usize) -> serde_json::Value {
    if depth > max_depth {
        return serde_json::Value::String(MAX_DEPTH_EXCEEDED.to_owned());
    }
    match value {
        Value::None => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Int(value) => {
            let text = value.to_string();
            text.parse::<i64>()
                .map(Number::from)
                .or_else(|_| text.parse::<u64>().map(Number::from))
                .ok()
                .or_else(|| text.parse::<f64>().ok().and_then(Number::from_f64))
                .map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        Value::Float(value) => {
            Number::from_f64(*value).map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        Value::Str(text) => serde_json::Value::String(strip_nul(text)),
        Value::List(values) | Value::Tuple(values) | Value::Set(values) => {
            serde_json::Value::Array(
                values
                    .iter()
                    .map(|value| sanitized(value, depth + 1, max_depth))
                    .collect(),
            )
        }
        Value::Dict(entries) => serde_json::Value::Object(
            entries
                .iter()
                .filter_map(|(key, value)| match key {
                    Value::Str(key) => {
                        Some((strip_nul(key), sanitized(value, depth + 1, max_depth)))
                    }
                    _ => None,
                })
                .collect(),
        ),
        other => serde_json::Value::String(strip_nul(&to_str(other))),
    }
}

fn column_value(column: &str, value: &Value, max_depth: usize) -> Result<serde_json::Value, Error> {
    match value {
        Value::Str(text) if SPEND_LOG_JSON_COLUMNS.contains(&column) => loads(text)
            .map(|parsed| sanitized(&parsed, 2, max_depth))
            .map_err(|_| {
                Error::RejectedSpendLogRow(format!("Spend log column {column} is not valid JSON"))
            }),
        value => Ok(sanitized(value, 2, max_depth)),
    }
}

fn spend_log_values(
    row: &SpendLogRow,
    max_depth: usize,
) -> Result<Map<String, serde_json::Value>, Error> {
    row.entries()
        .iter()
        .filter(|(key, _)| {
            ![
                ACCOUNTING_PAYLOAD,
                ACCOUNTING_LOG_DISABLED,
                ACCOUNTING_DELIVERY_ID,
            ]
            .contains(&key.as_str())
        })
        .map(|(key, value)| Ok((key.clone(), column_value(key, value, max_depth)?)))
        .collect()
}

fn accounting_payload(value: &Value) -> Result<Value, Error> {
    let invalid =
        || Error::InvalidSpendLogDelivery("accounting_payload is not a JSON object".into());
    let payload = match value {
        Value::Str(text) => loads(text).map_err(|_| invalid())?,
        value => value.clone(),
    };
    match &payload {
        Value::Dict(entries) if entries.iter().all(|(key, _)| matches!(key, Value::Str(_))) => {
            Ok(payload)
        }
        _ => Err(invalid()),
    }
}

pub fn delivery_id(row: &SpendLogRow) -> String {
    match row
        .get(ACCOUNTING_DELIVERY_ID)
        .or_else(|| row.get("request_id"))
    {
        Some(value) => to_str(value),
        None => "None".to_owned(),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LedgerRequest {
    pub legacy: Vec<Map<String, serde_json::Value>>,
    pub ledger: Vec<SpendLogLedgerEntry>,
}

pub fn ledger_request(rows: &[SpendLogRow], max_depth: usize) -> Result<LedgerRequest, Error> {
    let mut request = LedgerRequest::default();
    for row in rows {
        let Some(payload) = row.get(ACCOUNTING_PAYLOAD) else {
            request.legacy.push(spend_log_values(row, max_depth)?);
            continue;
        };
        let serde_json::Value::Object(accounting_payload) =
            sanitized(&accounting_payload(payload)?, 2, max_depth)
        else {
            return Err(Error::InvalidSpendLogDelivery(
                "accounting_payload is not a JSON object".into(),
            ));
        };
        request.ledger.push(SpendLogLedgerEntry {
            delivery: SpendLogDelivery {
                delivery_id: delivery_id(row),
                request_id: row
                    .get("request_id")
                    .map_or(serde_json::Value::Null, |value| {
                        sanitized(value, 2, max_depth)
                    }),
                accounting_payload,
                is_batch: row.get(ACCOUNTING_DELIVERY_ID).is_none(),
            },
            row: if row.log_disabled() {
                None
            } else {
                Some(spend_log_values(row, max_depth)?)
            },
        });
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use litellm_db::ErrorKind;
    use rstest::rstest;
    use serde_json::json;

    use super::{Retention, jsonified, ledger_request, retain_metadata};
    use crate::SpendLogRow;

    fn row(text: &str) -> SpendLogRow {
        SpendLogRow::parse(text).expect("row parses")
    }

    #[rstest]
    #[case::include(Retention { include: Some(vec!["a".into()]), exclude: None, always_kept: vec!["k".into()] }, r#"{"a": 1, "k": 3}"#)]
    #[case::exclude(Retention { include: None, exclude: Some(vec!["a".into()]), always_kept: vec![] }, r#"{"b": 2, "k": 3}"#)]
    fn retains_configured_metadata_fields(#[case] retention: Retention, #[case] metadata: &str) {
        let retained = retain_metadata(
            &row(r#"{"request_id": "r", "metadata": "{\"a\": 1, \"b\": 2, \"k\": 3}"}"#),
            Some(&retention),
            100,
        )
        .expect("metadata is an object");

        assert_eq!(
            retained.get("metadata"),
            Some(&litellm_python_compat::Value::Str(metadata.into()))
        );
    }

    #[rstest]
    #[case::not_json(r#"{"metadata": "nope"}"#)]
    #[case::not_an_object(r#"{"metadata": "[1]"}"#)]
    fn invalid_metadata_fails_retention(#[case] text: &str) {
        assert!(retain_metadata(&row(text), Some(&Retention::default()), 100).is_err());
    }

    #[rstest]
    #[case::dict_becomes_text(
        r#"{"accounting_payload": {"spend": 1.0}}"#,
        r#"{"accounting_payload": "{\"spend\": 1.0}"}"#
    )]
    #[case::list_stays(r#"{"messages": [1]}"#, r#"{"messages": [1]}"#)]
    fn jsonify_dumps_top_level_dicts(#[case] text: &str, #[case] expected: &str) {
        assert_eq!(jsonified(&row(text)).to_json(), expected);
    }

    #[rstest]
    #[case::ledger_row(
        r#"{"request_id": "r", "accounting_delivery_id": "d", "accounting_payload": "{\"spend\": NaN}", "accounting_log_disabled": false, "metadata": "{\"a\": \"x\\u0000y\"}"}"#,
        json!({"legacy": [], "ledger": [["d", "r", {"spend": null}, false, {"request_id": "r", "metadata": {"a": "xy"}}]]})
    )]
    #[case::batch_job_without_log(
        r#"{"request_id": "job", "accounting_payload": {"spend": 2}, "accounting_log_disabled": true}"#,
        json!({"legacy": [], "ledger": [["job", "job", {"spend": 2}, true, null]]})
    )]
    #[case::legacy_row(
        r#"{"request_id": "old", "request_tags": "[\"t\"]"}"#,
        json!({"legacy": [{"request_id": "old", "request_tags": ["t"]}], "ledger": []})
    )]
    fn builds_the_ledger_request(#[case] text: &str, #[case] expected: serde_json::Value) {
        let request = ledger_request(&[jsonified(&row(text))], 100).expect("request builds");
        let ledger: Vec<serde_json::Value> = request
            .ledger
            .iter()
            .map(|entry| {
                json!([
                    entry.delivery.delivery_id,
                    entry.delivery.request_id,
                    entry.delivery.accounting_payload,
                    entry.delivery.is_batch,
                    entry.row,
                ])
            })
            .collect();

        assert_eq!(
            json!({"legacy": request.legacy, "ledger": ledger}),
            expected
        );
    }

    #[rstest]
    #[case::invalid_json_column(
        r#"{"request_id": "r", "accounting_payload": {}, "messages": "not json"}"#,
        ErrorKind::DataRejected
    )]
    #[case::payload_not_an_object(
        r#"{"request_id": "r", "accounting_payload": "[1]"}"#,
        ErrorKind::Other
    )]
    fn refuses_rows_it_cannot_encode(#[case] text: &str, #[case] kind: ErrorKind) {
        let error = ledger_request(&[row(text)], 100).expect_err("row is refused");

        assert_eq!(error.kind(), kind);
    }
}
