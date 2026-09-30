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

/// next-task.sh is the mechanical part shared by both dispatch modes: it
/// reads a plan and its ledger and prints exactly one of RUN, WAIT, CHECK,
/// COMPLETE or BLOCKED. Run against the same worked example
/// `spec_example_plan_runs_its_first_task` uses, since the plan format is
/// the contract both share.
#[test]
fn next_task_reads_the_example_plan_and_its_ledger() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/spec/example");
    let plan_src = std::fs::read_dir(&example)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "md") && !p.to_string_lossy().ends_with(".ledger.md")
        })
        .expect("an example plan");
    let stem = plan_src.file_stem().unwrap().to_string_lossy().into_owned();
    let ledger_src = example.join(format!("{stem}.ledger.md"));
    let header = std::fs::read_to_string(&ledger_src).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let plan = tmp.path().join(plan_src.file_name().unwrap());
    let ledger = tmp.path().join(ledger_src.file_name().unwrap());
    std::fs::copy(&plan_src, &plan).unwrap();

    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/dispatch/next-task.sh");
    // A `pastor` stand-in for the PENDING branch: next-task.sh only ever
    // calls `task describe t-<id> --json` and reads its "state" field.
    let fake_pastor = tmp.path().join("fake-pastor");
    let run = |ledger_text: &str, pastor_state: Option<&str>| -> String {
        std::fs::write(&ledger, ledger_text).unwrap();
        let mut cmd = Command::new("sh");
        common::scrub(&mut cmd);
        cmd.arg(&script).arg(&plan);
        if let Some(state) = pastor_state {
            std::fs::write(
                &fake_pastor,
                format!("#!/bin/sh\necho '{{\"state\": \"{state}\"}}'\n"),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_pastor, std::fs::Permissions::from_mode(0o755)).unwrap();
            cmd.env("PASTOR", &fake_pastor);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };

    // Task 1 has never run.
    assert_eq!(
        run(&header, None),
        format!("RUN 1 docs/superpowers/plans/{stem}/task-1.md")
    );

    // Its agent is still working.
    let running = format!("{header}\nTask 1: ran as t-14 on pi-3\n");
    assert_eq!(run(&running, Some("running")), "WAIT 1 t-14");

    // It stopped and needs a decision.
    assert_eq!(run(&running, Some("done")), "CHECK 1 t-14 done");

    // Task 1 is complete: task 2 is next.
    let task2 = format!(
        "{header}\nTask 1: complete (a1b2c3d..e4f5a6b, make check: pass)\nTask 1: ran as t-14 on pi-3\n"
    );
    assert_eq!(
        run(&task2, None),
        format!("RUN 2 docs/superpowers/plans/{stem}/task-2.md")
    );

    // The ledger itself says task 2 is blocked.
    let blocked = format!("{task2}Task 2: blocked: the manual has no troubleshooting section\n");
    assert_eq!(
        run(&blocked, None),
        "BLOCKED 2 the manual has no troubleshooting section"
    );

    // Both tasks are complete.
    let done = format!(
        "{task2}Task 2: ran as t-15 on pi-3\nTask 2: complete (e4f5a6b..a1b2c3d, make check: pass)\n"
    );
    assert_eq!(run(&done, None), "COMPLETE");
}
