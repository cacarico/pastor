//! `{{ item.title }}` substitution for prompts, branches and repos. No logic,
//! no filters, no escapes: the spec asks for substitution only, and a template
//! engine would invite exactly the conditionals it rules out. A missing path
//! renders empty and is reported, so one odd item cannot fail a job forever.

use serde_json::Value;

pub struct Rendered {
    pub text: String,
    /// Paths that had no value in the context, in order of appearance.
    pub missing: Vec<String>,
}

/// Every `{{ path }}` in `template`, in order. Errors on an unterminated `{{`
/// or on a path that is not dotted identifiers, so a job file can be checked
/// at load time.
pub fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some((_, path, after)) = next_placeholder(rest)? {
        out.push(path.to_string());
        rest = after;
    }
    Ok(out)
}

pub fn render(template: &str, ctx: &Value) -> Result<Rendered, String> {
    let mut text = String::with_capacity(template.len());
    let mut missing = Vec::new();
    let mut rest = template;
    while let Some((before, path, after)) = next_placeholder(rest)? {
        text.push_str(before);
        match lookup(ctx, path) {
            Some(v) => text.push_str(&scalar(v)),
            None => missing.push(path.to_string()),
        }
        rest = after;
    }
    text.push_str(rest);
    Ok(Rendered { text, missing })
}

/// `(literal before, path, rest after the closing braces)` for the next
/// placeholder, or `None` when there is no `{{` left.
fn next_placeholder(rest: &str) -> Result<Option<(&str, &str, &str)>, String> {
    let Some(start) = rest.find("{{") else {
        return Ok(None);
    };
    let after_open = &rest[start + 2..];
    let Some(end) = after_open.find("}}") else {
        let shown: String = rest[start..].chars().take(30).collect();
        return Err(format!("unterminated placeholder near {shown:?}"));
    };
    let path = after_open[..end].trim();
    let well_formed = !path.is_empty()
        && path.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if !well_formed {
        return Err(format!(
            "bad placeholder {{{{ {path} }}}}: expected dotted names like item.title"
        ));
    }
    Ok(Some((&rest[..start], path, &after_open[end + 2..])))
}

fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(ctx, |v, seg| v.get(seg))
}

/// Strings raw, null empty, everything else as compact JSON.
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_dotted_paths_and_leaves_the_rest_alone() {
        let ctx = json!({
            "item": {"key": "k1", "title": "login broken", "n": 3, "ok": true, "tags": ["a", "b"], "none": null},
            "job": {"name": "support"},
            "task": {"id": "t-7"}
        });
        let r = render(
            "[{{ job.name }}] {{item.title}} #{{ item.n }} {{ item.ok }} {{ item.tags }} '{{ item.none }}' {{ task.id }}\n{ not a placeholder }",
            &ctx,
        )
        .unwrap();
        assert_eq!(
            r.text,
            "[support] login broken #3 true [\"a\",\"b\"] '' t-7\n{ not a placeholder }"
        );
        assert!(r.missing.is_empty());
    }

    #[test]
    fn missing_paths_render_empty_and_are_reported() {
        let ctx = json!({"item": {"key": "k"}, "job": {"name": "j"}, "task": {"id": "t-1"}});
        let r = render("a {{ item.title }} b {{ item.meta.deep }} c", &ctx).unwrap();
        assert_eq!(r.text, "a  b  c");
        assert_eq!(r.missing, vec!["item.title", "item.meta.deep"]);
    }

    #[test]
    fn placeholders_lists_paths_and_rejects_bad_syntax() {
        assert_eq!(
            placeholders("{{ item.key }}/{{job.name}} {{ task.id }}").unwrap(),
            vec!["item.key", "job.name", "task.id"]
        );
        assert!(placeholders("no braces").unwrap().is_empty());
        assert!(
            placeholders("{{ item.key ")
                .unwrap_err()
                .contains("unterminated")
        );
        assert!(
            placeholders("{{ }}")
                .unwrap_err()
                .contains("bad placeholder")
        );
        assert!(
            placeholders("{{ item.a-b }}")
                .unwrap_err()
                .contains("bad placeholder")
        );
        assert!(
            placeholders("{{ item..key }}")
                .unwrap_err()
                .contains("bad placeholder")
        );
        assert!(
            placeholders("{{ item | upper }}")
                .unwrap_err()
                .contains("bad placeholder")
        );
        // render rejects the same input, so a bad template never half-renders
        assert!(render("{{ item.key ", &json!({})).is_err());
    }
}
