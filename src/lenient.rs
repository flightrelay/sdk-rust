//! Lenient serde readers: a field whose value is not recognized reads as
//! absent, so the rest of the message still applies.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Reads an optional field, mapping `null` and any value that is not a known
/// one to `None`.
pub(crate) fn option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Reads a field that may be absent, `null` or a value, keeping the
/// difference: absent is `None` (with `#[serde(default)]`), `null` is
/// `Some(None)`, a known value is `Some(Some(_))`. A value that is not a known
/// one reads as absent.
#[allow(clippy::option_option)] // absent, null and a value are three different things on the wire
pub(crate) fn change<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(Some(None));
    }
    Ok(serde_json::from_value(value).ok().map(Some))
}

/// Writes a [`change`] field that is present: `null` or the value. Pair it
/// with `skip_serializing_if = "Option::is_none"`.
#[allow(clippy::option_option, clippy::ref_option)] // serde passes `&Option<Option<_>>`
pub(crate) fn write_change<S, T>(
    value: &Option<Option<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    match value {
        Some(inner) => inner.serialize(serializer),
        None => serializer.serialize_none(),
    }
}

/// Reads a list, leaving out each entry that is not a known value (e.g. an
/// unknown club code in a bag). A value that is not a list reads as empty.
pub(crate) fn list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// Reads an optional string-keyed map, leaving out each entry whose value is
/// not a known one. `null` and a value that is not an object read as `None`.
pub(crate) fn map<'de, D, T>(
    deserializer: D,
) -> Result<Option<std::collections::BTreeMap<String, T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Object(entries)) => Some(
            entries
                .into_iter()
                .filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?)))
                .collect(),
        ),
        _ => None,
    })
}
