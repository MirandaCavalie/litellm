use litellm_config::{GeneralSettings, Value};
use litellm_env::Lookup;
use litellm_python_compat::{Value as PythonValue, flags, number, pydantic, unicode::isspace};

use crate::Error;

const DEFAULT_CONNECTION_LIMIT: u64 = 10;
const DEFAULT_POOL_TIMEOUT: f64 = 60.0;
const DEFAULT_IDLE_LIFETIME: f64 = 60.0;
const POOL_TIMEOUT_ALIAS: &str = "database_connection_pool_timeout";
const DISABLE_PREPARED_STATEMENTS: &str = "DATABASE_DISABLE_PREPARED_STATEMENTS";
const SESSION_TIMEOUTS: [(&str, &str); 3] = [
    ("statement_timeout", "database_statement_timeout"),
    ("lock_timeout", "database_lock_timeout"),
    (
        "idle_in_transaction_session_timeout",
        "database_idle_in_transaction_session_timeout",
    ),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PoolSettings {
    pub connection_limit: u32,
    pub pool_timeout: f64,
    pub socket_timeout: Option<f64>,
    pub idle_lifetime: f64,
    pub unnamed_statements: bool,
    pub session_timeouts: Vec<(&'static str, i64)>,
}

impl PoolSettings {
    pub(crate) fn read(general: &GeneralSettings, lookup: &dyn Lookup) -> Result<Self, Error> {
        let extra = &general.additional_fields;
        let connection_limit = general
            .database_connection_pool_limit
            .unwrap_or(DEFAULT_CONNECTION_LIMIT);
        let pool_timeout = match general.database_connection_timeout {
            Some(seconds) => seconds,
            None => seconds(POOL_TIMEOUT_ALIAS, extra.get(POOL_TIMEOUT_ALIAS))?
                .unwrap_or(DEFAULT_POOL_TIMEOUT),
        };
        let session_timeouts = SESSION_TIMEOUTS
            .iter()
            .map(
                |(name, key)| -> Result<Option<(&'static str, i64)>, Error> {
                    let configured = seconds(key, extra.get(key))?;
                    Ok(configured.map(|value| (*name, (value * 1000.0) as i64)))
                },
            )
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self {
            connection_limit: u32::try_from(connection_limit)
                .ok()
                .filter(|limit| *limit > 0)
                .ok_or(Error::Invalid {
                    setting: "general_settings.database_connection_pool_limit".to_owned(),
                    expected: "a positive integer",
                })?,
            pool_timeout,
            socket_timeout: general.database_socket_timeout,
            idle_lifetime: general
                .database_max_idle_connection_lifetime
                .unwrap_or(DEFAULT_IDLE_LIFETIME),
            unnamed_statements: disables(extra.get("database_disable_prepared_statements"))
                || flag(
                    DISABLE_PREPARED_STATEMENTS,
                    lookup.get(DISABLE_PREPARED_STATEMENTS).as_deref(),
                )?,
            session_timeouts,
        })
    }
}

pub(crate) fn flag(variable: &'static str, value: Option<&str>) -> Result<bool, Error> {
    let Some(text) = value
        .map(|text| text.trim_matches(isspace))
        .filter(|text| !text.is_empty())
    else {
        return Ok(false);
    };
    pydantic::lax_bool(&PythonValue::Str(text.to_owned())).ok_or(Error::Invalid {
        setting: variable.to_owned(),
        expected: "a boolean",
    })
}

fn disables(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(disabled)) => *disabled,
        Some(Value::String(text)) => flags::str_to_bool(text) == Some(true),
        _ => false,
    }
}

fn seconds(key: &'static str, value: Option<&Value>) -> Result<Option<f64>, Error> {
    let invalid = || Error::Invalid {
        setting: format!("general_settings.{key}"),
        expected: "a number of seconds",
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => number::float(text).map(Some).ok_or_else(invalid),
        Some(other) => other.as_f64().map(Some).ok_or_else(invalid),
    }
}
