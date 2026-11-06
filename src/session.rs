//! Packing a Claude task's session files for a move between machines.
//!
//! Claude writes a session to `<config>/projects/<dir>/<session>.jsonl` and,
//! for a session that ran subagents, a `<session>/` folder beside it,
//! `<config>` being `CLAUDE_CONFIG_DIR` or `~/.claude` ([`crate::usage`]).
//! `<dir>` is the task's cwd with every character outside `[A-Za-z0-9]`
//! turned into `-`. These two commands move those files between machines:
//! `pack_command` tars them off the source machine's stdout, `unpack_command`
//! untars them into the destination's project dir for the new cwd. pastor
//! does not run them yet.

use crate::herdr::transport::shell_quote;
use crate::usage::config_dir_word;

/// Claude's project directory name for a task whose cwd is `cwd`: every
/// character outside `[A-Za-z0-9]` becomes `-`.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The shell command that tars session `session`'s files to stdout: the
/// `.jsonl` and, when it ran subagents, the `<session>/` folder beside it.
/// Fails with `no session <session>` on stderr and nothing on stdout when
/// no such session file exists under `config_dir`'s projects, and with
/// `ambiguous session <session>` when it exists under more than one
/// project directory: a session id can collide across two cwds (one moved
/// back to under a different cwd, or a fresh collision), and picking
/// either copy over the other would silently export the wrong one. A
/// symlink is not a session file. Refused here for anything but a session
/// id (`task::is_session_id`), since it goes into the command line as a
/// path and as a tar argument. `COPYFILE_DISABLE` keeps macOS tar from
/// adding `._*` sidecars of extended attributes.
pub fn pack_command(config_dir: Option<&str>, session: &str) -> anyhow::Result<String> {
    if !crate::task::is_session_id(session) {
        anyhow::bail!("not a Claude session id: {session:?}");
    }
    let dir = config_dir_word(config_dir);
    let s = session;
    Ok(format!(
        "d={dir}; set --; for f in \"$d\"/projects/*/{s}.jsonl; do if test -f \"$f\" && ! test -L \"$f\"; then set -- \"$@\" \"$f\"; fi; done; if test $# -eq 0; then echo \"no session {s}\" >&2; exit 1; fi; if test $# -gt 1; then echo \"ambiguous session {s}\" >&2; exit 1; fi; cd -- \"$(dirname -- \"$1\")\" && if test -d ./{s} && ! test -L ./{s}; then COPYFILE_DISABLE=1 tar -cf - ./{s}.jsonl ./{s}; else COPYFILE_DISABLE=1 tar -cf - ./{s}.jsonl; fi"
    ))
}

/// The shell command that untars session `session`, packed by
/// `pack_command`, from stdin into `config_dir`'s project dir for `cwd`,
/// creating it first. The stream is kept in a file there and read before
/// anything is written: every entry must be a plain file or a folder (no
/// link of either kind), named `<session>.jsonl` or under `<session>/`,
/// with no `..` in its path, and the `.jsonl` must be there. It is listed
/// with `-P`, since GNU tar otherwise lists a `../` or leading `/` already
/// stripped. Anything else
/// fails with `refused: ...` on stderr and nothing extracted. Refused here
/// for a `cwd` that is not absolute (an empty one would unpack into
/// `projects/` itself) and for anything but a session id.
pub fn unpack_command(
    config_dir: Option<&str>,
    cwd: &str,
    session: &str,
) -> anyhow::Result<String> {
    if !cwd.starts_with('/') {
        anyhow::bail!("not an absolute folder: {cwd:?}");
    }
    if !crate::task::is_session_id(session) {
        anyhow::bail!("not a Claude session id: {session:?}");
    }
    let dir = config_dir_word(config_dir);
    let p = shell_quote(&project_dir_name(cwd));
    let s = session;
    Ok(format!(
        "d={dir}; p=\"$d\"/projects/{p}; mkdir -p \"$p\" || exit 1; f=$(mktemp \"$p/.unpack.XXXXXX\") || exit 1; trap 'rm -f \"$f\" \"$f.list\" \"$f.names\"' EXIT; cat > \"$f\" || exit 1; \
         tar -P -tvf \"$f\" > \"$f.list\" 2>/dev/null && tar -P -tf \"$f\" > \"$f.names\" 2>/dev/null || {{ echo \"refused: not a tar stream\" >&2; exit 1; }}; \
         bad=; while read -r m rest; do case $m in -*|d*) ;; *) bad=1;; esac; case $rest in *' -> '*|*' link to '*) bad=1;; esac; done < \"$f.list\"; \
         if test -n \"$bad\"; then echo \"refused: an entry that is not a file or a folder\" >&2; exit 1; fi; \
         while IFS= read -r n; do case $n in ..|../*|*/..|*/../*) bad=1;; {s}.jsonl|./{s}.jsonl|{s}|{s}/|{s}/*|./{s}|./{s}/|./{s}/*) ;; *) bad=1;; esac; done < \"$f.names\"; \
         if test -n \"$bad\"; then echo \"refused: an entry outside session {s}\" >&2; exit 1; fi; \
         if ! grep -qxF -e {s}.jsonl -e ./{s}.jsonl \"$f.names\"; then echo \"refused: no {s}.jsonl in the stream\" >&2; exit 1; fi; \
         tar -C \"$p\" -xf \"$f\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21";

    fn run(home: &std::path::Path, command: &str) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", command])
            .env("HOME", home)
            .output()
            .unwrap()
    }

    /// Runs `pack` and `unpack` as the two separate machines of a real move
    /// would: `pack`'s whole stdout feeds `unpack`'s stdin, each under its
    /// own `sh -c`. A single shared `sh -c "pack | unpack"` would not do,
    /// since the pipe only joins the last statement of `pack` to the first
    /// of `unpack`, not either command as a whole.
    fn pack_and_unpack(home: &std::path::Path, pack: &str, unpack: &str) -> std::process::Output {
        use std::io::Write;
        use std::process::Stdio;

        let packed = run(home, pack);
        assert!(packed.status.success(), "{packed:?}");

        let mut child = std::process::Command::new("sh")
            .args(["-c", unpack])
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&packed.stdout)
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Claude's rule: every character outside `[A-Za-z0-9]` becomes `-`.
    #[test]
    fn project_dir_name_escapes_every_non_alnum_char() {
        assert_eq!(
            project_dir_name("/home/u/src/github.com/someone/app"),
            "-home-u-src-github-com-someone-app"
        );
        assert_eq!(
            project_dir_name("/home/u/.cache/worktrees/app/feat-x"),
            "-home-u--cache-worktrees-app-feat-x"
        );
    }

    /// Packing on one cwd's project dir and unpacking for a different cwd
    /// lands both the session file and its subagents folder, unchanged.
    #[test]
    fn pack_then_unpack_round_trips_both_files() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(src.join(SESSION).join("subagents")).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "the session\n").unwrap();
        std::fs::write(
            src.join(SESSION).join("subagents/agent-1.jsonl"),
            "a subagent\n",
        )
        .unwrap();

        let dest_cwd = "/home/u/src/github.com/someone/app";
        let out = pack_and_unpack(
            home.path(),
            &pack_command(None, SESSION).unwrap(),
            &unpack_command(None, dest_cwd, SESSION).unwrap(),
        );
        assert!(out.status.success(), "{out:?}");

        let dest = home
            .path()
            .join(".claude/projects")
            .join(project_dir_name(dest_cwd));
        assert_eq!(
            std::fs::read_to_string(dest.join(format!("{SESSION}.jsonl"))).unwrap(),
            "the session\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join(SESSION).join("subagents").join("agent-1.jsonl"))
                .unwrap(),
            "a subagent\n"
        );
    }

    /// `CLAUDE_CONFIG_DIR` is honoured on both ends, `~` meaning the
    /// machine's home.
    #[test]
    fn a_config_dir_is_honoured() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude-alt/projects/-src-app");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "the session\n").unwrap();

        let dest_cwd = "/home/u/project";
        let out = pack_and_unpack(
            home.path(),
            &pack_command(Some("~/.claude-alt"), SESSION).unwrap(),
            &unpack_command(Some("~/.claude-alt"), dest_cwd, SESSION).unwrap(),
        );
        assert!(out.status.success(), "{out:?}");

        let dest = home
            .path()
            .join(".claude-alt/projects")
            .join(project_dir_name(dest_cwd))
            .join(format!("{SESSION}.jsonl"));
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "the session\n");
    }

    /// A session that never ran subagents has no `<s>/` folder; it still
    /// packs and unpacks.
    #[test]
    fn a_session_without_a_subagents_folder_packs() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "solo session\n").unwrap();

        let dest_cwd = "/home/u/project";
        let out = pack_and_unpack(
            home.path(),
            &pack_command(None, SESSION).unwrap(),
            &unpack_command(None, dest_cwd, SESSION).unwrap(),
        );
        assert!(out.status.success(), "{out:?}");

        let dest = home
            .path()
            .join(".claude/projects")
            .join(project_dir_name(dest_cwd))
            .join(format!("{SESSION}.jsonl"));
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "solo session\n");
    }

    /// No such session file: nothing on stdout, `no session <s>` on
    /// stderr, a non-zero exit.
    #[test]
    fn a_missing_session_fails() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
        let out = run(home.path(), &pack_command(None, SESSION).unwrap());
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            format!("no session {SESSION}")
        );
    }

    /// The same session id under two project directories is ambiguous:
    /// `pack` must refuse rather than arbitrarily pick one and silently
    /// export the wrong machine's copy.
    #[test]
    fn a_session_id_in_two_projects_is_ambiguous() {
        let home = tempfile::tempdir().unwrap();
        for dir in ["-src-app-one", "-src-app-two"] {
            let src = home.path().join(".claude/projects").join(dir);
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join(format!("{SESSION}.jsonl")), "a session\n").unwrap();
        }

        let out = run(home.path(), &pack_command(None, SESSION).unwrap());
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            format!("ambiguous session {SESSION}")
        );
    }

    /// A cwd that needs shell escaping still round-trips.
    #[test]
    fn the_escape_rule_quotes_odd_paths() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "odd session\n").unwrap();

        let dest_cwd = "/home/u/it's a weird dir/app";
        let out = pack_and_unpack(
            home.path(),
            &pack_command(None, SESSION).unwrap(),
            &unpack_command(None, dest_cwd, SESSION).unwrap(),
        );
        assert!(out.status.success(), "{out:?}");

        let dest = home
            .path()
            .join(".claude/projects")
            .join(project_dir_name(dest_cwd))
            .join(format!("{SESSION}.jsonl"));
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "odd session\n");
    }

    /// The session id goes into the command as a path and a tar argument:
    /// anything but a session id is refused before a command is made, and
    /// so is a destination that is not an absolute folder (an empty one
    /// would unpack into `projects/` itself).
    #[test]
    fn what_is_not_a_session_id_or_a_folder_is_refused() {
        for bad in [
            "../secret",
            "--checkpoint-action=exec=sh",
            "-v",
            "",
            "it's a session",
            "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e2/",
        ] {
            assert!(pack_command(None, bad).is_err(), "{bad:?}");
            assert!(unpack_command(None, "/src/app", bad).is_err(), "{bad:?}");
        }
        for cwd in ["", "src/app", "./app"] {
            assert!(unpack_command(None, cwd, SESSION).is_err(), "{cwd:?}");
        }
    }

    /// The names in a tar stream, by the machine's own `tar`.
    fn names(home: &std::path::Path, stream: &[u8]) -> Vec<String> {
        let file = home.join("stream.tar");
        std::fs::write(&file, stream).unwrap();
        let out = std::process::Command::new("tar")
            .arg("-tf")
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }

    /// Only the session's own files go: not another session beside it, nor
    /// the config dir's credentials and settings.
    #[test]
    fn pack_takes_only_the_session() {
        let home = tempfile::tempdir().unwrap();
        let d = home.path().join(".claude");
        let src = d.join("projects/-src-app");
        let other = "11111111-2222-3333-4444-555555555555";
        std::fs::create_dir_all(src.join(SESSION).join("subagents")).unwrap();
        std::fs::create_dir_all(src.join(other)).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "mine\n").unwrap();
        std::fs::write(src.join(SESSION).join("subagents/a.jsonl"), "sub\n").unwrap();
        std::fs::write(src.join(format!("{other}.jsonl")), "theirs\n").unwrap();
        std::fs::write(src.join(other).join("x.jsonl"), "theirs\n").unwrap();
        std::fs::write(d.join(".credentials.json"), "secret\n").unwrap();
        std::fs::write(d.join("settings.json"), "{}\n").unwrap();

        let out = run(home.path(), &pack_command(None, SESSION).unwrap());
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            names(home.path(), &out.stdout),
            [
                format!("./{SESSION}.jsonl"),
                format!("./{SESSION}/"),
                format!("./{SESSION}/subagents/"),
                format!("./{SESSION}/subagents/a.jsonl"),
            ]
        );
    }

    /// A symlink in a session's place is not that session: following it
    /// could pack any file the link points at.
    #[test]
    fn a_symlinked_session_is_not_packed() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(home.path().join("secret"), "secret\n").unwrap();
        std::os::unix::fs::symlink(
            home.path().join("secret"),
            src.join(format!("{SESSION}.jsonl")),
        )
        .unwrap();
        let out = run(home.path(), &pack_command(None, SESSION).unwrap());
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            format!("no session {SESSION}")
        );
    }

    /// macOS tar adds `._*` sidecars of extended attributes unless
    /// `COPYFILE_DISABLE` is set: a `tar` first on `PATH` sees it set.
    #[test]
    fn pack_tells_macos_tar_to_leave_out_its_sidecars() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(format!("{SESSION}.jsonl")), "s\n").unwrap();
        let real = String::from_utf8(
            std::process::Command::new("sh")
                .args(["-c", "command -v tar"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let seen = home.path().join("seen");
        let fake = bin.join("tar");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"COPYFILE_DISABLE=$COPYFILE_DISABLE\" > {}\nexec {} \"$@\"\n",
                seen.display(),
                real.trim()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", &pack_command(None, SESSION).unwrap()])
            .env("HOME", home.path())
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            "COPYFILE_DISABLE=1"
        );
    }

    /// A tar stream made in `dir` of `members` (`tar -P`, so `..` and a
    /// leading `/` are kept as given), as a hostile source could send.
    fn stream_of(dir: &std::path::Path, members: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new("tar")
            .arg("-P")
            .arg("-cf")
            .arg("-")
            .args(members)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        out.stdout
    }

    /// `unpack` of `stream` for `cwd`, as the destination machine runs it.
    fn unpack_stream(home: &std::path::Path, cwd: &str, stream: &[u8]) -> std::process::Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new("sh")
            .args(["-c", &unpack_command(None, cwd, SESSION).unwrap()])
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stream).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Every file under `dir`, as paths relative to it.
    fn files_under(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(at) = todo.pop() {
            for e in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                let path = e.path();
                if path.is_dir() && !path.is_symlink() {
                    todo.push(path);
                } else {
                    out.push(path.strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
        out
    }

    /// Makes a hostile tar stream from a source folder holding the session.
    type MakeStream<'a> = Box<dyn Fn(&std::path::Path) -> Vec<u8> + 'a>;

    /// A stream with a `..` path, an absolute path, a link of either kind,
    /// another session's file, or no session file at all is refused before
    /// anything is written: the project dir stays empty, nothing lands
    /// outside it, and the staged copy is gone.
    #[test]
    fn unpack_refuses_what_is_not_the_session() {
        let jsonl = format!("{SESSION}.jsonl");
        let cases: Vec<(&str, MakeStream<'_>)> = vec![
            (
                "a .. path after a good file",
                Box::new(|src| {
                    std::fs::write(src.join("../evil"), "evil\n").unwrap();
                    stream_of(src, &[&jsonl, "../evil"])
                }),
            ),
            (
                "an absolute path",
                Box::new(|src| {
                    let abs = src.join("abs");
                    std::fs::write(&abs, "evil\n").unwrap();
                    stream_of(src, &[&jsonl, abs.to_str().unwrap()])
                }),
            ),
            (
                "a symlink",
                Box::new(|src| {
                    std::fs::remove_file(src.join(&jsonl)).unwrap();
                    std::os::unix::fs::symlink("../../.credentials.json", src.join(&jsonl))
                        .unwrap();
                    stream_of(src, &[&jsonl])
                }),
            ),
            (
                "a hardlink",
                Box::new(|src| {
                    std::fs::create_dir_all(src.join(SESSION)).unwrap();
                    std::fs::hard_link(src.join(&jsonl), src.join(SESSION).join("x.jsonl"))
                        .unwrap();
                    stream_of(src, &[&jsonl, SESSION])
                }),
            ),
            (
                "another session's file",
                Box::new(|src| {
                    std::fs::write(src.join("other.jsonl"), "theirs\n").unwrap();
                    stream_of(src, &[&jsonl, "other.jsonl"])
                }),
            ),
            (
                "no session file",
                Box::new(|src| {
                    std::fs::create_dir_all(src.join(SESSION)).unwrap();
                    std::fs::write(src.join(SESSION).join("x.jsonl"), "sub\n").unwrap();
                    stream_of(src, &[SESSION])
                }),
            ),
            (
                "not a tar stream",
                Box::new(|_| b"not tar at all\n".to_vec()),
            ),
        ];
        for (what, make) in cases {
            let home = tempfile::tempdir().unwrap();
            let src = home.path().join("from/sub");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join(&jsonl), "the session\n").unwrap();
            let stream = make(&src);
            let before = files_under(home.path());

            let dest_cwd = "/src/app";
            let out = unpack_stream(home.path(), dest_cwd, &stream);
            assert!(!out.status.success(), "{what}: {out:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).starts_with("refused: "),
                "{what}: {out:?}"
            );
            if what == "a .. path after a good file" || what == "an absolute path" {
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("outside session"),
                    "{what}: {out:?}"
                );
            }
            let dest = home
                .path()
                .join(".claude/projects")
                .join(project_dir_name(dest_cwd));
            assert_eq!(files_under(&dest), Vec::<String>::new(), "{what}");
            let mut after = files_under(home.path());
            after.retain(|f| !before.contains(f));
            assert_eq!(after, Vec::<String>::new(), "{what}: written outside");
        }
    }
}
