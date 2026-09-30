use crate::helpers::*;

/// `pastor completions` must offer the real, nested command tree, and never
/// the old top-level spellings (`run`, `list`, `attach`, `reload`) or
/// `machine status`: pastor is fresh software, and those names are gone, not
/// merely hidden. This guards against a regression re-adding them.
#[test]
fn completions_offer_only_the_nested_spellings() {
    let gen_ = |shell: &str| {
        let out = pastor().args(["completions", shell]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let fish = gen_("fish");
    let bash = gen_("bash");
    let top = bash
        .lines()
        .find(|l| {
            l.trim_start()
                .starts_with("opts=\"-h -V --skill --head --help --version")
        })
        .unwrap_or_else(|| panic!("no top-level opts line:\n{bash}"));
    assert!(fish.contains("-l skill"), "fish does not offer --skill");
    for old in ["run", "list", "attach", "reload"] {
        assert!(
            !fish.contains(&format!("__fish_pastor_needs_command\" -f -a \"{old}\"")),
            "fish offers {old}"
        );
        assert!(
            !top.split_whitespace().any(|w| w.trim_matches('"') == old),
            "bash offers {old}: {top}"
        );
        assert!(
            !bash.contains(&format!("pastor,{old})")),
            "bash knows {old}"
        );
    }
    assert!(fish.contains("-f -a \"run\" -d 'Create a one-off task and dispatch it'"));
    assert!(bash.contains("pastor__subcmd__task,run)"));
    assert!(bash.contains("pastor__subcmd__job,reload)"));
    // `machine status` never appears one level down either.
    assert!(
        !bash.contains("pastor__subcmd__machine,status)"),
        "bash knows machine status"
    );
    // `serve status` is a command of its own; only machine's is gone.
    assert!(
        !fish
            .lines()
            .any(|l| l.contains("using_subcommand machine") && l.contains("-f -a \"status\"")),
        "fish offers machine status"
    );
    assert!(fish.contains("-f -a \"status\" -d 'Whether pastor serve runs here"));
    assert!(bash.contains("pastor__subcmd__machine,list)"));
}

/// `pastor --skill` is how an agent on any machine gets the guide, so it must
/// print the embedded file and nothing else: no log line, no trailing text.
#[test]
fn skill_flag_prints_the_embedded_skill_and_nothing_else() {
    let out = pastor().arg("--skill").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("---\nname: pastor\n"), "{stdout}");
    assert_eq!(stdout, include_str!("../../skills/pastor/SKILL.md"));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With a command it would be ambiguous which one ran; clap refuses it.
    let out = pastor().args(["--skill", "task", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{:?}", out.status);
}

/// An agent reads `--help` first; the footer is what sends it to the skill.
#[test]
fn help_footer_points_agents_at_the_skill() {
    let out = pastor().arg("--help").output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    let footer = help.trim_end().lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        footer.iter().any(|l| l.contains("pastor --skill")),
        "{help}"
    );
    assert!(
        footer.iter().any(|l| l.contains("already in your context")),
        "{help}"
    );
}

/// clap leaves a visible alias out of the `help` subtree, so `pastor help
/// task describe` would not complete; the completion tree adds it back.
#[test]
fn completions_offer_aliases_under_help() {
    let gen_ = |shell: &str| {
        let out = pastor().args(["completions", shell]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let bash = gen_("bash");
    assert!(
        bash.contains("pastor__subcmd__help__subcmd__task,describe)"),
        "bash help task has no describe case"
    );
    let help_task = bash
        .split("pastor__subcmd__help__subcmd__task)")
        .nth(1)
        .and_then(|s| s.lines().find(|l| l.trim_start().starts_with("opts=")))
        .expect("bash help task opts");
    assert!(
        help_task
            .split_whitespace()
            .any(|w| w.trim_matches('"') == "describe"),
        "bash help task opts: {help_task}"
    );
    let fish = gen_("fish");
    assert!(
        fish.lines().any(|l| l
            .contains("__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task")
            && l.contains("-a \"describe\"")),
        "fish help task has no describe"
    );
}

#[test]
fn complete_offers_job_names() {
    let (_tmp, config, state) = completion_config();
    for words in [
        &["job", "describe", ""][..],
        &["job", "run", "tr"],
        &["tick", "--job", ""],
        &["connector", "try", "github-issues", "--job", ""],
        &["task", "list", "--json", "--job=n"],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert_eq!(out, "nightly\ntriage\n", "{words:?}");
    }
    // A slot that takes no name is not completed from pastor's names, and
    // neither is a name that has already been given.
    for words in [
        &["job", ""][..],
        &["job", "describe", "--"],
        &["job", "describe", "nightly", ""],
        &["task", "read", "t-1", "--lines", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(!ok, "{words:?}: {out}");
        assert!(out.is_empty(), "{words:?}: {out}");
    }
}

#[test]
fn complete_offers_flock_names() {
    let (_tmp, config, state) = completion_config();
    for words in [
        &["flock", "describe", ""][..],
        &["flock", "default", "set", ""],
        &["machine", "move", "pi-1", ""],
        &["flock", "join", ""],
        &["flock", "leave", ""],
        &["task", "run", "fix it", "--flock", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert_eq!(out, "home\tdefault\nlab\n", "{words:?}");
    }
    // A new flock's name is the user's to choose.
    let (ok, _) = complete(&config, &state, &["flock", "add", ""]);
    assert!(!ok);
}

#[test]
fn complete_offers_machine_names() {
    let (_tmp, config, state) = completion_config();
    for words in [
        &["machine", "describe", ""][..],
        &["machine", "move", ""],
        &["machine", "open", ""],
        &["flock", "join", "lab", ""],
        &["flock", "join", "lab", "--max", "2", ""],
        &["flock", "leave", "home", ""],
        &["flock", "add", "new", ""],
        &["flock", "add", "new", "pi-1", ""],
        &["task", "list", "--machine", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert_eq!(out, "pi-1\thome\npi-2\tlab\n", "{words:?}");
    }
    let (ok, _) = complete(&config, &state, &["machine", "add", ""]);
    assert!(!ok);
}

/// fish shows a job's, flock's or machine's description beside its name;
/// one without keeps what it showed before.
#[test]
fn complete_shows_descriptions_beside_names() {
    let (_tmp, config, state) = completion_config();
    std::fs::write(
        config.join("jobs/nightly.toml"),
        "description = \"Run the suite at night\"\nevery = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"lab\"\ndescription = \"Test rigs\"\n\n\
         [[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\nflock = \"home\"\ndescription = \"The desk one\"\n\n\
         [[machine]]\nname = \"pi-2\"\nssh = \"user@pi-2\"\nflock = \"lab\"\n",
    )
    .unwrap();
    let (ok, out) = complete(&config, &state, &["job", "describe", ""]);
    assert!(ok);
    assert_eq!(out, "nightly\tRun the suite at night\ntriage\n");
    let (ok, out) = complete(&config, &state, &["flock", "describe", ""]);
    assert!(ok);
    assert_eq!(out, "home\tdefault\nlab\tTest rigs\n");
    let (ok, out) = complete(&config, &state, &["machine", "describe", ""]);
    assert!(ok);
    assert_eq!(out, "pi-1\tThe desk one\npi-2\tlab\n");
}

#[test]
fn complete_offers_connector_ids() {
    let (_tmp, config, state) = completion_config();
    for words in [
        &["connector", "uninstall", ""][..],
        &["connector", "unlink", ""],
        &["connector", "try", ""],
        &["watch", "--connector", ""],
        &["watch", "--now", "--connector=g"],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert_eq!(out, "github-issues\n", "{words:?}");
    }
}

/// `--model` and `--fallback` both take names from `[models]`.
#[test]
fn complete_offers_model_names_after_model_and_fallback() {
    let (_tmp, config, state) = completion_config();
    std::fs::write(
        config.join("pastor.toml"),
        "[models.sonnet]\nkind = \"claude\"\nargs = []\n[models.gpt]\nkind = \"opencode\"\nargs = []\n",
    )
    .unwrap();
    for words in [
        &["task", "run", "fix it", "--model", ""][..],
        &["task", "run", "fix it", "--fallback", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert!(out.contains("sonnet\tclaude\n"), "{words:?}: {out}");
        assert!(out.contains("gpt\topencode\n"), "{words:?}: {out}");
    }
}

/// `--fallback` takes a comma-separated list as one word, so after
/// `sonnet,` pastor completes the next name and puts the names already typed
/// before each candidate, in both the `--fallback x` and `--fallback=x`
/// forms. No other slot splits on commas.
#[test]
fn complete_prefixes_the_names_already_typed_in_a_fallback_list() {
    let (_tmp, config, state) = fallback_completion_config();
    for words in [
        &["task", "run", "fix it", "--fallback", "sonnet,g"][..],
        &["task", "run", "fix it", "--fallback=sonnet,g"],
        &["task", "run", "fix it", "--fallback", "=", "sonnet,g"],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert_eq!(
            out, "sonnet,gpt\topencode\nsonnet,sonnet\tclaude\n",
            "{words:?}"
        );
    }
    let (ok, out) = complete(
        &config,
        &state,
        &["task", "run", "fix it", "--model", "a,g"],
    );
    assert!(ok);
    assert_eq!(out, "gpt\topencode\nsonnet\tclaude\n");
    let (ok, out) = complete(&config, &state, &["machine", "describe", "west,e"]);
    assert!(ok);
    assert_eq!(out, "west,east\thome\n");
}

/// bash filters pastor's answer against the whole word under the cursor:
/// `sonnet,` finishes a second fallback name, and a machine named
/// `west,east` still completes from `west,e`.
#[test]
fn bash_completion_finishes_a_second_fallback_name_and_comma_names() {
    let (_tmp, config, state) = fallback_completion_config();
    // Alphabetical, as `[models]` is a `BTreeMap`: "gpt" before "sonnet".
    let reply = bash_complete(
        &config,
        &state,
        r#"pastor task run "fix it" --fallback "sonnet,""#,
        5,
    );
    assert_eq!(reply, ["sonnet,gpt", "sonnet,sonnet"]);
    let reply = bash_complete(
        &config,
        &state,
        r#"pastor task run "fix it" --fallback "sonnet,g""#,
        5,
    );
    assert_eq!(reply, ["sonnet,gpt"]);
    let reply = bash_complete(&config, &state, r#"pastor machine describe "west,e""#, 3);
    assert_eq!(reply, ["west,east"]);
}

/// The same through the real generated fish script, with fish's own
/// `complete -C`, in the `--fallback x` and `--fallback=x` forms. Skipped,
/// saying so, where fish is not installed.
#[test]
fn fish_completion_finishes_a_second_fallback_name_and_comma_names() {
    if Command::new("fish").arg("--version").output().is_err() {
        eprintln!("skipped: fish is not installed, so the fish completion test cannot run");
        return;
    }
    let (_tmp, config, state) = fallback_completion_config();
    let script = pastor().args(["completions", "fish"]).output().unwrap();
    assert!(script.status.success());
    let script_path = state.join("completions.fish");
    std::fs::write(&script_path, &script.stdout).unwrap();
    let fish = |line: &str| -> Vec<String> {
        let out = Command::new("fish")
            .args(["--no-config", "-c"])
            .arg(format!(
                "source '{}'; complete -C '{line}'",
                script_path.display()
            ))
            .env("PATH", path_with_pastor())
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", state.join("data"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.split('\t').next().unwrap().to_string())
            .collect()
    };
    let reply = fish(r#"pastor task run "fix it" --fallback sonnet,g"#);
    assert_eq!(reply, ["sonnet,gpt"]);
    // fish answers an `--opt=value` word with the option kept in front.
    let reply = fish(r#"pastor task run "fix it" --fallback=sonnet,g"#);
    assert_eq!(reply, ["--fallback=sonnet,gpt"]);
    let reply = fish("pastor machine describe west,e");
    assert_eq!(reply, ["west,east"]);
}

#[test]
fn complete_offers_task_ids_with_their_note() {
    let (_tmp, config, state) = completion_config();
    // No store yet: nothing to offer, and no store is created by asking.
    let (ok, out) = complete(&config, &state, &["task", "read", ""]);
    assert!(ok);
    assert!(out.is_empty(), "{out}");
    assert!(!state.join("pastor.db").exists());
    let out = pastor()
        .args(["tick", "--job", "nightly"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("PASTOR_DATA_DIR", state.join("data"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for words in [
        &["task", "read", ""][..],
        &["task", "describe", ""],
        &["task", "done", ""],
        &["task", "close", ""],
        // close takes several: past one id it offers more.
        &["task", "close", "t-2", ""],
        &["events", "--task", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        // The clock item's title is the note, as in `task list`.
        assert!(out.starts_with("t-1\tclock "), "{words:?}: {out}");
        assert_eq!(out.lines().count(), 1, "{words:?}: {out}");
    }
}

/// `queue move` offers the queued tasks, for the task and for `--before`
/// and `--after`, with their level; `pastor queue` with no head shows the
/// store's queue and says it cannot tell why each waits.
#[test]
fn complete_offers_queued_tasks_and_the_queue_reads_offline() {
    let (_tmp, config, state) = completion_config();
    let tick = pastor()
        .args(["tick", "--job", "nightly"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("PASTOR_DATA_DIR", state.join("data"))
        .output()
        .unwrap();
    assert!(
        tick.status.success(),
        "{}",
        String::from_utf8_lossy(&tick.stderr)
    );
    for words in [
        &["queue", "move", ""][..],
        &["queue", "move", "t-1", "--before", ""],
        &["queue", "move", "t-1", "--after", ""],
    ] {
        let (ok, out) = complete(&config, &state, words);
        assert!(ok, "{words:?}");
        assert!(out.starts_with("t-1\tnormal clock "), "{words:?}: {out}");
    }
    let out = pastor()
        .args(["queue"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("PASTOR_DATA_DIR", state.join("data"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("job nightly"), "{text}");
    assert!(text.contains("pastor serve is not running"), "{text}");
}

/// The installed scripts ask `pastor __complete` at TAB time; fish needs a
/// separate entry for the options, since after `--flock` it consults only
/// that option's own entries.
#[test]
fn completion_scripts_ask_pastor_for_names() {
    let fish = pastor().args(["completions", "fish"]).output().unwrap();
    let fish = String::from_utf8(fish.stdout).unwrap();
    assert!(fish.contains("pastor __complete fish --"), "{fish}");
    let opts = fish
        .lines()
        .find(|l| l.contains("-n __fish_pastor_names -l "))
        .unwrap_or_else(|| panic!("no option entry:\n{fish}"));
    for long in ["flock", "machine", "job", "task"] {
        assert!(opts.contains(&format!("-l {long} ")), "{opts}");
    }
    assert!(
        !fish.contains("-a \"__complete\""),
        "the hidden command is offered"
    );
    let bash = pastor().args(["completions", "bash"]).output().unwrap();
    let bash = String::from_utf8(bash.stdout).unwrap();
    assert!(bash.contains("pastor __complete bash --"), "{bash}");
    assert!(bash.contains("complete -F _pastor_names"), "{bash}");
    assert!(
        !bash.contains("pastor,__complete)"),
        "bash knows __complete"
    );
}

/// The release tarball ships `contrib/completions/*` as checked in, so a copy
/// that no longer matches what the CLI prints would ship stale without anyone
/// noticing. `make completions` regenerates them.
#[test]
fn shipped_completions_match_the_cli() {
    for shell in ["bash", "fish"] {
        let out = pastor().args(["completions", shell]).output().unwrap();
        assert!(out.status.success());
        let fresh = String::from_utf8(out.stdout).unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("contrib/completions")
            .join(format!("pastor.{shell}"));
        let shipped = std::fs::read_to_string(&path).unwrap();
        assert!(
            shipped == fresh,
            "{} is stale: run make completions",
            path.display()
        );
    }
}

/// `--model` offers the `[models]` names, with their kind.
#[test]
fn complete_offers_model_names_after_model() {
    let (_tmp, config, state) = completion_config();
    std::fs::write(
        config.join("pastor.toml"),
        "[models.sonnet]\nkind = \"claude\"\nargs = [\"--model\", \"claude-sonnet-5\"]\n\
         [models.gpt]\nkind = \"codex\"\nargs = []\n",
    )
    .unwrap();
    let (ok, out) = complete(&config, &state, &["task", "run", "--model", ""]);
    assert!(ok);
    assert_eq!(out, "gpt\tcodex\nsonnet\tclaude\n");
}

/// `--priority` and `task priority` offer the levels.
#[test]
fn complete_offers_the_levels() {
    let (_tmp, config, state) = completion_config();
    let levels = "low\nnormal\nhigh\ncritical\n";
    let (ok, out) = complete(&config, &state, &["task", "run", "--priority", ""]);
    assert!(ok);
    assert_eq!(out, levels);
    let (ok, out) = complete(&config, &state, &["task", "priority", "t-1", ""]);
    assert!(ok);
    assert_eq!(out, levels);
}
