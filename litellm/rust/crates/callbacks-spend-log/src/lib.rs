mod accounting;
mod privacy;
mod py;
mod request;
mod row;
mod text;

use litellm_python_compat::{Value, json, number, pydantic};
use litellm_tracing::TokenRedactor;

use crate::py::get_or_none;

pub use accounting::{build_accounting_payload, finalize_ledger_row};

pub const INPUT_VERSION: i64 = 1;
const DEFAULT_MAX_STRING_LENGTH_PROMPT_IN_DB: i64 = 2048;

#[derive(Debug, thiserror::Error)]
pub enum RowError {
    #[error("spend log input version {0} is not {INPUT_VERSION}")]
    Version(String),
    #[error("{0} is missing")]
    Missing(&'static str),
    #[error("{0} is not a mapping")]
    NotAMapping(&'static str),
    #[error("{0} has an unexpected type")]
    Type(&'static str),
    #[error(transparent)]
    Json(#[from] litellm_python_compat::Error),
}

pub struct RowConfig {
    store_prompts_in_spend_logs: Value,
    store_prompts_in_spend_logs_env: bool,
    latest_turn_only: bool,
    redact_tokens: bool,
    drop_requester_ip_address: bool,
    omit_missing_session_id: bool,
    max_string_length_prompt_in_db: i64,
    disable_end_user_cost_tracking: bool,
    request_correlation_in_logs: bool,
}

impl RowConfig {
    pub fn from_settings(settings: &Value) -> Result<Self, RowError> {
        check_version(settings)?;
        let flag = |key: &str| matches!(get_or_none(settings, key), Value::Bool(true));
        let drop_ip = get_or_none(settings, "disable_requester_ip_address_logging");
        Ok(Self {
            store_prompts_in_spend_logs: get_or_none(settings, "store_prompts_in_spend_logs")
                .clone(),
            store_prompts_in_spend_logs_env: flag("store_prompts_in_spend_logs_env"),
            latest_turn_only: matches!(
                get_or_none(settings, "spend_logs_prompt_storage"),
                Value::Str(storage) if storage == "latest_turn"
            ),
            redact_tokens: flag("redact_tokens_in_spend_logs"),
            drop_requester_ip_address: !matches!(drop_ip, Value::None)
                && pydantic::lax_bool(drop_ip) == Some(true),
            omit_missing_session_id: matches!(
                get_or_none(settings, "missing_session_id"),
                Value::Str(policy) if policy == "omit"
            ),
            max_string_length_prompt_in_db: match get_or_none(
                settings,
                "max_string_length_prompt_in_db",
            ) {
                Value::Str(raw) => number::int(raw)
                    .map(|length| {
                        i64::try_from(&length).unwrap_or(
                            if length.sign() == litellm_python_compat::BigInt::from(1).sign() {
                                i64::MAX
                            } else {
                                i64::MIN
                            },
                        )
                    })
                    .unwrap_or(DEFAULT_MAX_STRING_LENGTH_PROMPT_IN_DB),
                _ => DEFAULT_MAX_STRING_LENGTH_PROMPT_IN_DB,
            },
            disable_end_user_cost_tracking: flag("disable_end_user_cost_tracking"),
            request_correlation_in_logs: flag("request_correlation_in_logs"),
        })
    }
}

fn check_version(value: &Value) -> Result<(), RowError> {
    match get_or_none(value, "version") {
        Value::Int(version) if *version == litellm_python_compat::BigInt::from(INPUT_VERSION) => {
            Ok(())
        }
        other => Err(RowError::Version(litellm_python_compat::repr::repr(other))),
    }
}

pub struct SpendLogRowBuilder {
    config: RowConfig,
    tokens: TokenRedactor,
}

impl SpendLogRowBuilder {
    pub fn new(config: RowConfig, tokens: TokenRedactor) -> Self {
        Self { config, tokens }
    }

    pub fn build_row(&self, input: &Value) -> Result<Value, RowError> {
        check_version(input)?;
        row::build(self, input)
    }

    pub fn build_row_json(&self, input: &str) -> Result<String, RowError> {
        Ok(json::dumps(&self.build_row(&json::loads(input)?)?)?)
    }
}
