//! Schedule-only jobs: one item per run, keyed by the run time, so a job with
//! no external source still goes through the same seen-store and templates.

use chrono::SecondsFormat;
use serde_json::{Map, Value};

use super::{Item, ItemSource, RunFuture, RunInput, RunOutput};

pub struct Clock;

impl ItemSource for Clock {
    fn id(&self) -> &str {
        "clock"
    }

    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
        Box::pin(async move {
            let at = input.now.to_rfc3339_opts(SecondsFormat::Secs, true);
            let mut fields = Map::new();
            fields.insert("title".into(), Value::String(format!("clock {at}")));
            fields.insert("at".into(), Value::String(at.clone()));
            Ok(RunOutput {
                items: vec![Item::new(at, fields)],
                cursor: None,
                logs: Vec::new(),
                batch: 0,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[tokio::test]
    async fn one_item_per_run_keyed_by_the_run_time() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 5).unwrap();
        let out = Clock
            .run(RunInput {
                config: serde_json::json!({}),
                cursor: None,
                since: now,
                now,
            })
            .await
            .unwrap();
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        assert_eq!(item.key, "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["key"], "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["at"], "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["title"], "clock 2026-09-24T10:00:05Z");
        assert!(out.cursor.is_none());
    }
}
