//! `pastor setup launchd [--herdr]`: the macOS side of `setup systemd`.
//! Installs the shipped LaunchAgent plist into `~/Library/LaunchAgents` and
//! applies the requested action with `launchctl` in the user's `gui/<uid>`
//! domain. Every `launchctl` and `id` call goes through [`Runner`], as the
//! systemd ones do, so the tests run on Linux.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Action, Plan, Runner, Unit, Written, secure, write_file};
use crate::config::Paths;

const PASTOR_PLIST: &str = include_str!("../../contrib/launchd/pastor.plist");
const HERDR_PLIST: &str = include_str!("../../contrib/launchd/herdr.plist");

impl Unit {
    /// The launchd label, which is also the plist's file stem. Both start with
    /// `pastor.` so it is plain who installed them, and so pastor's herdr
    /// agent cannot collide with one herdr might ship itself.
    pub fn label(self) -> &'static str {
        match self {
            Unit::Pastor => "pastor.serve",
            Unit::Herdr => "pastor.herdr",
        }
    }

    pub fn plist_name(self) -> &'static str {
        match self {
            Unit::Pastor => "pastor.serve.plist",
            Unit::Herdr => "pastor.herdr.plist",
        }
    }

    pub fn plist_template(self) -> &'static str {
        match self {
            Unit::Pastor => PASTOR_PLIST,
            Unit::Herdr => HERDR_PLIST,
        }
    }
}

/// One `setup launchd` run, with everything it would otherwise read from the
/// environment spelled out.
pub struct Install {
    pub unit: Unit,
    /// `~/Library/LaunchAgents` in production.
    pub agent_dir: PathBuf,
    /// `~/Library/Logs` in production: stdout and stderr go to
    /// `<label>.log` there, where Console.app finds them.
    pub log_dir: PathBuf,
    /// Absolute path of the binary ProgramArguments runs.
    pub exec: PathBuf,
    /// EnvironmentVariables entries, in order.
    pub env: Vec<(String, String)>,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub unit: Unit,
    pub plist_path: PathBuf,
    pub log_path: PathBuf,
    pub written: Written,
    /// One line per permission pastor tightened.
    pub fixed: Vec<String>,
    pub action: Action,
    /// Whether the agent was loaded before this run, which is what decides
    /// between bootstrap and kickstart.
    pub was_loaded: bool,
    pub uid: String,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = self.unit.label();
        let plist = self.plist_path.display();
        for line in &self.fixed {
            writeln!(f, "{line}")?;
        }
        match &self.written {
            Written::Created => writeln!(f, "wrote {plist}")?,
            Written::Unchanged => writeln!(f, "{plist} is up to date")?,
            Written::Updated { backup } => {
                writeln!(
                    f,
                    "updated {plist} (previous version kept as {})",
                    backup.display()
                )?;
                if self.was_loaded {
                    writeln!(
                        f,
                        "a loaded {label} keeps the old plist until `launchctl bootout gui/{uid}/{label}` and `launchctl bootstrap gui/{uid} {plist}`",
                        uid = self.uid
                    )?;
                }
            }
        }
        writeln!(f, "ran launchd action: {} {label}", self.action)?;
        writeln!(f, "logs go to {}", self.log_path.display())?;
        writeln!(
            f,
            "a LaunchAgent runs while you are logged in and loads again at login"
        )
    }
}

impl Install {
    /// Tighten permissions (pastor only), write the plist, then run the
    /// requested action through `launchctl`. Fails on a file error or a
    /// `launchctl` call that fails.
    pub fn run(&self, runner: &dyn Runner, paths: &Paths) -> anyhow::Result<Report> {
        // pastor.herdr goes on flock machines that may never run pastor
        // itself; creating ~/.config/pastor there would be litter.
        let fixed = match self.unit {
            Unit::Pastor => secure(paths)?,
            Unit::Herdr => Vec::new(),
        };
        let plist_path = self.agent_dir.join(self.unit.plist_name());
        let log_path = self.log_path();
        let text = render(self.unit.plist_template(), &self.exec, &self.env, &log_path);
        let written = write_file(&self.agent_dir, &plist_path, &text)?;
        std::fs::create_dir_all(&self.log_dir)
            .with_context(|| format!("create {}", self.log_dir.display()))?;

        let uid = uid(runner)?;
        let domain = format!("gui/{uid}");
        let target = format!("{domain}/{}", self.unit.label());
        let plist = plist_path.to_string_lossy();
        // `print` exits non-zero for a service launchd does not know.
        let was_loaded = runner
            .run("launchctl", &["print", &target])
            .is_ok_and(|o| o.success);
        let start = |loaded: bool| -> Vec<Vec<&str>> {
            if loaded {
                vec![vec!["kickstart", &target]]
            } else {
                // RunAtLoad starts it as it loads.
                vec![vec!["bootstrap", &domain, &plist]]
            }
        };
        let calls: Vec<Vec<&str>> = match self.action {
            Action::Enable => vec![vec!["enable", &target]],
            Action::Start => start(was_loaded),
            Action::EnableStart | Action::EnableNow => {
                let mut calls = vec![vec!["enable", target.as_str()]];
                calls.extend(start(was_loaded));
                calls
            }
            // bootout, not kill: KeepAlive would start a killed agent again.
            Action::Stop if was_loaded => vec![vec!["bootout", &target]],
            Action::Stop => Vec::new(),
        };
        for args in calls {
            launchctl(runner, &args)?;
        }
        Ok(Report {
            unit: self.unit,
            plist_path,
            log_path,
            written,
            fixed,
            action: self.action,
            was_loaded,
            uid,
        })
    }

    fn log_path(&self) -> PathBuf {
        self.log_dir.join(format!("{}.log", self.unit.label()))
    }

    pub fn plan(&self) -> Plan<'_> {
        Plan {
            file: self.unit.plist_name(),
            dir_label: "agent dir",
            dir: &self.agent_dir,
            exec_label: "program",
            exec: &self.exec,
            action: self.action,
        }
    }
}

/// The shipped plist with the program (the first ProgramArguments string)
/// replaced by `exec`, and EnvironmentVariables, StandardOutPath and
/// StandardErrorPath added right after ProgramArguments.
pub fn render(template: &str, exec: &Path, env: &[(String, String)], log: &Path) -> String {
    let mut out = String::with_capacity(template.len() + 512);
    // 0: before ProgramArguments, 1: waiting for its first string, 2: in its
    // array after that, 3: past it.
    let mut state = 0;
    for line in template.lines() {
        let trimmed = line.trim();
        match state {
            0 if trimmed == "<key>ProgramArguments</key>" => state = 1,
            1 if trimmed.starts_with("<string>") => {
                let indent = &line[..line.len() - line.trim_start().len()];
                out.push_str(&format!(
                    "{indent}<string>{}</string>\n",
                    escape(&exec.to_string_lossy())
                ));
                state = 2;
                continue;
            }
            2 if trimmed == "</array>" => {
                out.push_str(line);
                out.push('\n');
                if !env.is_empty() {
                    out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
                    for (k, v) in env {
                        out.push_str(&format!(
                            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                            escape(k),
                            escape(v)
                        ));
                    }
                    out.push_str("\t</dict>\n");
                }
                let log = escape(&log.to_string_lossy());
                for key in ["StandardOutPath", "StandardErrorPath"] {
                    out.push_str(&format!("\t<key>{key}</key>\n\t<string>{log}</string>\n"));
                }
                state = 3;
                continue;
            }
            _ => {}
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Text for a plist `<string>` or `<key>`.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The uid this process runs as, from `id -u`: macOS has no `/proc`, and
/// pastor does not link libc for one call.
fn uid(runner: &dyn Runner) -> anyhow::Result<String> {
    let out = runner.run("id", &["-u"]).context("run `id -u`")?;
    let uid = out.stdout.trim();
    if !out.success || uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        anyhow::bail!("`id -u` failed: {}", out.stderr.trim());
    }
    Ok(uid.to_string())
}

fn launchctl(runner: &dyn Runner, args: &[&str]) -> anyhow::Result<()> {
    let shown = format!("launchctl {}", args.join(" "));
    let out = runner
        .run("launchctl", args)
        .with_context(|| format!("run `{shown}`"))?;
    if !out.success {
        let why = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        anyhow::bail!("`{shown}` failed: {why}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::CmdOutput;
    use super::super::tests::FakeRunner;
    use super::*;

    struct Env {
        tmp: tempfile::TempDir,
        paths: Paths,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("config"), tmp.path().join("state"));
        Env { tmp, paths }
    }

    impl Env {
        fn install(&self, unit: Unit, action: Action) -> Install {
            Install {
                unit,
                agent_dir: self.tmp.path().join("LaunchAgents"),
                log_dir: self.tmp.path().join("Logs"),
                exec: PathBuf::from("/opt/bin/pastor"),
                env: vec![("PATH".into(), "/usr/bin:/opt/bin".into())],
                action,
            }
        }
    }

    /// `id -u` says 501, `launchctl print` succeeds when `loaded`, every
    /// other call succeeds.
    fn launchd(loaded: bool) -> FakeRunner {
        FakeRunner::new(move |call| {
            Ok(CmdOutput {
                success: !call.starts_with("launchctl print") || loaded,
                stdout: if call == "id -u" {
                    "501\n".into()
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        })
    }

    fn actions(runner: &FakeRunner) -> Vec<String> {
        runner
            .calls()
            .into_iter()
            .filter(|c| c != "id -u" && !c.starts_with("launchctl print"))
            .collect()
    }

    #[test]
    fn shipped_plists_meet_the_spec() {
        for unit in [Unit::Pastor, Unit::Herdr] {
            let t = unit.plist_template();
            assert!(t.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
            assert!(t.contains(&format!("<string>{}</string>", unit.label())));
            for key in ["RunAtLoad", "KeepAlive"] {
                assert!(
                    t.contains(&format!("<key>{key}</key>\n\t<true/>")),
                    "{unit:?} lacks {key}"
                );
            }
            // An XML comment may not contain `--`, so no flags in the prose.
            let comment = t.split("<!--").nth(1).unwrap().split("-->").next().unwrap();
            assert!(!comment.contains("--"), "{unit:?}");
        }
        assert!(PASTOR_PLIST.contains("<string>serve</string>"));
        assert!(HERDR_PLIST.contains("<string>server</string>"));
    }

    #[test]
    fn render_sets_the_program_env_and_logs() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/Users/u/my bin".to_string()),
            ("PASTOR_STATE_DIR".to_string(), "/s/a&b<c>".to_string()),
        ];
        let text = render(
            PASTOR_PLIST,
            Path::new("/Users/u/.cargo/bin/pastor"),
            &env,
            Path::new("/Users/u/Library/Logs/pastor.serve.log"),
        );
        let expected = "\
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/Users/u/.cargo/bin/pastor</string>
\t\t<string>serve</string>
\t</array>
\t<key>EnvironmentVariables</key>
\t<dict>
\t\t<key>PATH</key>
\t\t<string>/usr/bin:/Users/u/my bin</string>
\t\t<key>PASTOR_STATE_DIR</key>
\t\t<string>/s/a&amp;b&lt;c&gt;</string>
\t</dict>
\t<key>StandardOutPath</key>
\t<string>/Users/u/Library/Logs/pastor.serve.log</string>
\t<key>StandardErrorPath</key>
\t<string>/Users/u/Library/Logs/pastor.serve.log</string>
\t<key>RunAtLoad</key>
";
        assert!(text.contains(expected), "{text}");
        assert!(text.ends_with("</dict>\n</plist>\n"), "{text}");
    }

    #[test]
    fn installs_enables_and_bootstraps_an_unloaded_agent() {
        let e = env();
        let runner = launchd(false);
        let report = e
            .install(Unit::Pastor, Action::EnableNow)
            .run(&runner, &e.paths)
            .unwrap();

        let plist = e.tmp.path().join("LaunchAgents/pastor.serve.plist");
        assert_eq!(report.plist_path, plist);
        assert_eq!(report.written, Written::Created);
        let text = std::fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<string>/opt/bin/pastor</string>"), "{text}");
        assert!(e.tmp.path().join("Logs").is_dir());
        assert_eq!(
            actions(&runner),
            [
                "launchctl enable gui/501/pastor.serve".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );
        // The pastor agent tightens pastor's own dirs, as systemd's does.
        assert!(e.paths.config_dir.is_dir());
        let shown = report.to_string();
        assert!(
            shown.contains("ran launchd action: enable --now pastor.serve"),
            "{shown}"
        );
        assert!(shown.contains("pastor.serve.log"), "{shown}");
    }

    #[test]
    fn a_loaded_agent_is_kickstarted_not_bootstrapped_again() {
        let e = env();
        let runner = launchd(true);
        e.install(Unit::Pastor, Action::Start)
            .run(&runner, &e.paths)
            .unwrap();
        assert_eq!(
            actions(&runner),
            ["launchctl kickstart gui/501/pastor.serve"]
        );
    }

    #[test]
    fn enable_alone_does_not_start() {
        let e = env();
        let runner = launchd(false);
        e.install(Unit::Pastor, Action::Enable)
            .run(&runner, &e.paths)
            .unwrap();
        assert_eq!(actions(&runner), ["launchctl enable gui/501/pastor.serve"]);
    }

    #[test]
    fn stop_boots_out_a_loaded_agent_and_skips_an_unloaded_one() {
        let e = env();
        let runner = launchd(true);
        e.install(Unit::Pastor, Action::Stop)
            .run(&runner, &e.paths)
            .unwrap();
        assert_eq!(actions(&runner), ["launchctl bootout gui/501/pastor.serve"]);

        let runner = launchd(false);
        e.install(Unit::Pastor, Action::Stop)
            .run(&runner, &e.paths)
            .unwrap();
        assert!(actions(&runner).is_empty());
    }

    #[test]
    fn a_failing_launchctl_is_an_error_with_its_output() {
        let e = env();
        let runner = FakeRunner::new(|call| {
            Ok(CmdOutput {
                success: !call.starts_with("launchctl"),
                stdout: if call == "id -u" {
                    "501\n".into()
                } else {
                    String::new()
                },
                stderr: "Bootstrap failed: 5: Input/output error\n".into(),
            })
        });
        let err = e
            .install(Unit::Pastor, Action::Start)
            .run(&runner, &e.paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("launchctl bootstrap gui/501"), "{err}");
        assert!(err.contains("Input/output error"), "{err}");
    }

    #[test]
    fn a_changed_loaded_plist_is_backed_up_and_the_reload_named() {
        let e = env();
        let dir = e.tmp.path().join("LaunchAgents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pastor.serve.plist"), "<!-- hand edited -->\n").unwrap();

        let report = e
            .install(Unit::Pastor, Action::EnableNow)
            .run(&launchd(true), &e.paths)
            .unwrap();
        let backup = dir.join("pastor.serve.plist.bak");
        assert_eq!(
            report.written,
            Written::Updated {
                backup: backup.clone()
            }
        );
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "<!-- hand edited -->\n"
        );
        let shown = report.to_string();
        assert!(
            shown.contains("launchctl bootout gui/501/pastor.serve"),
            "{shown}"
        );
        assert!(!dir.join("pastor.serve.plist.tmp").exists());

        let again = e
            .install(Unit::Pastor, Action::EnableNow)
            .run(&launchd(true), &e.paths)
            .unwrap();
        assert_eq!(again.written, Written::Unchanged);
    }

    #[test]
    fn herdr_agent_skips_pastor_dirs() {
        let e = env();
        let mut install = e.install(Unit::Herdr, Action::Enable);
        install.exec = PathBuf::from("/opt/homebrew/bin/herdr");
        let runner = launchd(false);
        let report = install.run(&runner, &e.paths).unwrap();

        assert!(report.plist_path.ends_with("pastor.herdr.plist"));
        let text = std::fs::read_to_string(&report.plist_path).unwrap();
        assert!(
            text.contains("<string>/opt/homebrew/bin/herdr</string>\n\t\t<string>server</string>")
        );
        assert!(text.contains("<string>pastor.herdr</string>"));
        assert_eq!(actions(&runner), ["launchctl enable gui/501/pastor.herdr"]);
        assert!(!e.paths.config_dir.exists());
        assert!(!e.paths.state_dir.exists());
    }

    #[test]
    fn the_prompt_names_the_plist_and_program() {
        let e = env();
        let install = e.install(Unit::Pastor, Action::EnableNow);
        let mut out = Vec::new();
        super::super::confirm(
            &install.plan(),
            false,
            true,
            &mut "yes\n".as_bytes(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("About to install pastor.serve.plist"),
            "{text}"
        );
        assert!(text.contains("agent dir: "), "{text}");
        assert!(text.contains("program: /opt/bin/pastor"), "{text}");
    }

    #[test]
    fn a_bad_id_is_an_error() {
        let e = env();
        let runner = FakeRunner::new(|_| {
            Ok(CmdOutput {
                success: false,
                stdout: String::new(),
                stderr: "id: no such user\n".into(),
            })
        });
        let err = e
            .install(Unit::Pastor, Action::Start)
            .run(&runner, &e.paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("id -u"), "{err}");
    }
}
