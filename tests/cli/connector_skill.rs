use crate::helpers::*;

/// The connector skill's worked example
/// (skills/connector/example/local-files) must actually run the protocol
/// its SKILL.md documents: `connector link` registers it, `connector try`
/// runs its poll command and prints one item per file with no job file
/// required, and `tick --dry-run` against a job that uses it would create
/// one task per file, with no daemon running — the exact steps the skill's
/// checklist tells an agent to run to test what it scaffolded.
#[test]
fn connector_skill_worked_example_links_tries_and_dry_run_ticks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", state.join("data"))
            .output()
            .unwrap()
    };

    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills/connector/example/local-files");
    let linked = ok(run(&["connector", "link", example.to_str().unwrap()]));
    assert!(linked.contains("linked local-files"), "{linked}");

    let watched = tempfile::tempdir().unwrap();
    std::fs::write(watched.path().join("a.txt"), "").unwrap();
    std::fs::write(watched.path().join("b.txt"), "").unwrap();

    std::fs::write(
        config.join("jobs/local-files-test.toml"),
        format!(
            "every = \"1h\"\n[connector]\nuse = \"local-files\"\ndir = {:?}\n[dispatch]\nprompt = \"new file {{{{ item.title }}}}\"\n",
            watched.path()
        ),
    )
    .unwrap();

    let out = ok(run(&[
        "connector",
        "try",
        "local-files",
        "--job",
        "local-files-test",
    ]));
    let items: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(items.len(), 2, "{out}");
    let mut titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    titles.sort();
    assert_eq!(titles, ["a.txt", "b.txt"], "{out}");

    let out = ok(run(&[
        "tick",
        "--dry-run",
        "--job",
        "local-files-test",
        "--json",
    ]));
    let runs: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run", "{out}");
    assert_eq!(runs[0]["created"].as_array().unwrap().len(), 2, "{out}");

    let out = ok(run(&["task", "list", "--json"]));
    let tasks: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert!(tasks.is_empty(), "--dry-run must write nothing: {tasks:?}");
}
