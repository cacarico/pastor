//! `pastor setup systemd [--herdr]`: install the shipped user unit, enable and
//! start it, and check what a unit needs to outlive a login (lingering) and
//! what the spec requires of pastor's own files (config and state dirs 0700,
//! socket and plugin `.env` files 0600).
//!
//! Every `systemctl` and `loginctl` call goes through [`Runner`], so the tests
//! script their answers instead of touching the real user manager.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{Paths, create_private_dir};

const PASTOR_UNIT: &str = include_str!("../contrib/systemd/pastor.service");
const HERDR_UNIT: &str = include_str!("../contrib/systemd/herdr.service");

#[derive(clap::Subcommand)]
pub enum SetupCmd {
    /// Install and start a systemd user unit: pastor.service, or herdr.service with --herdr
    Systemd {
        /// Install herdr.service (the herdr server) instead, for a flock machine
        #[arg(long)]
        herdr: bool,
    },
}

/// Entry point for `pastor setup`.
pub fn cli(paths: &Paths, cmd: SetupCmd) -> anyhow::Result<()> {
    let SetupCmd::Systemd { herdr } = cmd;
    let unit = if herdr { Unit::Herdr } else { Unit::Pastor };
    let path_var = std::env::var("PATH").unwrap_or_default();
    let exec = match unit {
        Unit::Pastor => std::env::current_exe().context("locate the pastor binary")?,
        Unit::Herdr => which("herdr", &path_var)
            .context("herdr is not on PATH; install it or add its directory to PATH")?,
    };
    let mut env = vec![("PATH".to_string(), path_var)];
    if unit == Unit::Pastor {
        // An override the shell runs with has to reach the daemon too, or the
        // service would serve a different config and store than the CLI reads.
        for (var, dir) in [
            ("PASTOR_CONFIG_DIR", &paths.config_dir),
            ("PASTOR_STATE_DIR", &paths.state_dir),
        ] {
            if std::env::var_os(var).is_some() {
                let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.clone());
                env.push((var.to_string(), dir.to_string_lossy().into_owned()));
            }
        }
    }
    let unit_dir = dirs::config_dir()
        .context("no config dir")?
        .join("systemd/user");
    let install = Install {
        unit,
        unit_dir,
        exec,
        env,
    };
    let report = install.run(&SystemRunner, paths)?;
    print!("{report}");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Pastor,
    Herdr,
}

impl Unit {
    pub fn file_name(self) -> &'static str {
        match self {
            Unit::Pastor => "pastor.service",
            Unit::Herdr => "herdr.service",
        }
    }

    pub fn template(self) -> &'static str {
        match self {
            Unit::Pastor => PASTOR_UNIT,
            Unit::Herdr => HERDR_UNIT,
        }
    }
}

/// What a command printed and whether it exited 0.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs an external program to completion. The seam the tests replace.
pub trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// One `setup systemd` run, with everything it would otherwise read from the
/// environment spelled out.
pub struct Install {
    pub unit: Unit,
    /// `~/.config/systemd/user` in production.
    pub unit_dir: PathBuf,
    /// Absolute path of the binary ExecStart runs.
    pub exec: PathBuf,
    /// `Environment=` assignments added to `[Service]`, in order.
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    Created,
    Unchanged,
    /// The previous file differed and was moved to `backup`.
    Updated {
        backup: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linger {
    On,
    Off,
    /// `loginctl` could not say: not installed, no logind, and so on.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub unit: Unit,
    pub unit_path: PathBuf,
    pub written: Written,
    /// One line per permission pastor tightened.
    pub fixed: Vec<String>,
    pub linger: Linger,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.unit.file_name();
        for line in &self.fixed {
            writeln!(f, "{line}")?;
        }
        match &self.written {
            Written::Created => writeln!(f, "wrote {}", self.unit_path.display())?,
            Written::Unchanged => writeln!(f, "{} is up to date", self.unit_path.display())?,
            Written::Updated { backup } => {
                writeln!(
                    f,
                    "updated {} (previous version kept as {})",
                    self.unit_path.display(),
                    backup.display()
                )?;
                writeln!(
                    f,
                    "a running {name} keeps the old unit until `systemctl --user restart {name}`"
                )?;
            }
        }
        writeln!(f, "enabled and started {name}")?;
        match &self.linger {
            Linger::On => writeln!(f, "lingering is on: {name} runs without a login session"),
            Linger::Off => writeln!(
                f,
                "lingering is off, so {name} stops when you log out; turn it on with:\n  loginctl enable-linger"
            ),
            Linger::Unknown(why) => writeln!(
                f,
                "could not check lingering ({why}); {name} needs `loginctl enable-linger` to run without a login"
            ),
        }
    }
}

impl Install {
    /// Tighten permissions (pastor.service only), write the unit, enable and
    /// start it, then check lingering. Fails only on a file or `systemctl`
    /// error; a lingering check that cannot run is reported, not fatal.
    pub fn run(&self, runner: &dyn Runner, paths: &Paths) -> anyhow::Result<Report> {
        // herdr.service goes on flock machines that may never run pastor
        // itself; creating ~/.config/pastor there would be litter.
        let fixed = match self.unit {
            Unit::Pastor => secure(paths)?,
            Unit::Herdr => Vec::new(),
        };
        let unit_path = self.unit_dir.join(self.unit.file_name());
        let written = self.write(&unit_path)?;
        systemctl(runner, &["daemon-reload"])?;
        systemctl(runner, &["enable", "--now", self.unit.file_name()])?;
        Ok(Report {
            unit: self.unit,
            unit_path,
            written,
            fixed,
            linger: linger(runner),
        })
    }

    fn write(&self, unit_path: &Path) -> anyhow::Result<Written> {
        let text = render(self.unit.template(), &self.exec, &self.env);
        std::fs::create_dir_all(&self.unit_dir)
            .with_context(|| format!("create {}", self.unit_dir.display()))?;
        let written = match std::fs::read_to_string(unit_path) {
            Ok(old) if old == text => return Ok(Written::Unchanged),
            Ok(_) => {
                // Never destroy: the old file may carry hand edits.
                let backup = unit_path.with_extension("service.bak");
                std::fs::rename(unit_path, &backup)
                    .with_context(|| format!("back up {}", unit_path.display()))?;
                Written::Updated { backup }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Written::Created,
            Err(e) => return Err(e).with_context(|| format!("read {}", unit_path.display())),
        };
        std::fs::write(unit_path, text)
            .with_context(|| format!("write {}", unit_path.display()))?;
        Ok(written)
    }
}

/// The shipped unit with ExecStart's program replaced by `exec` and one
/// `Environment=` line per assignment right after it.
pub fn render(template: &str, exec: &Path, env: &[(String, String)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    for line in template.lines() {
        if let Some(rest) = line.strip_prefix("ExecStart=") {
            let args = rest.split_once(' ').map(|(_, a)| a);
            out.push_str("ExecStart=");
            out.push_str(&quote(&exec.to_string_lossy()));
            if let Some(args) = args {
                out.push(' ');
                out.push_str(args);
            }
            out.push('\n');
            for (k, v) in env {
                out.push_str("Environment=");
                out.push_str(&quote(&format!("{k}={v}")));
                out.push('\n');
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A unit-file word: `%` doubled so systemd does not expand it as a
/// specifier, double-quoted when it has whitespace, quotes or backslashes.
fn quote(s: &str) -> String {
    let s = s.replace('%', "%%");
    if s.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c == '\'')
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

fn systemctl(runner: &dyn Runner, args: &[&str]) -> anyhow::Result<()> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    let shown = format!("systemctl {}", full.join(" "));
    let out = runner
        .run("systemctl", &full)
        .with_context(|| format!("run `{shown}`"))?;
    if !out.success {
        anyhow::bail!("`{shown}` failed: {}", out.stderr.trim());
    }
    Ok(())
}

fn linger(runner: &dyn Runner) -> Linger {
    let uid = match current_uid() {
        Ok(uid) => uid.to_string(),
        Err(e) => return Linger::Unknown(e.to_string()),
    };
    let args = ["show-user", uid.as_str(), "--property=Linger", "--value"];
    match runner.run("loginctl", &args) {
        Ok(out) if out.success => match out.stdout.trim() {
            "yes" => Linger::On,
            "no" => Linger::Off,
            other => Linger::Unknown(format!("loginctl said {other:?}")),
        },
        Ok(out) => Linger::Unknown(format!("loginctl: {}", out.stderr.trim())),
        Err(e) => Linger::Unknown(format!("loginctl: {e}")),
    }
}

/// The uid this process runs as, without libc: `/proc/self` is owned by it.
fn current_uid() -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata("/proc/self")?.uid())
}

fn which(name: &str, path_var: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Bring pastor's files to the modes the spec lists: config and state dirs
/// 0700 (created if missing), the daemon socket and every plugin `.env` 0600.
/// Returns one line per change. Symlinks are left alone: chmod would follow
/// them to a file pastor does not own.
pub fn secure(paths: &Paths) -> anyhow::Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let mut fixed = Vec::new();
    for dir in [&paths.config_dir, &paths.state_dir] {
        let before = std::fs::symlink_metadata(dir)
            .ok()
            .map(|m| m.permissions().mode() & 0o777);
        create_private_dir(dir)?;
        match before {
            None => fixed.push(format!("created {} (0700)", dir.display())),
            Some(m) if m != 0o700 => {
                fixed.push(format!("{}: {m:04o} -> 0700", dir.display()));
            }
            Some(_) => {}
        }
    }
    let mut files = vec![paths.socket_file()];
    if let Ok(entries) = std::fs::read_dir(paths.config_dir.join("plugins")) {
        let mut envs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path().join(".env"))
            .collect();
        envs.sort();
        files.extend(envs);
    }
    for file in files {
        let Ok(md) = std::fs::symlink_metadata(&file) else {
            continue;
        };
        let mode = md.permissions().mode() & 0o777;
        if md.file_type().is_symlink() || mode & 0o077 == 0 {
            continue;
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", file.display()))?;
        fixed.push(format!("{}: {mode:04o} -> 0600", file.display()));
    }
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    type Reply = Box<dyn Fn(&str) -> std::io::Result<CmdOutput>>;

    /// Records every call and answers from `reply`.
    struct FakeRunner {
        calls: Mutex<Vec<String>>,
        reply: Reply,
    }

    impl FakeRunner {
        fn new(reply: impl Fn(&str) -> std::io::Result<CmdOutput> + 'static) -> FakeRunner {
            FakeRunner {
                calls: Mutex::new(Vec::new()),
                reply: Box::new(reply),
            }
        }
        /// systemctl succeeds, loginctl answers `linger`.
        fn ok(linger: &'static str) -> FakeRunner {
            FakeRunner::new(move |call| {
                Ok(CmdOutput {
                    success: true,
                    stdout: if call.starts_with("loginctl") {
                        format!("{linger}\n")
                    } else {
                        String::new()
                    },
                    stderr: String::new(),
                })
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
            let call = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(call.clone());
            (self.reply)(&call)
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn chmod(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

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
        fn install(&self, unit: Unit) -> Install {
            Install {
                unit,
                unit_dir: self.tmp.path().join("systemd/user"),
                exec: PathBuf::from("/opt/bin/pastor"),
                env: vec![("PATH".into(), "/usr/bin:/opt/bin".into())],
            }
        }
    }

    #[test]
    fn shipped_units_meet_the_spec() {
        for unit in [Unit::Pastor, Unit::Herdr] {
            let t = unit.template();
            for line in [
                "After=network-online.target",
                "Restart=on-failure",
                "StandardOutput=journal",
                "WantedBy=default.target",
            ] {
                assert!(t.lines().any(|l| l == line), "{unit:?} lacks {line}");
            }
            assert_eq!(t.lines().filter(|l| l.starts_with("ExecStart=")).count(), 1);
        }
        assert!(PASTOR_UNIT.contains("ExecStart=%h/.cargo/bin/pastor serve"));
        assert!(HERDR_UNIT.contains("ExecStart=%h/.local/bin/herdr server"));
    }

    #[test]
    fn render_replaces_the_program_and_keeps_the_arguments() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/home/u/my bin".to_string()),
            ("PASTOR_STATE_DIR".to_string(), "/s/100%".to_string()),
        ];
        let text = render(PASTOR_UNIT, Path::new("/home/u/.cargo/bin/pastor"), &env);
        let service: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("ExecStart="))
            .take(3)
            .collect();
        assert_eq!(
            service,
            [
                "ExecStart=/home/u/.cargo/bin/pastor serve",
                "Environment=\"PATH=/usr/bin:/home/u/my bin\"",
                "Environment=PASTOR_STATE_DIR=/s/100%%",
            ]
        );
        assert!(text.contains("Restart=on-failure\n"));
    }

    #[test]
    fn render_quotes_an_awkward_binary_path() {
        let text = render(HERDR_UNIT, Path::new("/opt/my \"tools\"/herdr"), &[]);
        assert!(
            text.contains("ExecStart=\"/opt/my \\\"tools\\\"/herdr\" server\n"),
            "{text}"
        );
    }

    #[test]
    fn installs_enables_and_reports_lingering_off() {
        let e = env();
        let runner = FakeRunner::ok("no");
        let report = e.install(Unit::Pastor).run(&runner, &e.paths).unwrap();

        let unit_path = e.tmp.path().join("systemd/user/pastor.service");
        assert_eq!(report.unit_path, unit_path);
        assert_eq!(report.written, Written::Created);
        let text = std::fs::read_to_string(&unit_path).unwrap();
        assert!(text.contains("ExecStart=/opt/bin/pastor serve\n"), "{text}");
        assert!(
            text.contains("Environment=PATH=/usr/bin:/opt/bin\n"),
            "{text}"
        );

        let uid = current_uid().unwrap();
        assert_eq!(
            runner.calls(),
            [
                "systemctl --user daemon-reload".to_string(),
                "systemctl --user enable --now pastor.service".to_string(),
                format!("loginctl show-user {uid} --property=Linger --value"),
            ]
        );
        assert_eq!(report.linger, Linger::Off);
        let shown = report.to_string();
        assert!(shown.contains("\n  loginctl enable-linger\n"), "{shown}");
        assert!(
            shown.contains("enabled and started pastor.service"),
            "{shown}"
        );
    }

    #[test]
    fn lingering_on_prints_no_hint() {
        let e = env();
        let report = e
            .install(Unit::Pastor)
            .run(&FakeRunner::ok("yes"), &e.paths)
            .unwrap();
        assert_eq!(report.linger, Linger::On);
        assert!(!report.to_string().contains("enable-linger"));
    }

    #[test]
    fn a_failing_loginctl_is_reported_not_fatal() {
        let e = env();
        let runner = FakeRunner::new(|call| {
            if call.starts_with("loginctl") {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                Ok(CmdOutput {
                    success: true,
                    ..Default::default()
                })
            }
        });
        let report = e.install(Unit::Pastor).run(&runner, &e.paths).unwrap();
        assert!(matches!(report.linger, Linger::Unknown(_)));
        assert!(report.to_string().contains("loginctl enable-linger"));
    }

    #[test]
    fn a_failing_systemctl_is_an_error_with_its_stderr() {
        let e = env();
        let runner = FakeRunner::new(|call| {
            Ok(CmdOutput {
                success: !call.contains("enable"),
                stdout: String::new(),
                stderr: "Failed to connect to bus: No medium found\n".into(),
            })
        });
        let err = e
            .install(Unit::Pastor)
            .run(&runner, &e.paths)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("systemctl --user enable --now pastor.service"),
            "{err}"
        );
        assert!(err.contains("Failed to connect to bus"), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("loginctl")));
    }

    #[test]
    fn rerunning_leaves_an_identical_unit_alone() {
        let e = env();
        let install = e.install(Unit::Pastor);
        install.run(&FakeRunner::ok("yes"), &e.paths).unwrap();
        let again = install.run(&FakeRunner::ok("yes"), &e.paths).unwrap();
        assert_eq!(again.written, Written::Unchanged);
        assert!(
            !e.tmp
                .path()
                .join("systemd/user/pastor.service.bak")
                .exists()
        );
    }

    #[test]
    fn a_changed_unit_is_backed_up_before_it_is_replaced() {
        let e = env();
        let dir = e.tmp.path().join("systemd/user");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pastor.service"), "# hand edited\n").unwrap();

        let report = e
            .install(Unit::Pastor)
            .run(&FakeRunner::ok("yes"), &e.paths)
            .unwrap();
        let backup = dir.join("pastor.service.bak");
        assert_eq!(
            report.written,
            Written::Updated {
                backup: backup.clone()
            }
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "# hand edited\n");
        assert!(
            report
                .to_string()
                .contains("systemctl --user restart pastor.service")
        );
    }

    #[test]
    fn herdr_unit_skips_pastor_dirs() {
        let e = env();
        let mut install = e.install(Unit::Herdr);
        install.exec = PathBuf::from("/home/u/.local/bin/herdr");
        let runner = FakeRunner::ok("yes");
        let report = install.run(&runner, &e.paths).unwrap();

        let text = std::fs::read_to_string(&report.unit_path).unwrap();
        assert!(report.unit_path.ends_with("herdr.service"));
        assert!(text.contains("ExecStart=/home/u/.local/bin/herdr server\n"));
        assert!(
            runner
                .calls()
                .contains(&"systemctl --user enable --now herdr.service".into())
        );
        assert!(!e.paths.config_dir.exists());
        assert!(!e.paths.state_dir.exists());
    }

    #[test]
    fn secure_creates_missing_dirs_private() {
        let e = env();
        let fixed = secure(&e.paths).unwrap();
        assert_eq!(fixed.len(), 2, "{fixed:?}");
        assert_eq!(mode(&e.paths.config_dir), 0o700);
        assert_eq!(mode(&e.paths.state_dir), 0o700);
        assert!(secure(&e.paths).unwrap().is_empty(), "idempotent");
    }

    #[test]
    fn secure_tightens_dirs_socket_and_env_files() {
        let e = env();
        e.paths.ensure().unwrap();
        chmod(&e.paths.config_dir, 0o755);
        std::fs::write(e.paths.socket_file(), "").unwrap();
        chmod(&e.paths.socket_file(), 0o666);
        let plugin = e.paths.config_dir.join("plugins/github");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join(".env"), "GITHUB_TOKEN=x\n").unwrap();
        chmod(&plugin.join(".env"), 0o644);
        let quiet = e.paths.config_dir.join("plugins/linear");
        std::fs::create_dir_all(&quiet).unwrap();
        std::fs::write(quiet.join(".env"), "").unwrap();
        chmod(&quiet.join(".env"), 0o600);

        let fixed = secure(&e.paths).unwrap();
        assert_eq!(fixed.len(), 3, "{fixed:?}");
        assert!(fixed[0].ends_with("0755 -> 0700"), "{fixed:?}");
        assert!(fixed[1].contains("pastor.sock: 0666 -> 0600"), "{fixed:?}");
        assert!(fixed[2].contains("github/.env: 0644 -> 0600"), "{fixed:?}");
        assert_eq!(mode(&e.paths.config_dir), 0o700);
        assert_eq!(mode(&e.paths.socket_file()), 0o600);
        assert_eq!(mode(&plugin.join(".env")), 0o600);
    }

    #[test]
    fn secure_does_not_follow_a_symlinked_env() {
        let e = env();
        e.paths.ensure().unwrap();
        let outside = e.tmp.path().join("outside.env");
        std::fs::write(&outside, "").unwrap();
        chmod(&outside, 0o644);
        let plugin = e.paths.config_dir.join("plugins/linked");
        std::fs::create_dir_all(&plugin).unwrap();
        std::os::unix::fs::symlink(&outside, plugin.join(".env")).unwrap();

        assert!(secure(&e.paths).unwrap().is_empty());
        assert_eq!(mode(&outside), 0o644);
    }

    #[test]
    fn which_finds_only_executables() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("herdr"), "").unwrap();
        chmod(&a.join("herdr"), 0o644);
        std::fs::write(b.join("herdr"), "").unwrap();
        chmod(&b.join("herdr"), 0o755);
        let path = std::env::join_paths([&a, &b]).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(which("herdr", path), Some(b.join("herdr")));
        assert_eq!(which("nope", path), None);
    }
}
