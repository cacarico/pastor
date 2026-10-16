use crate::helpers::*;

/// `pastor status` sums up a running head: the fake machine up, a queued
/// task, the job and no limits; `--json` carries the same parts.
#[test]
fn status_sums_up_a_running_head() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    wait_for_machine(&env, "fake", true);
    ok(env.cmd(&["flock", "add", "lab"]));
    ok(env.cmd(&["task", "run", "waits", "--flock", "lab"]));

    let text = ok(env.cmd(&["status"]));
    let line = |label: &str| {
        text.lines()
            .find(|l| l.starts_with(label))
            .unwrap_or_else(|| panic!("no {label} line: {text}"))
            .to_string()
    };
    assert!(line("head").contains(env!("CARGO_PKG_VERSION")), "{text}");
    assert!(line("machines").ends_with("1 up"), "{text}");
    assert!(line("tasks").ends_with("1 queued"), "{text}");
    assert!(line("jobs").ends_with("1 enabled of 1"), "{text}");
    assert!(line("limits").ends_with("none"), "{text}");

    let v: serde_json::Value = serde_json::from_str(&ok(env.cmd(&["status", "--json"]))).unwrap();
    assert_eq!(v["head"]["running"], true, "{v}");
    assert_eq!(v["head"]["at"], "here", "{v}");
    assert_eq!(v["machines"]["up"], 1, "{v}");
    assert_eq!(v["tasks"]["queued"], 1, "{v}");
    assert_eq!(v["jobs"]["total"], 1, "{v}");
    assert_eq!(v["limits"], serde_json::json!([]), "{v}");
}

/// With no head, `status` stops with `not_running` and names `--probe`;
/// with it, it reads the files here and says it probed.
#[test]
fn status_without_a_head_needs_probe() {
    let o = offline();
    let out = o.cmd(&["status"]);
    assert_eq!(error_code(&out), "not_running");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--probe"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = ok(o.cmd(&["status", "--probe"]));
    assert!(text.starts_with("head      not running; probed"), "{text}");
    assert!(text.contains("machines  none"), "{text}");
    assert!(text.contains("jobs      1 enabled of 1"), "{text}");
    let v: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["status", "--probe", "--json"]))).unwrap();
    assert_eq!(v["head"]["running"], false, "{v}");
}
