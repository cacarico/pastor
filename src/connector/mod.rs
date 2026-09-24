//! The seam between the scheduler and whatever produces items: the built-in
//! `clock`, and process connectors from plugins (`process`) behind the same
//! trait. A `Catalog` says which connector ids exist. Named `ItemSource` in code only because `herdr::Connector` is
//! already the transport; wherever a user sees it, it is a connector.

pub mod clock;
pub mod process;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// One thing a connector found. `key` is the stable identity the seen-store
/// uses; `fields` is the whole object (always including `key`) and is what
/// templates see as `item.*` and what `tasks.item` stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub fields: Map<String, Value>,
}

impl Item {
    pub fn new(key: impl Into<String>, mut fields: Map<String, Value>) -> Item {
        let key = key.into();
        fields.insert("key".into(), Value::String(key.clone()));
        Item { key, fields }
    }

    pub fn as_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }
}

/// What a run receives; the same shape plan 3 will put on a plugin's stdin.
#[derive(Debug, Clone)]
pub struct RunInput {
    /// The job's `[connector]` table minus `use`.
    pub config: Value,
    pub cursor: Option<String>,
    /// Start of the last successful run, or `now - backfill` on the first.
    pub since: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    pub items: Vec<Item>,
    /// Persisted only when the run succeeds; `None` keeps the previous cursor.
    pub cursor: Option<String>,
    pub logs: Vec<String>,
}

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<RunOutput, String>> + Send + 'a>>;

pub trait ItemSource: Send + Sync {
    fn id(&self) -> &str;
    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a>;
}

/// The connectors pastor ships inside the binary.
pub fn builtin(id: &str) -> Option<Arc<dyn ItemSource>> {
    match id {
        "clock" => Some(Arc::new(clock::Clock)),
        _ => None,
    }
}

/// Which connectors exist and whether a job's `[connector]` table suits one.
/// Job files are validated against it at load, the scheduler resolves through
/// it at run time.
pub trait Catalog: Send + Sync {
    fn source(&self, id: &str) -> Option<Arc<dyn ItemSource>>;
    /// Err(reason) for a missing plugin or a config key the manifest requires.
    fn check(&self, id: &str, config: &Value) -> Result<(), String>;
    /// The source for `id` running on behalf of `job`. A catalog whose
    /// sources care which job they serve (plugins: `PASTOR_JOB`, run logs,
    /// scratch) overrides this; the default ignores the job.
    fn source_for_job(&self, id: &str, _job: &str) -> Option<Arc<dyn ItemSource>> {
        self.source(id)
    }
}

/// Only what ships in the binary: the clock. What a daemon with no plugins,
/// and most tests, use.
pub struct Builtins;

impl Catalog for Builtins {
    fn source(&self, id: &str) -> Option<Arc<dyn ItemSource>> {
        builtin(id)
    }

    fn check(&self, id: &str, _config: &Value) -> Result<(), String> {
        match builtin(id) {
            Some(_) => Ok(()),
            None => Err(format!(
                "connector {id:?} is not available: no built-in connector or installed plugin has that id"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_clock_is_built_in() {
        let none = serde_json::json!({});
        assert!(Builtins.check("clock", &none).is_ok());
        assert_eq!(Builtins.source("clock").unwrap().id(), "clock");
        let err = Builtins.check("slack", &none).unwrap_err();
        assert!(
            err.contains("slack") && err.contains("not available"),
            "{err}"
        );
        assert!(Builtins.source("slack").is_none());
        assert!(builtin("").is_none());
    }

    #[test]
    fn item_always_carries_its_key_in_fields() {
        let mut fields = serde_json::Map::new();
        fields.insert("title".into(), serde_json::Value::String("x".into()));
        let item = Item::new("k1", fields);
        assert_eq!(item.as_value()["key"], "k1");
        assert_eq!(item.as_value()["title"], "x");
    }
}
