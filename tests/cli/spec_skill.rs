use crate::helpers::*;

/// The spec skill's worked example is a plan whose first task must run as
/// written: its Dispatch command goes through `pastor task run` unchanged
/// apart from the paths. In a real repo the prompt file sits under
/// `docs/superpowers/plans/`, and the repo is a path on the flock machine.
#[test]
fn spec_example_plan_runs_its_first_task() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/spec/example");
    let plan = std::fs::read_dir(&example)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "md") && !p.to_string_lossy().ends_with(".ledger.md")
        })
        .expect("an example plan");
    let text = std::fs::read_to_string(&plan).unwrap();
    let words = first_dispatch_command(&text);
    assert_eq!(&words[..3], ["pastor", "task", "run"], "{words:?}");
    let mut args: Vec<String> = words[1..].to_vec();
    let at = args.iter().position(|w| w == "--prompt-file").unwrap() + 1;
    let in_repo = args[at].clone();
    let file = example.join(
        in_repo
            .strip_prefix("docs/superpowers/plans/")
            .unwrap_or_else(|| panic!("prompt file {in_repo} is outside the plans dir")),
    );
    args[at] = file.to_string_lossy().into_owned();
    // The fake is a `command` machine with no home for a `~` to name.
    let at = args.iter().position(|w| w == "--repo").unwrap() + 1;
    assert!(args[at].starts_with('~'), "{args:?}");
    args[at] = "/tmp/pastor".to_string();
    assert!(args.iter().any(|w| w == "--json"), "{args:?}");

    let env = start();
    // The plan names its model from `[models]`, as a real head has it.
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [models.sonnet]\nkind = \"claude\"\nargs = [\"--model\", \"claude-sonnet-5\"]\n",
    )
    .unwrap();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&refs))).unwrap();
    let prompt = std::fs::read_to_string(&file).unwrap();
    assert_eq!(t["prompt"], prompt.trim_end(), "{t}");
    // The prompt owns the branch handling, so it must say where to push.
    assert!(prompt.contains("git push origin HEAD:"), "{prompt}");
    let last = prompt.trim_end().lines().last().unwrap();
    assert!(last.contains("DONE"), "{prompt}");
    env.wait_done(&format!("t-{}", t["id"]));
}

#[test]
fn spec_example_prompts_rebase_before_the_ledger_and_retry_the_push() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/spec/example");
    let mut checked = 0;
    for dir in std::fs::read_dir(&example).unwrap() {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        // A plan dir is `<date>-<name>`; the plan branch is `pastor/<name>`.
        let stem = dir.file_name().unwrap().to_string_lossy().into_owned();
        let name = stem.get(11..).expect("a dated plan dir");
        let rebase = format!("git rebase origin/pastor/{name}");
        let ledger_commit = format!("{stem}.ledger.md and commit it");
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            if file.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let p = std::fs::read_to_string(&file).unwrap();
            let at = |needle: &str| {
                p.find(needle)
                    .unwrap_or_else(|| panic!("{} lacks {needle:?}", file.display()))
            };
            assert!(
                at(&rebase) < at(&ledger_commit),
                "{} must rebase before the ledger commit",
                file.display()
            );
            let step = p.lines().find(|l| l.contains(&rebase)).unwrap();
            assert!(
                step.contains("git fetch origin"),
                "{} must fetch in the rebase step: {step}",
                file.display()
            );
            at("rejected as not a fast-forward, do steps 1 and 3 once more");
            at("keep both sides' lines");
            at("PUSH FAILED");
            at(&format!("git push origin HEAD:pastor/{name}"));
            assert_eq!(
                p.trim_end().lines().last(),
                Some("Print DONE as your last line."),
                "{} must end by printing DONE",
                file.display()
            );
            checked += 1;
        }
    }
    assert!(checked >= 2, "only {checked} prompt files checked");
}
