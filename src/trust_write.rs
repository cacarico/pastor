//! Trust written before the agent starts, so its folder-trust dialog never
//! shows. For a folder pastor already trusts on a machine (`Store::trust_repo`:
//! the task's repo or the folder a task with no repo starts in, and the
//! worktree it gets), pastor puts that folder in the harness's own store
//! before it starts the agent:
//!
//! - Claude: `projects.<folder>.hasTrustDialogAccepted = true` in the agent
//!   home's `.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json`, else
//!   `~/.claude.json`). Running Claude sessions keep that file in memory and
//!   rewrite all of it, so the write is a compare and swap through the
//!   connector (the file is replaced only if it still holds what was read),
//!   the file is read again after a moment to see the entry survived, and a
//!   lost write is retried a bounded number of times.
//! - agy: the folder in `trustedWorkspaces` of its settings.
//!
//! A file that is not there is left not there (a home that has never run has
//! no store to add to), a symlink is not written through, and a file that is
//! not the JSON it should be is left alone. Nothing here can fail a task: the
//! auto-answer of the dialog (`Actor::auto_answer`) stays as the backstop for
//! every case this gives up on.

use std::time::Duration;

use crate::herdr::transport::Connector;
use crate::provision::agy::{AgyWrite, Json, SETTINGS_FILE};

/// How many times a write is tried: it loses its compare to a session that
/// saved meanwhile, or it lands and a session rewrites the file from an older
/// copy.
pub const ATTEMPTS: usize = 3;

/// How long after a write the file is read again to see the entry survived.
pub const RECHECK: Duration = Duration::from_millis(500);

/// What `ensure` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The folder was trusted already.
    Already,
    /// The entry is in the file, and was there when it was read again.
    Written,
    /// The harness has no store on the machine yet.
    NoFile,
    /// The store is a symlink, which pastor does not write through.
    Linked,
    /// Not written, and why: the auto-answer takes it.
    GaveUp(String),
    /// A kind with no store to write.
    Skipped,
}

/// Claude's store for the home `config_dir` names (`$CLAUDE_CONFIG_DIR`, `~`
/// being the machine's home), else the default one.
pub fn claude_file(config_dir: Option<&str>) -> String {
    match config_dir.filter(|d| !d.trim().is_empty()) {
        Some(dir) => format!("{}/.claude.json", dir.trim_end_matches('/')),
        None => "~/.claude.json".to_string(),
    }
}

/// What an edit of a store found to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Already,
    Write(String),
}

fn object(entries: &mut Vec<(String, Json)>, key: &str) -> usize {
    match entries.iter().rposition(|(k, _)| k == key) {
        Some(at) => at,
        None => {
            entries.push((key.to_string(), Json::Null));
            entries.len() - 1
        }
    }
}

fn parse(text: &str) -> Result<Json, String> {
    let json: Json = serde_json::from_str(text).map_err(|e| format!("not JSON ({e})"))?;
    match json {
        Json::Object(_) => Ok(json),
        _ => Err("its top level is not an object".into()),
    }
}

/// Text the same way the harness writes its file: two-space JSON, and the
/// trailing newline the original had.
fn render(root: &Json, like: &str) -> Result<String, String> {
    let mut out = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    if like.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Claude's `.claude.json` `text` with `folder` trusted: `Already` when it
/// is, else the text to write. Every other key keeps its value and place; a
/// project that is there keeps its other keys.
pub fn claude_edit(text: &str, folder: &str) -> Result<Edit, String> {
    let mut root = parse(text)?;
    let Json::Object(top) = &mut root else {
        unreachable!("parse gives an object")
    };
    let at = object(top, "projects");
    if top[at].1 == Json::Null {
        top[at].1 = Json::Object(vec![]);
    }
    let Json::Object(projects) = &mut top[at].1 else {
        return Err("its projects is not an object".into());
    };
    let at = object(projects, folder);
    if projects[at].1 == Json::Null {
        projects[at].1 = Json::Object(vec![]);
    }
    let Json::Object(project) = &mut projects[at].1 else {
        return Err(format!("its projects entry for {folder} is not an object"));
    };
    let at = object(project, "hasTrustDialogAccepted");
    if project[at].1 == Json::Bool(true) {
        return Ok(Edit::Already);
    }
    project[at].1 = Json::Bool(true);
    Ok(Edit::Write(render(&root, text)?))
}

/// agy's settings `text` with `folder` among `trustedWorkspaces`.
pub fn agy_edit(text: &str, folder: &str) -> Result<Edit, String> {
    let mut root = parse(text)?;
    let Json::Object(top) = &mut root else {
        unreachable!("parse gives an object")
    };
    let at = object(top, "trustedWorkspaces");
    if top[at].1 == Json::Null {
        top[at].1 = Json::Array(vec![]);
    }
    let Json::Array(list) = &mut top[at].1 else {
        return Err("its trustedWorkspaces is not an array".into());
    };
    if list
        .iter()
        .any(|j| matches!(j, Json::String(s) if s == folder))
    {
        return Ok(Edit::Already);
    }
    list.push(Json::String(folder.to_string()));
    Ok(Edit::Write(render(&root, text)?))
}

/// Whether `text` already trusts `folder` for `kind`, for the read after a
/// write; a file that no longer parses trusts nothing.
fn trusts(kind: &str, text: &str, folder: &str) -> bool {
    let edit = match kind {
        "claude" => claude_edit(text, folder),
        _ => agy_edit(text, folder),
    };
    matches!(edit, Ok(Edit::Already))
}

/// Put `folder` in the store of `kind`'s agent on the machine behind `conn`:
/// compare and swap, read again after `recheck`, at most `ATTEMPTS` times.
/// `config_dir` is the agent's `CLAUDE_CONFIG_DIR`, for Claude.
pub async fn ensure(
    conn: &dyn Connector,
    kind: &str,
    config_dir: Option<&str>,
    folder: &str,
    recheck: Duration,
) -> Outcome {
    let path = match kind {
        "claude" => claude_file(config_dir),
        "agy" => SETTINGS_FILE.to_string(),
        _ => return Outcome::Skipped,
    };
    let edit = |text: &str| match kind {
        "claude" => claude_edit(text, folder),
        _ => agy_edit(text, folder),
    };
    let gave_up = |why: String| Outcome::GaveUp(why);
    for _ in 0..ATTEMPTS {
        let read = match conn.home_file(&path).await {
            Ok(Some(read)) => read,
            Ok(None) => return gave_up("the machine takes no shell command".into()),
            Err(e) => return gave_up(e.to_string()),
        };
        if read.link {
            return Outcome::Linked;
        }
        let Some(text) = read.content else {
            return Outcome::NoFile;
        };
        let new = match edit(&text) {
            Ok(Edit::Already) => return Outcome::Already,
            Ok(Edit::Write(new)) => new,
            Err(why) => return gave_up(format!("{path} is {why}")),
        };
        // Claude saves under `<file>.lock`; holding it, the compare and the
        // rename cannot be split by its save.
        match conn
            .write_home_file(&path, Some(&text), &new, kind == "claude")
            .await
        {
            Ok(Some(AgyWrite::Written)) => {}
            // Saved meanwhile, or its lock is held: read it again and add to
            // what is there now.
            Ok(Some(AgyWrite::Changed | AgyWrite::Busy)) => continue,
            Ok(Some(AgyWrite::Link)) => return Outcome::Linked,
            Ok(Some(AgyWrite::Failed)) | Ok(None) => {
                return gave_up(format!("could not write {path}"));
            }
            Err(e) => return gave_up(e.to_string()),
        }
        // A session that held an older copy writes it all back: look again.
        if !recheck.is_zero() {
            tokio::time::sleep(recheck).await;
        }
        match conn.home_file(&path).await {
            Ok(Some(again))
                if again
                    .content
                    .as_deref()
                    .is_some_and(|t| trusts(kind, t, folder)) =>
            {
                return Outcome::Written;
            }
            // Gone again: another round.
            Ok(Some(_)) => {}
            Ok(None) => return gave_up("the machine takes no shell command".into()),
            Err(e) => return gave_up(e.to_string()),
        }
    }
    gave_up(format!(
        "{path} was rewritten {ATTEMPTS} times while it was written"
    ))
}

/// Before `task`'s agent starts in `folder`: `ensure`, when the harness is
/// one with a store and pastor trusts the task's folder on this machine
/// (`trusted`, its saved keys: the repo as the task names it, or the folder a
/// task with no repo starts in). Never an error: what it cannot do, the
/// dialog's auto-answer does.
pub async fn before_start(
    conn: &dyn Connector,
    trusted: &[String],
    recheck: Duration,
    task: &crate::task::Task,
    launch: &crate::config::Launch,
    folder: Option<&str>,
) {
    let Some(folder) = folder else { return };
    if !matches!(launch.kind.as_str(), "claude" | "agy") {
        return;
    }
    let Some(key) = task.spec.trust_dir() else {
        return;
    };
    if !trusted.iter().any(|t| t == key) {
        return;
    }
    let config_dir = launch.env.get("CLAUDE_CONFIG_DIR").map(String::as_str);
    match ensure(conn, &launch.kind, config_dir, folder, recheck).await {
        Outcome::Written => {
            tracing::info!(task = %task.display_id(), %folder, kind = %launch.kind, "wrote the trust of a folder before the start");
        }
        Outcome::GaveUp(why) => {
            tracing::warn!(task = %task.display_id(), %folder, kind = %launch.kind, %why, "could not write trust before the start; the dialog is answered when it shows");
        }
        Outcome::Already | Outcome::NoFile | Outcome::Linked | Outcome::Skipped => {}
    }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;

    /// A `.claude.json` as Claude writes it: two-space JSON, keys in its
    /// order, a trailing newline.
    const HOME: &str = "{\n  \"numStartups\": 12,\n  \"projects\": {\n    \"/w/old\": {\n      \"allowedTools\": [],\n      \"hasTrustDialogAccepted\": true\n    },\n    \"/w/ask\": {\n      \"allowedTools\": [],\n      \"hasTrustDialogAccepted\": false\n    }\n  },\n  \"userID\": \"abc\"\n}\n";

    fn written(edit: Result<Edit, String>) -> String {
        match edit {
            Ok(Edit::Write(text)) => text,
            other => panic!("{other:?}"),
        }
    }

    fn value(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    /// A folder gets its entry at the end of `projects`, a project asking
    /// still keeps its other keys, one already trusted is left alone, and no
    /// other key moves or changes. The text comes out as Claude writes it.
    #[test]
    fn claude_trust_is_added_in_claudes_own_shape() {
        let new = written(claude_edit(HOME, "/w/new"));
        assert!(new.ends_with("}\n"));
        let v = value(&new);
        assert_eq!(v["projects"]["/w/new"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/w/old"], value(HOME)["projects"]["/w/old"]);
        assert_eq!(v["numStartups"], 12);
        let keys: Vec<&str> = new.lines().filter(|l| l.starts_with("    \"/w/")).collect();
        assert_eq!(keys.len(), 3, "{new}");
        assert!(keys[2].contains("/w/new"), "last: {keys:?}");
        assert!(new.find("numStartups").unwrap() < new.find("projects").unwrap());
        assert!(new.find("projects").unwrap() < new.find("userID").unwrap());

        let asking = written(claude_edit(HOME, "/w/ask"));
        let v = value(&asking);
        assert_eq!(v["projects"]["/w/ask"]["hasTrustDialogAccepted"], true);
        assert_eq!(
            v["projects"]["/w/ask"]["allowedTools"],
            serde_json::json!([])
        );

        assert_eq!(claude_edit(HOME, "/w/old"), Ok(Edit::Already));
        // Only the entry is new in the text: every line Claude wrote is
        // still there, in order (a comma aside), and the new project is
        // three lines.
        let again = written(claude_edit(&asking, "/w/more"));
        let before: Vec<&str> = asking.lines().collect();
        let after: Vec<&str> = again.lines().map(|l| l.trim_end_matches(',')).collect();
        assert_eq!(after.len(), before.len() + 3, "{again}");
        let mut rest = after.iter();
        assert!(
            before
                .iter()
                .all(|l| rest.any(|a| *a == l.trim_end_matches(','))),
            "{asking}\n{again}"
        );
        // A file with no projects yet, and one with no trailing newline.
        let bare = written(claude_edit("{\"userID\": \"abc\"}", "/w/new"));
        assert!(!bare.ends_with('\n'));
        assert_eq!(
            value(&bare)["projects"]["/w/new"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn a_store_that_is_not_the_shape_is_left_alone() {
        for (text, why) in [
            ("{ nope", "not JSON"),
            ("[1]", "top level is not an object"),
            ("{\"projects\": []}", "projects is not an object"),
            ("{\"projects\": {\"/w\": 3}}", "is not an object"),
        ] {
            let err = claude_edit(text, "/w").unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
        assert!(
            agy_edit("{\"trustedWorkspaces\": 1}", "/w")
                .unwrap_err()
                .contains("not an array")
        );
    }

    #[test]
    fn agy_trust_is_a_workspace_in_its_list() {
        let text = "{\n  \"agentMode\": \"accept-edits\",\n  \"trustedWorkspaces\": [\n    \"/w/old\"\n  ]\n}\n";
        let v = value(&written(agy_edit(text, "/w/new")));
        assert_eq!(
            v["trustedWorkspaces"],
            serde_json::json!(["/w/old", "/w/new"])
        );
        assert_eq!(v["agentMode"], "accept-edits");
        assert_eq!(agy_edit(text, "/w/old"), Ok(Edit::Already));
        let v = value(&written(agy_edit("{}", "/w/new")));
        assert_eq!(v["trustedWorkspaces"], serde_json::json!(["/w/new"]));
    }

    #[test]
    fn claudes_store_is_in_its_config_dir_else_the_home() {
        assert_eq!(claude_file(None), "~/.claude.json");
        assert_eq!(claude_file(Some("")), "~/.claude.json");
        assert_eq!(
            claude_file(Some("~/.claude-alt")),
            "~/.claude-alt/.claude.json"
        );
        assert_eq!(claude_file(Some("/srv/c/")), "/srv/c/.claude.json");
    }

    const PATH: &str = "~/.claude.json";

    /// The entry lands in the store, and what was there stays.
    #[tokio::test]
    async fn ensure_writes_the_entry() {
        let fake = FakeHerdr::new();
        fake.set_home_file(PATH, Some(HOME));
        let got = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert_eq!(got, Outcome::Written);
        let v = value(&fake.home_file_now(PATH).unwrap());
        assert_eq!(v["projects"]["/w/new"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["userID"], "abc");
        assert_eq!(fake.file_writes().len(), 1);
        // Again: nothing to write.
        let got = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert_eq!(got, Outcome::Already);
        assert_eq!(fake.file_writes().len(), 1);
    }

    /// A session saves the file between pastor's read and its write: the
    /// write is refused, pastor reads the new file and adds to it, and the
    /// session's own change is still there.
    #[tokio::test]
    async fn a_concurrent_writer_between_the_read_and_the_write_does_not_cost_the_entry() {
        let fake = FakeHerdr::new();
        fake.set_home_file(PATH, Some(HOME));
        let theirs = HOME.replace("\"numStartups\": 12", "\"numStartups\": 13");
        fake.file_saves_before_write(PATH, Some(&theirs));
        let got = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert_eq!(got, Outcome::Written);
        let v = value(&fake.home_file_now(PATH).unwrap());
        assert_eq!(v["projects"]["/w/new"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["numStartups"], 13, "the session's save survives");
    }

    /// A session that held an older copy writes it all back after pastor's
    /// write: pastor reads the file again, finds the entry gone and writes it
    /// again. One that keeps doing it is given up on, with the file as the
    /// session left it.
    #[tokio::test]
    async fn a_session_rewriting_the_file_from_memory_is_written_around() {
        let fake = FakeHerdr::new();
        fake.set_home_file(PATH, Some(HOME));
        fake.file_overwritten_after_write(PATH, HOME);
        let got = ensure(&fake, "claude", None, "/w/new", Duration::from_millis(1)).await;
        assert_eq!(got, Outcome::Written);
        assert_eq!(fake.file_writes().len(), 2, "written, lost, written");
        let v = value(&fake.home_file_now(PATH).unwrap());
        assert_eq!(v["projects"]["/w/new"]["hasTrustDialogAccepted"], true);

        let fake = FakeHerdr::new();
        fake.set_home_file(PATH, Some(HOME));
        for _ in 0..ATTEMPTS {
            fake.file_overwritten_after_write(PATH, HOME);
        }
        let got = ensure(&fake, "claude", None, "/w/new", Duration::from_millis(1)).await;
        assert!(matches!(got, Outcome::GaveUp(_)), "{got:?}");
        assert_eq!(fake.file_writes().len(), ATTEMPTS);
        assert_eq!(fake.home_file_now(PATH).as_deref(), Some(HOME));
    }

    /// No file, a symlink, a store that is not JSON: nothing is written.
    #[tokio::test]
    async fn what_is_not_there_or_not_writable_is_left_alone() {
        let fake = FakeHerdr::new();
        assert_eq!(
            ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await,
            Outcome::NoFile
        );
        fake.set_home_file(PATH, Some("{ nope"));
        let got = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert!(
            matches!(&got, Outcome::GaveUp(why) if why.contains("not JSON")),
            "{got:?}"
        );
        assert_eq!(fake.home_file_now(PATH).as_deref(), Some("{ nope"));
        assert!(fake.file_writes().is_empty());
        assert_eq!(
            ensure(&fake, "codex", None, "/w/new", Duration::ZERO).await,
            Outcome::Skipped
        );
    }

    /// Claude's store follows the agent's config dir, and agy's trusted
    /// workspaces are the list in its settings, where the provisioner writes.
    #[tokio::test]
    async fn the_store_is_the_agents_own() {
        let fake = FakeHerdr::new();
        let path = "~/.claude-main/.claude.json";
        fake.set_home_file(path, Some(HOME));
        fake.set_home_file(PATH, Some(HOME));
        let got = ensure(
            &fake,
            "claude",
            Some("~/.claude-main"),
            "/w/new",
            Duration::ZERO,
        )
        .await;
        assert_eq!(got, Outcome::Written);
        assert_eq!(
            fake.home_file_now(PATH).as_deref(),
            Some(HOME),
            "the default home is not touched"
        );
        assert!(fake.home_file_now(path).unwrap().contains("/w/new"));

        fake.set_home_file(SETTINGS_FILE, Some("{\"agentMode\": \"accept-edits\"}\n"));
        let got = ensure(&fake, "agy", None, "/w/new", Duration::ZERO).await;
        assert_eq!(got, Outcome::Written);
        let v = value(&fake.home_file_now(SETTINGS_FILE).unwrap());
        assert_eq!(v["trustedWorkspaces"], serde_json::json!(["/w/new"]));
        assert_eq!(v["agentMode"], "accept-edits");
    }

    /// A store as big as real ones (they pass 64 KiB with a few hundred
    /// folders, and a command line may not hold 128 KiB) is read and written
    /// whole on a local machine, keeps its mode and its other keys, and
    /// leaves no lock or temp file behind.
    #[tokio::test]
    async fn a_store_of_any_size_is_written_on_a_local_machine() {
        use crate::herdr::transport::Endpoint;
        use std::os::unix::fs::PermissionsExt;
        let local = Endpoint::Local {
            session: "s".into(),
        };
        for projects in [30, 700, 1_300, 5_000] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = serde_json::json!({"primaryApiKey": "sk-keep", "projects": {}});
            for n in 0..projects {
                store["projects"][format!("/w/project-{n}")] = serde_json::json!({
                    "allowedTools": [], "hasTrustDialogAccepted": true,
                    "history": [{"display": "x".repeat(40)}],
                });
            }
            let text = format!("{}\n", serde_json::to_string_pretty(&store).unwrap());
            let file = dir.path().join(".claude.json");
            std::fs::write(&file, &text).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            let outcome = ensure(
                &local,
                "claude",
                dir.path().to_str(),
                "/w/new",
                Duration::ZERO,
            )
            .await;
            assert_eq!(outcome, Outcome::Written, "{} bytes", text.len());
            let after = std::fs::read_to_string(&file).unwrap();
            let v = value(&after);
            assert_eq!(v["projects"]["/w/new"]["hasTrustDialogAccepted"], true);
            assert_eq!(v["primaryApiKey"], "sk-keep");
            assert_eq!(v["projects"].as_object().unwrap().len(), projects + 1);
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(names, [".claude.json"], "{} bytes", text.len());
            // The second time there is nothing to write.
            let again = ensure(
                &local,
                "claude",
                dir.path().to_str(),
                "/w/new",
                Duration::ZERO,
            )
            .await;
            assert_eq!(again, Outcome::Already);
        }
    }

    /// Claude's store is written holding the lock Claude saves under; agy's
    /// settings are not. A lock held by another process is waited out by the
    /// next round, and three busy rounds give up with the file untouched.
    #[tokio::test]
    async fn claudes_store_is_written_under_its_lock() {
        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(HOME));
        let out = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert_eq!(out, Outcome::Written);
        assert_eq!(fake.file_locks(), [("~/.claude.json".to_string(), true)]);

        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(HOME));
        fake.file_lock_held("~/.claude.json", 1);
        assert_eq!(
            ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await,
            Outcome::Written
        );
        assert_eq!(fake.file_locks().len(), 2);

        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(HOME));
        fake.file_lock_held("~/.claude.json", 10);
        let out = ensure(&fake, "claude", None, "/w/new", Duration::ZERO).await;
        assert!(matches!(out, Outcome::GaveUp(_)), "{out:?}");
        assert_eq!(fake.file_locks().len(), ATTEMPTS);
        assert_eq!(fake.home_file_now("~/.claude.json").as_deref(), Some(HOME));

        let fake = FakeHerdr::new();
        fake.set_home_file(SETTINGS_FILE, Some("{}\n"));
        assert_eq!(
            ensure(&fake, "agy", None, "/w/new", Duration::ZERO).await,
            Outcome::Written
        );
        assert_eq!(fake.file_locks(), [(SETTINGS_FILE.to_string(), false)]);
    }
}
