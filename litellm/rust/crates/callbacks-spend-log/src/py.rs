use litellm_python_compat::{BigInt, Value, repr, truthy::truthy};
use num_traits::ToPrimitive;

use crate::RowError;

pub(crate) const NONE: Value = Value::None;

pub(crate) fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    entries(value)?
        .iter()
        .find(|(name, _)| matches!(name, Value::Str(name) if name == key))
        .map(|(_, item)| item)
}

pub(crate) fn get_or_none<'a>(value: &'a Value, key: &str) -> &'a Value {
    get(value, key).unwrap_or(&NONE)
}

pub(crate) fn has(value: &Value, key: &str) -> bool {
    get(value, key).is_some()
}

pub(crate) fn entries(value: &Value) -> Option<&[(Value, Value)]> {
    match value {
        Value::Dict(entries) => Some(entries),
        _ => None,
    }
}

pub(crate) fn dict_get<'a>(
    value: &'a Value,
    key: &str,
    field: &'static str,
) -> Result<&'a Value, RowError> {
    match value {
        Value::Dict(_) => Ok(get_or_none(value, key)),
        _ => Err(RowError::NotAMapping(field)),
    }
}

pub(crate) fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::Str(text) => Some(text),
        _ => None,
    }
}

pub(crate) fn is_true(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

pub(crate) fn or<'a>(first: &'a Value, second: &'a Value) -> &'a Value {
    if truthy(first) { first } else { second }
}

pub(crate) fn text(value: &str) -> Value {
    Value::Str(value.to_owned())
}

pub(crate) fn to_str(value: &Value) -> String {
    repr::to_str(value)
}

pub(crate) fn string_or_empty(value: &Value) -> Value {
    match value {
        Value::Str(_) => value.clone(),
        _ => text(""),
    }
}

pub(crate) fn int(value: i64) -> Value {
    Value::Int(BigInt::from(value))
}

pub(crate) fn big_to_f64(value: &BigInt) -> Option<f64> {
    value.to_f64().filter(|number| number.is_finite())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Dict(Vec<(Value, Value)>);

impl Dict {
    pub(crate) fn from_entries(entries: &[(Value, Value)]) -> Self {
        Self(entries.to_vec())
    }

    pub(crate) fn set(&mut self, key: &str, value: Value) {
        match self
            .0
            .iter_mut()
            .find(|(name, _)| matches!(name, Value::Str(name) if name == key))
        {
            Some(slot) => slot.1 = value,
            None => self.0.push((text(key), value)),
        }
    }

    pub(crate) fn set_value(&mut self, key: Value, value: Value) {
        match self.0.iter_mut().find(|(name, _)| *name == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub(crate) fn extend(&mut self, entries: &[(Value, Value)]) {
        for (key, value) in entries {
            self.set_value(key.clone(), value.clone());
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(name, _)| matches!(name, Value::Str(name) if name == key))
            .map(|(_, value)| value)
    }

    pub(crate) fn into_value(self) -> Value {
        Value::Dict(self.0)
    }
}

pub(crate) fn chars(value: &str) -> Vec<char> {
    value.chars().collect()
}

pub(crate) fn char_len(value: &str) -> usize {
    value.chars().count()
}

fn clamp_index(index: i64, length: i64) -> i64 {
    if index < 0 {
        (index + length).max(0)
    } else {
        index.min(length)
    }
}

pub(crate) fn slice(value: &str, start: Option<i64>, end: Option<i64>) -> String {
    let characters = chars(value);
    let length = characters.len() as i64;
    let from = clamp_index(start.unwrap_or(0), length);
    let to = clamp_index(end.unwrap_or(length), length);
    if from >= to {
        return String::new();
    }
    characters[from as usize..to as usize].iter().collect()
}

pub(crate) struct Structure {
    pub(crate) non_finite_as_null: bool,
    pub(crate) integer_limit: Option<i64>,
}

const MAX_DEPTH: usize = 100;

pub(crate) fn safe_structure(value: &Value, options: &Structure) -> Value {
    structure_at(value, 0, options)
}

fn structure_at(value: &Value, depth: usize, options: &Structure) -> Value {
    if depth > MAX_DEPTH {
        return text("MaxDepthExceeded");
    }
    match value {
        Value::Str(content) => Value::Str(content.replace('\0', "")),
        Value::Int(number) => match options.integer_limit {
            Some(limit) => Value::Int(
                number
                    .clone()
                    .clamp(BigInt::from(-limit), BigInt::from(limit)),
            ),
            None => value.clone(),
        },
        Value::Float(number) if options.non_finite_as_null && !number.is_finite() => Value::None,
        Value::None | Value::Bool(_) | Value::Float(_) => value.clone(),
        Value::Dict(items) => {
            let mut result = Dict::default();
            for (key, item) in items {
                if let Value::Str(key) = key {
                    let cleaned = key.replace('\0', "");
                    let stored = structure_at(item, depth + 1, options);
                    result.set_value(Value::Str(cleaned), stored);
                }
            }
            result.into_value()
        }
        Value::List(items) | Value::Tuple(items) | Value::Set(items) => Value::List(
            items
                .iter()
                .map(|item| structure_at(item, depth + 1, options))
                .collect(),
        ),
        other => Value::Str(repr::to_str(other).replace('\0', "")),
    }
}

pub(crate) fn safe_dumps(value: &Value, non_finite_as_null: bool) -> Result<String, RowError> {
    let structure = safe_structure(
        value,
        &Structure {
            non_finite_as_null,
            integer_limit: None,
        },
    );
    Ok(litellm_python_compat::json::dumps(&structure)?)
}
