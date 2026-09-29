//! The one place that builds ssh's argv, for the head client (`head.rs`) and
//! the machine transport (`herdr/transport.rs`): the destination behind `--`,
//! `BatchMode`, and the shared `ControlMaster` whose socket must fit in
//! `sun_path`, or no multiplexing at all when it cannot.

use std::path::{Path, PathBuf};

use crate::config::Paths;
use crate::herdr::transport::posix_command;

/// OpenSSH creates the master socket at `<path>.XXXXXXXXXXXXXXXX` and renames it
/// into place, so the staged name is 17 bytes longer than the ControlPath.
pub(crate) const CONTROL_PATH_STAGING: usize = 17;

/// `sun_path` is 108 bytes on Linux and 104 on macOS and the BSDs, including
/// the terminating NUL.
pub(crate) const UNIX_PATH_MAX: usize = unix_path_max(cfg!(target_os = "linux"));

const fn unix_path_max(linux: bool) -> usize {
    if linux { 108 } else { 104 }
}

/// `%C` expands to a hex SHA-1 of the connection parameters.
const EXPANDED_C: usize = 40;

/// One ssh invocation that runs a remote command. Each side sets what it needs
/// beyond the shared part; `args` is the only thing that turns it into options.
#[derive(Debug, Clone, Copy)]
pub struct Ssh<'a> {
    /// The destination, as ssh takes it. Always after `--`, so a value that
    /// starts with `-` is never read as an option.
    pub target: &'a str,
    /// The `ControlPath` for the shared master, from `fitting_control_path`.
    /// `None` turns multiplexing off outright.
    pub control_path: Option<&'a Path>,
    /// `ControlPersist`, as ssh takes it (`600`, `60s`).
    pub control_persist: &'a str,
    /// `ServerAliveInterval=15` and `ServerAliveCountMax=3`: a dead link is
    /// given up on after 45s.
    pub keepalive: bool,
    /// `-T`: no pseudo-terminal.
    pub no_tty: bool,
}

impl Ssh<'_> {
    /// The argv after `ssh`. Nothing here goes through a local shell: the
    /// target and the ControlPath are argv elements of their own, and only the
    /// remote command, which the remote login shell does parse, is quoted, by
    /// `posix_command`.
    pub fn args(&self, remote: &str) -> Vec<String> {
        let mut opts = vec!["BatchMode=yes".to_string()];
        if self.keepalive {
            opts.push("ServerAliveInterval=15".into());
            opts.push("ServerAliveCountMax=3".into());
        }
        match self.control_path {
            // One authenticated master per destination, reused by every
            // request: without it each request would pay a full handshake.
            Some(path) => {
                opts.push("ControlMaster=auto".into());
                opts.push(format!("ControlPath={}", path.display()));
                opts.push(format!("ControlPersist={}", self.control_persist));
            }
            // Said outright, so a `ControlMaster` in the user's ssh config
            // cannot bring back the socket that did not fit.
            None => {
                opts.push("ControlMaster=no".into());
                opts.push("ControlPath=none".into());
            }
        }
        let mut args = Vec::new();
        for opt in opts {
            args.push("-o".to_string());
            args.push(opt);
        }
        if self.no_tty {
            args.push("-T".into());
        }
        args.push("--".into());
        args.push(self.target.to_string());
        args.push(posix_command(remote));
        args
    }
}

/// The length of `path` as ssh will have expanded it: `%%` is one byte, `%C` is
/// a 40-byte hash. Other `%` tokens do not appear in paths pastor builds.
pub(crate) fn expanded_len(path: &Path) -> usize {
    let s = path.to_string_lossy();
    let mut len = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            len += c.len_utf8();
            continue;
        }
        match chars.next() {
            Some('%') => len += 1,
            Some('C') => len += EXPANDED_C,
            Some(other) => len += 1 + other.len_utf8(),
            None => len += 1,
        }
    }
    len
}

/// The ControlPath for `name` (a machine, or `head`), with the name shortened
/// until ssh's staged socket name fits in `sun_path`. The name is only there
/// to make the socket recognisable in `ls`; `%C` (a hash of the destination)
/// is what keeps destinations apart, so cutting the name loses nothing. `None`
/// only when even a bare `-%C` does not fit, which takes an unusually deep
/// state dir.
pub fn fitting_control_path(paths: &Paths, name: &str) -> Option<PathBuf> {
    let chars = name.chars().count();
    (0..=chars).rev().find_map(|keep| {
        let short: String = name.chars().take(keep).collect();
        let path = paths.ssh_control_path(&short);
        control_path_fits(&path).then_some(path)
    })
}

/// Would ssh's staged socket name fit in `sun_path`? A ControlPath that does not
/// makes every connection fail, so a path that is too long drops the
/// multiplexing options instead.
pub(crate) fn control_path_fits(path: &Path) -> bool {
    // `+ 1` for the NUL would read better, but clippy prefers the strict form.
    expanded_len(path) + CONTROL_PATH_STAGING < UNIX_PATH_MAX
}

/// The master socket lives in the ControlPath's directory; ssh creates the
/// socket itself but not the directory, and it must not be world-readable.
/// Every ssh that carries a ControlPath can be the one that starts the master,
/// so each of them calls this first. The path is in ssh's escaped form (`%%`
/// for a literal `%`), so undo that before touching the filesystem or a `%` in
/// the state dir would create one directory while ssh looks for another.
pub fn ensure_control_dir(control_path: Option<&Path>) -> anyhow::Result<()> {
    let Some(parent) = control_path.and_then(|p| p.parent()) else {
        return Ok(());
    };
    let literal = PathBuf::from(parent.to_string_lossy().replace("%%", "%"));
    crate::config::create_private_dir(&literal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(args: &[String], o: &str) -> bool {
        args.windows(2).any(|w| w[0] == "-o" && w[1] == o)
    }

    fn ssh<'a>(target: &'a str, control_path: Option<&'a Path>) -> Ssh<'a> {
        Ssh {
            target,
            control_path,
            control_persist: "600",
            keepalive: false,
            no_tty: false,
        }
    }

    #[test]
    fn multiplexes_over_the_control_path_and_never_prompts() {
        let path = Path::new("/tmp/s/ssh/pi-3-%C");
        let args = ssh("fleet@pi-3", Some(path)).args("herdr remote-api-bridge");
        for o in [
            "BatchMode=yes",
            "ControlMaster=auto",
            "ControlPath=/tmp/s/ssh/pi-3-%C",
            "ControlPersist=600",
        ] {
            assert!(opt(&args, o), "{o}: {args:?}");
        }
        assert_eq!(
            &args[args.len() - 3..],
            ["--", "fleet@pi-3", "sh -c 'herdr remote-api-bridge'"]
        );
        assert!(
            !args
                .iter()
                .any(|a| a == "-T" || a.starts_with("ServerAlive"))
        );
    }

    #[test]
    fn keepalive_and_no_tty_are_asked_for() {
        let args = Ssh {
            keepalive: true,
            no_tty: true,
            ..ssh("fleet@pi-3", None)
        }
        .args("true");
        assert!(opt(&args, "ServerAliveInterval=15"), "{args:?}");
        assert!(opt(&args, "ServerAliveCountMax=3"), "{args:?}");
        assert_eq!(args[args.len() - 4], "-T");
    }

    /// No socket fits: ssh still connects, without multiplexing, whatever
    /// `~/.ssh/config` says.
    #[test]
    fn no_control_path_turns_multiplexing_off() {
        let args = ssh("fleet@pi-3", None).args("true");
        for o in ["BatchMode=yes", "ControlMaster=no", "ControlPath=none"] {
            assert!(opt(&args, o), "{o}: {args:?}");
        }
        assert!(
            !args.iter().any(|a| a.starts_with("ControlPersist")),
            "{args:?}"
        );
        assert_eq!(&args[args.len() - 3..], ["--", "fleet@pi-3", "sh -c true"]);
    }

    #[test]
    fn a_dash_prefixed_destination_is_never_an_option() {
        let args = ssh("-oProxyCommand=evil", None).args("true");
        let dash = args.iter().position(|a| a == "--").expect("has --");
        assert_eq!(args[dash + 1], "-oProxyCommand=evil");
        assert!(
            !args[..dash].iter().any(|a| a.contains("ProxyCommand")),
            "{args:?}"
        );
    }

    #[test]
    fn a_state_dir_too_deep_for_any_socket_has_no_control_path() {
        let deep = Paths::new("/c", format!("/tmp/{}", "d".repeat(80)));
        assert_eq!(fitting_control_path(&deep, "pi-3"), None);
        let paths = Paths::new("/c", "/tmp/s");
        assert_eq!(
            fitting_control_path(&paths, "pi-3"),
            Some(paths.ssh_control_path("pi-3"))
        );
    }

    #[test]
    fn a_fresh_state_dir_gets_a_private_ssh_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // A `%` in the state dir is escaped in the ControlPath; the directory
        // made must be the literal one ssh will look in.
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s%1"));
        ensure_control_dir(Some(&paths.ssh_control_path("pi-3"))).unwrap();
        let md = std::fs::metadata(paths.ssh_dir()).unwrap();
        assert!(md.is_dir());
        assert_eq!(md.permissions().mode() & 0o777, 0o700);
        assert!(!tmp.path().join("s%%1").exists());
        ensure_control_dir(None).unwrap();
    }

    #[test]
    fn expanded_len_counts_ssh_escapes() {
        assert_eq!(expanded_len(Path::new("/a/b")), 4);
        assert_eq!(expanded_len(Path::new("%%")), 1);
        assert_eq!(expanded_len(Path::new("/a-%C")), 3 + EXPANDED_C);
        assert_eq!(unix_path_max(true), 108);
        assert_eq!(unix_path_max(false), 104);
        let expected = if cfg!(target_os = "linux") { 108 } else { 104 };
        assert_eq!(UNIX_PATH_MAX, expected);
        // The guard's boundary: exactly `UNIX_PATH_MAX` staged bytes is fine.
        let fits = "x".repeat(UNIX_PATH_MAX - CONTROL_PATH_STAGING - 1);
        assert!(control_path_fits(Path::new(&fits)));
        assert!(!control_path_fits(Path::new(&format!("{fits}x"))));
    }

    proptest::proptest! {
        /// Whatever the target and ControlPath, the target is the word right
        /// after the first `--`, and everything before `--` is the same as for
        /// any other target: none of the options came from it.
        #[test]
        fn prop_target_sits_right_after_the_double_dash(
            target in "[^-\\s\\p{C}][^\\s\\p{C}]*",
            control_path in proptest::option::of("\\PC+"),
            keepalive in proptest::prelude::any::<bool>(),
            no_tty in proptest::prelude::any::<bool>(),
            remote in ".*",
        ) {
            proptest::prop_assume!(crate::config::flock::ssh_target_problem(&target).is_none());
            let control_path = control_path.map(PathBuf::from);
            let make = |target| Ssh {
                keepalive,
                no_tty,
                ..ssh(target, control_path.as_deref())
            }
            .args(&remote);
            let args = make(&target);
            let dash = args.iter().position(|a| a == "--").expect("a `--`");
            proptest::prop_assert_eq!(&args[dash + 1], &target);
            proptest::prop_assert_eq!(&args[dash + 2], &posix_command(&remote));
            proptest::prop_assert_eq!(args.len(), dash + 3);
            proptest::prop_assert_eq!(&args[..dash], &make("other")[..dash]);
        }
    }
}
