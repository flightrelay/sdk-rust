//! Driver configuration: serde structs with validation helpers.
//!
//! Requires the `config` feature, which needs nothing beyond the crate's
//! base dependencies, so a crate that only describes a device's settings (a
//! settings form, a WebAssembly UI) can use it without any device stack.
//!
//! A driver's settings are a plain struct that implements [`DriverConfig`].
//! Every field has a default, and every field is left out of the serialized
//! form while it holds that default ([`is_default`]), so writing a default
//! config writes nothing. A field whose default is not its type's uses
//! [`is_default_value`] instead. Both stop skipping while a `--help` text is
//! generated, which is how that text lists every key.
//!
//! A field that is `None` by default (an address that is blank until the user
//! sets one) is listed too, as unset, when [`DriverConfig::example`] returns a
//! config that has it filled in. `--set` accepts such a field with or without
//! `example`: a key is known when setting it changes the config.
//!
//! ```
//! use flightrelay::config::{ConfigError, DriverConfig, is_default};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
//! #[serde(default)]
//! struct Config {
//!     #[serde(skip_serializing_if = "is_default")]
//!     address: Option<String>,
//!     #[serde(skip_serializing_if = "is_default")]
//!     range: u32,
//! }
//!
//! impl DriverConfig for Config {
//!     fn validate(&self) -> Result<(), ConfigError> {
//!         if self.range <= 100 {
//!             Ok(())
//!         } else {
//!             Err(ConfigError::field("range", "must be 0 to 100"))
//!         }
//!     }
//! }
//!
//! let config = Config::from_map(
//!     serde_json::json!({ "range": 25 }).as_object().cloned().unwrap_or_default(),
//! )?;
//! assert_eq!(config.range, 25);
//! assert_eq!(config.to_map().len(), 1); // `address` is at its default
//! let bad = Config::from_map(
//!     serde_json::json!({ "range": 250 }).as_object().cloned().unwrap_or_default(),
//! );
//! assert_eq!(bad.unwrap_err().to_string(), "range: must be 0 to 100");
//! # Ok::<(), ConfigError>(())
//! ```

use std::cell::Cell;
use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Settings of one driver.
///
/// Implementors are plain serde structs. Of the provided methods, a driver
/// overrides only [`validate`](Self::validate),
/// [`restart_needed`](Self::restart_needed) and [`example`](Self::example).
/// See the [module docs](self) for the conventions a config follows.
pub trait DriverConfig:
    Serialize + DeserializeOwned + Default + Clone + PartialEq + fmt::Debug + Send + Sync + 'static
{
    /// Range and consistency checks that serde cannot express. Messages name
    /// the field and the allowed range, e.g. `"must be 0 to 1000"`.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] for a value that must be refused.
    fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }

    /// Whether moving from `old` to `new` needs a new session. The default is
    /// any change. A driver that applies some fields live overrides this and
    /// `DeviceDriver::apply_config` (feature `driver`).
    #[must_use]
    fn restart_needed(old: &Self, new: &Self) -> bool {
        old != new
    }

    /// A config with every optional field filled in, for the key list of a
    /// `--help` text. The default is [`Default::default`], which lists every
    /// field that serializes at its default; override it when a field is
    /// `None` by default, or skipped while empty, so that the key is listed
    /// too (shown as unset). Only the keys of the result matter, not its
    /// values.
    ///
    /// ```
    /// use flightrelay::config::{DriverConfig, is_default};
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    /// #[serde(default)]
    /// struct Config {
    ///     #[serde(skip_serializing_if = "Option::is_none")]
    ///     address: Option<String>,
    ///     #[serde(skip_serializing_if = "is_default")]
    ///     range: u32,
    /// }
    ///
    /// impl DriverConfig for Config {
    ///     fn example() -> Self {
    ///         Self { address: Some(String::new()), ..Self::default() }
    ///     }
    /// }
    /// # #[cfg(feature = "main")]
    /// assert!(Config::help_toml().contains("# address = (unset)"));
    /// ```
    #[must_use]
    fn example() -> Self {
        Self::default()
    }

    /// Read a config from a JSON object, then [`validate`](Self::validate) it.
    /// Keys that are absent keep their defaults.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the field when one key has the wrong
    /// type, or whatever [`validate`](Self::validate) refuses.
    fn from_map(map: Map<String, Value>) -> Result<Self, ConfigError> {
        let config: Self = match serde_json::from_value(Value::Object(map.clone())) {
            Ok(config) => config,
            Err(error) => return Err(locate::<Self>(&map, &error)),
        };
        config.validate()?;
        Ok(config)
    }

    /// The config as a JSON object, leaving out every field at its default.
    #[must_use]
    fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }

    /// Read a config from a TOML document holding the config's keys at the
    /// top level, then [`validate`](Self::validate) it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] for a document that is not TOML, a key with
    /// the wrong type, or a value [`validate`](Self::validate) refuses.
    #[cfg(feature = "main")]
    fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let table: toml::Table = toml::from_str(s).map_err(|e| ConfigError::new(e.message()))?;
        match crate::cli::toml_to_json(&toml::Value::Table(table)) {
            Value::Object(map) => Self::from_map(map),
            _ => Err(ConfigError::new("expected a table")),
        }
    }

    /// Every key of the default config with its default, as TOML, for a
    /// `--help` text. A key whose default is unset is listed as a comment,
    /// e.g. `# range = (unset)`; so is a key only [`example`](Self::example)
    /// has.
    #[cfg(feature = "main")]
    #[must_use]
    fn help_toml() -> String {
        Self::default().help_toml_of()
    }

    /// Like [`help_toml`](Self::help_toml), for the defaults `self` holds.
    #[cfg(feature = "main")]
    #[must_use]
    fn help_toml_of(&self) -> String {
        crate::cli::render_toml(&with_unset(listed(self), listed(&Self::example())))
    }
}

/// `skip_serializing_if` function for driver config fields: `true` while the
/// value equals its type's default, so a default config serializes to
/// nothing.
///
/// It compares with the field *type's* `Default`. A field whose default in
/// the config's own `Default` is something else (a flag that defaults to
/// `true`, a number that defaults to 10) needs its own predicate built on
/// [`is_default_value`]: with `is_default`, a user's explicit `false` or `0`
/// would be left out when the config is written, and read back as the
/// config's default.
///
/// Returns `false` while a `--help` text is being generated (the
/// `DriverConfig::help_toml` default method, feature `main`), so that text
/// can list every key with its default.
#[must_use]
pub fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    !LIST_DEFAULTS.with(Cell::get) && *v == T::default()
}

/// For a field whose default is `default`, not its type's: `true` while `v`
/// equals it, and `false` while a `--help` text is being generated, like
/// [`is_default`]. Wrap it in the `skip_serializing_if` function of such a
/// field:
///
/// ```
/// use flightrelay::config::is_default_value;
///
/// fn is_default_range(v: &u32) -> bool {
///     is_default_value(v, &10)
/// }
/// assert!(is_default_range(&10));
/// assert!(!is_default_range(&0));
/// ```
#[must_use]
pub fn is_default_value<T: PartialEq>(v: &T, default: &T) -> bool {
    !LIST_DEFAULTS.with(Cell::get) && v == default
}

thread_local! {
    static LIST_DEFAULTS: Cell<bool> = const { Cell::new(false) };
}

/// `config` serialized with every field present, `None` as `null`: the keys
/// `--help` lists and `--set` accepts.
#[cfg(feature = "main")]
pub(crate) fn listed<C: Serialize>(config: &C) -> Value {
    let _listing = ListDefaults::on();
    serde_json::to_value(config).unwrap_or(Value::Null)
}

/// `listed` of a config, plus every key only `example` has, as unset (`null`;
/// a table of them for a table).
#[cfg(feature = "main")]
fn with_unset(listed: Value, example: Value) -> Value {
    match (listed, example) {
        (Value::Object(mut keys), Value::Object(example)) => {
            for (key, example) in example {
                let value = match keys.remove(&key) {
                    Some(value) => with_unset(value, example),
                    None => unset(example),
                };
                keys.insert(key, value);
            }
            Value::Object(keys)
        }
        (Value::Null, example @ Value::Object(_)) => unset(example),
        (listed, _) => listed,
    }
}

/// `value` with every leaf `null`.
#[cfg(feature = "main")]
fn unset(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, unset(v))).collect()),
        _ => Value::Null,
    }
}

/// Makes [`is_default`] report `false` on this thread until dropped.
#[cfg(feature = "main")]
struct ListDefaults;

#[cfg(feature = "main")]
impl ListDefaults {
    fn on() -> Self {
        LIST_DEFAULTS.with(|c| c.set(true));
        Self
    }
}

#[cfg(feature = "main")]
impl Drop for ListDefaults {
    fn drop(&mut self) {
        LIST_DEFAULTS.with(|c| c.set(false));
    }
}

/// A config that is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{}", .field.as_deref().map(|f| format!("{f}: ")).unwrap_or_default(), .message)]
#[non_exhaustive]
pub struct ConfigError {
    /// The serde field name (dotted for nested fields), when one field is at
    /// fault.
    pub field: Option<String>,
    /// What is wrong, e.g. `"must be 0 to 1000"`.
    pub message: String,
}

impl ConfigError {
    /// An error that names no field.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            field: None,
            message: message.into(),
        }
    }

    /// An error about `field`.
    #[must_use]
    pub fn field(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: Some(field.into()),
            message: message.into(),
        }
    }
}

/// Finds the key behind a deserialization error, as a dotted path into
/// nested tables. Every field has a default, so a map holding only the
/// offending key (inside only the offending tables) fails the same way.
fn locate<C: DriverConfig>(map: &Map<String, Value>, error: &serde_json::Error) -> ConfigError {
    let mut path = Vec::new();
    match descend::<C>(&mut path, &Value::Object(map.clone())) {
        Some(message) if !path.is_empty() => ConfigError::field(path.join("."), message),
        _ => ConfigError::new(error.to_string()),
    }
}

/// Finds the deepest key of `value` (at `path`) that fails on its own,
/// leaving `path` pointing at it, and returns the failure.
fn descend<C: DriverConfig>(path: &mut Vec<String>, value: &Value) -> Option<String> {
    let Value::Object(entries) = value else {
        return None;
    };
    let mut entries: Vec<(&String, &Value)> = entries.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    for (key, child) in entries {
        path.push(key.clone());
        if let Err(e) = serde_json::from_value::<C>(nest(path, child.clone())) {
            return descend::<C>(path, child).or_else(|| Some(e.to_string()));
        }
        path.pop();
    }
    None
}

/// `value` wrapped in one single-key object per element of `path`.
fn nest(path: &[String], value: Value) -> Value {
    path.iter().rev().fold(value, |inner, key| {
        let mut one = Map::new();
        one.insert(key.clone(), inner);
        Value::Object(one)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Inner {
        #[serde(skip_serializing_if = "is_default")]
        gain: f64,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Middle {
        #[serde(skip_serializing_if = "is_default")]
        inner: Inner,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Cfg {
        #[serde(skip_serializing_if = "is_default")]
        range: u32,
        #[serde(skip_serializing_if = "is_default")]
        tuning: Inner,
        #[serde(skip_serializing_if = "is_default")]
        deep: Middle,
    }
    impl DriverConfig for Cfg {}

    fn map(value: &serde_json::Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn an_error_in_a_nested_table_names_the_dotted_path() {
        let e =
            Cfg::from_map(map(&serde_json::json!({ "tuning": { "gain": "abc" } }))).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("tuning.gain"), "{e}");
        assert!(e.to_string().starts_with("tuning.gain: "), "{e}");

        let e = Cfg::from_map(map(
            &serde_json::json!({ "deep": { "inner": { "gain": [1] } }, "range": 3 }),
        ))
        .unwrap_err();
        assert_eq!(e.field.as_deref(), Some("deep.inner.gain"), "{e}");

        let e = Cfg::from_map(map(&serde_json::json!({ "range": "x" }))).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("range"), "{e}");
    }
}
