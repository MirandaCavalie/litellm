use litellm_python_compat::{Value, truthy::truthy};

use crate::SpendLogRowBuilder;
use crate::privacy::{TOOL_OUTPUT_BLOCK_TYPES, error_information_without_ip_addresses};
use crate::py::{Dict, as_str, entries, get, get_or_none, has, safe_dumps, text};
use crate::text::masked_ip_text;
use crate::{RowError, row::RowInput};

const RESPONSES_CALL_TYPES: [&str; 2] = ["responses", "aresponses"];
const STATEFUL_REQUEST_KEYS: [&str; 2] = ["previous_response_id", "conversation"];
const TURNS_KEYS: [&str; 2] = ["messages", "input"];
const TEXT_BLOCK_TYPES: [&str; 2] = ["text", "input_text"];
const CLASSIFIER_AUDIT_FIELDS: [&str; 2] = ["classifier_input", "originating_request_masked"];
const REQUEST_METADATA_KEYS: [&str; 2] = ["metadata", "litellm_metadata"];
const CLIENT_IP_HEADERS: [&str; 18] = [
    "x-forwarded-for",
    "x-real-ip",
    "forwarded",
    "true-client-ip",
    "cf-connecting-ip",
    "x-client-ip",
    "x-cluster-client-ip",
    "fastly-client-ip",
    "x-original-forwarded-for",
    "x-envoy-external-address",
    "x-azure-clientip",
    "cloudfront-viewer-address",
    "fly-client-ip",
    "cf-connecting-ipv6",
    "cf-pseudo-ipv4",
    "x-appengine-user-ip",
    "x-vercel-forwarded-for",
    "x-arr-clientip",
];

fn omitted_block_placeholder(block_type: &str) -> Option<&'static str> {
    match block_type {
        "image" | "image_url" | "input_image" => Some("[image omitted]"),
        "file" | "input_file" | "document" => Some("[file omitted]"),
        "input_audio" => Some("[audio omitted]"),
        _ => None,
    }
}

fn parts_of(value: &Value) -> &[Value] {
    match value {
        Value::List(items) | Value::Tuple(items) => items,
        _ => &[],
    }
}

fn block_type(block: &Value) -> Option<&str> {
    as_str(get_or_none(block, "type"))
}

fn content_block_text(block: &Value) -> Option<String> {
    if let Value::Str(content) = block {
        return Some(content.clone());
    }
    let kind = block_type(block)?;
    if TEXT_BLOCK_TYPES.contains(&kind) {
        return as_str(get_or_none(block, "text")).map(str::to_owned);
    }
    omitted_block_placeholder(kind).map(str::to_owned)
}

fn user_content_text(content: &Value) -> Option<String> {
    if let Value::Str(content) = content {
        return (!content.is_empty()).then(|| content.clone());
    }
    let blocks = parts_of(content);
    if blocks
        .iter()
        .any(|block| block_type(block).is_some_and(|kind| TOOL_OUTPUT_BLOCK_TYPES.contains(&kind)))
    {
        return None;
    }
    let texts: Vec<String> = blocks
        .iter()
        .filter_map(content_block_text)
        .filter(|content| !content.is_empty())
        .collect();
    let joined = texts.join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn latest_user_message_text(turns: &Value) -> Option<String> {
    if let Value::Str(content) = turns {
        return (!content.is_empty()).then(|| content.clone());
    }
    let latest = parts_of(turns).last()?;
    if !matches!(get_or_none(latest, "role"), Value::Str(role) if role == "user") {
        return None;
    }
    user_content_text(get_or_none(latest, "content"))
}

fn continues_stored_conversation(body: &Value, call_type: &Value) -> bool {
    matches!(call_type, Value::Str(kind) if RESPONSES_CALL_TYPES.contains(&kind.as_str()))
        && STATEFUL_REQUEST_KEYS
            .iter()
            .any(|key| truthy(get_or_none(body, key)))
}

fn replayed_by_responses_bridge(payload: Option<&Value>) -> bool {
    let Some(payload) = payload else {
        return false;
    };
    matches!(get_or_none(payload, "call_type"), Value::Str(kind) if RESPONSES_CALL_TYPES.contains(&kind.as_str()))
        && matches!(get_or_none(payload, "response"), response @ Value::Dict(_) if has(response, "choices"))
}

fn kept_model(body: &Value) -> Dict {
    let mut kept = Dict::default();
    if let Some(model) = get(body, "model") {
        kept.set("model", model.clone());
    }
    kept
}

fn trimmed_turn(body: &Value, call_type: &Value) -> Value {
    if continues_stored_conversation(body, call_type) {
        return body.clone();
    }
    let mut kept = kept_model(body);
    let Some(turns_key) = TURNS_KEYS.iter().find(|key| has(body, key)) else {
        return kept.into_value();
    };
    let Some(latest) = latest_user_message_text(get_or_none(body, turns_key)) else {
        return kept.into_value();
    };
    kept.set(
        turns_key,
        Value::List(vec![Value::Dict(vec![
            (text("role"), text("user")),
            (text("content"), Value::Str(latest)),
        ])]),
    );
    kept.into_value()
}

fn latest_turn_body(body: &Value, call_type: &Value, originating: &Value) -> Value {
    if !matches!(originating, Value::Dict(_)) {
        return trimmed_turn(body, call_type);
    }
    let trimmed_originating = trimmed_turn(originating, &Value::None);
    let mut stored = Dict::from_entries(entries(&trimmed_originating).unwrap_or_default());
    if let Some(model) = get(body, "model") {
        stored.set("model", model.clone());
    }
    stored.set("originating_request_masked", trimmed_originating);
    stored.into_value()
}

fn without_requester_ip_address(value: &Value, headers: bool) -> Value {
    match value {
        Value::Dict(items) => Value::Dict(
            items
                .iter()
                .filter(|(key, _)| {
                    !(headers
                        && as_str(key).is_some_and(|name| {
                            CLIENT_IP_HEADERS.contains(&name.to_lowercase().as_str())
                        }))
                })
                .map(|(key, item)| {
                    let stored = match item {
                        Value::Str(content) if headers => Value::Str(masked_ip_text(content)),
                        _ => stored_value_without_requester_ip_address(key, item),
                    };
                    (key.clone(), stored)
                })
                .collect(),
        ),
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|item| without_requester_ip_address(item, false))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn stored_value_without_requester_ip_address(key: &Value, value: &Value) -> Value {
    match (as_str(key), value) {
        (Some("requester_ip_address"), _) => Value::None,
        (Some("error_information"), Value::Dict(_)) => {
            error_information_without_ip_addresses(value)
        }
        (name, _) => without_requester_ip_address(value, name == Some("headers")),
    }
}

fn request_metadata_without_requester_ip_address(body: &Value) -> Value {
    Value::Dict(
        entries(body)
            .unwrap_or_default()
            .iter()
            .map(|(key, value)| {
                let stored =
                    if as_str(key).is_some_and(|name| REQUEST_METADATA_KEYS.contains(&name)) {
                        without_requester_ip_address(value, false)
                    } else {
                        value.clone()
                    };
                (key.clone(), stored)
            })
            .collect(),
    )
}

fn stored_request_without_requester_ip_address(body: &Value) -> Value {
    let without_metadata = request_metadata_without_requester_ip_address(body);
    Value::Dict(
        entries(&without_metadata)
            .unwrap_or_default()
            .iter()
            .map(|(key, value)| {
                let stored = match value {
                    Value::Dict(_)
                        if as_str(key)
                            .is_some_and(|name| CLASSIFIER_AUDIT_FIELDS.contains(&name)) =>
                    {
                        request_metadata_without_requester_ip_address(value)
                    }
                    _ => value.clone(),
                };
                (key.clone(), stored)
            })
            .collect(),
    )
}

impl SpendLogRowBuilder {
    pub(crate) fn messages_json(&self, input: &RowInput<'_>) -> Result<String, RowError> {
        let Some(payload) = input.payload else {
            return Ok("{}".to_owned());
        };
        if !self.stores_prompts(input.metadata) {
            return Ok("{}".to_owned());
        }
        if !matches!(get_or_none(payload, "call_type"), Value::Str(kind) if kind == "_arealtime") {
            return Ok("{}".to_owned());
        }
        match get_or_none(payload, "messages") {
            Value::None => Ok("{}".to_owned()),
            messages => Ok(self.without_tokens(&safe_dumps(messages, true)?)),
        }
    }

    pub(crate) fn response_json(&self, input: &RowInput<'_>) -> Result<String, RowError> {
        let Some(payload) = input.payload else {
            return Ok("{}".to_owned());
        };
        if !self.stores_prompts(input.metadata) {
            return Ok("{}".to_owned());
        }
        let response = get_or_none(payload, "response");
        if matches!(response, Value::None) {
            return Ok("{}".to_owned());
        }
        let tool_calls = get_or_none(input.kwargs, "realtime_tool_calls");
        let response = match response {
            Value::Dict(items) if truthy(tool_calls) => {
                let mut with_tool_calls = Dict::from_entries(items);
                with_tool_calls.set("tool_calls", tool_calls.clone());
                with_tool_calls.into_value()
            }
            other => other.clone(),
        };
        let wrapper = self.sanitize_body(&[(text("response"), response)], true);
        match get_or_none(&wrapper, "response") {
            Value::None => Ok("{}".to_owned()),
            Value::Str(content) => Ok(content.replace('\0', "")),
            sanitized => safe_dumps(sanitized, true),
        }
    }

    pub(crate) fn proxy_server_request_json(
        &self,
        input: &RowInput<'_>,
        litellm_params: &Value,
    ) -> Result<String, RowError> {
        if !self.stores_prompts(input.metadata) {
            return Ok("{}".to_owned());
        }
        let proxy_server_request = match get(litellm_params, "proxy_server_request") {
            Some(Value::None) => return Ok("{}".to_owned()),
            Some(found) => found,
            None => &Value::Dict(Vec::new()),
        };
        let found_body = match proxy_server_request {
            Value::Dict(_) => get_or_none(proxy_server_request, "body"),
            _ => return Err(RowError::NotAMapping("proxy_server_request")),
        };
        let mut body = Dict::from_entries(if truthy(found_body) {
            entries(found_body).ok_or(RowError::NotAMapping("proxy_server_request.body"))?
        } else {
            &[]
        });
        if input.payload.is_some() {
            body.extend(entries(input.classifier_audit).unwrap_or_default());
        }
        let realtime_tools = get_or_none(input.kwargs, "realtime_tools");
        if truthy(realtime_tools) {
            body.set("tools", realtime_tools.clone());
        }
        let body = body.into_value();
        let selected =
            if !self.config.latest_turn_only || replayed_by_responses_bridge(input.payload) {
                body
            } else {
                match input.payload {
                    None => latest_turn_body(&body, &Value::None, &Value::None),
                    Some(payload) => latest_turn_body(
                        &body,
                        get_or_none(payload, "call_type"),
                        get_or_none(payload, "originating_request_masked"),
                    ),
                }
            };
        let stored = self.sanitize_body(entries(&selected).unwrap_or_default(), true);
        let stored = if self.config.drop_requester_ip_address {
            stored_request_without_requester_ip_address(&stored)
        } else {
            stored
        };
        safe_dumps(&stored, true)
    }
}
