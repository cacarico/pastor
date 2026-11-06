//! What bootstrap learns about the machine before it changes anything.
//! Every probe goes through the [`Runner`], so the tests script the answers.

use std::path::PathBuf;

use crate::setup::Runner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `linux` or `darwin` (lowercased `uname -s`).
    pub os: String,
    /// `uname -m`, with macOS's `arm64` read as `aarch64`.
    pub arch: String,
    /// `getconf LONG_BIT`: 32 on a 64-bit kernel over a 32-bit userland.
    pub bits: u8,
    pub init: Init,
    /// The uid bootstrap runs as.
    pub uid: u32,
    /// The login user's name (`id -un`).
    pub login: String,
    /// The login user's home, from the user database, not `$HOME`, which a
    /// wrapper may have changed.
    pub home: PathBuf,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Launchd,
    Other(String),
}

impl Facts {
    /// The facts a release is picked by (`artifacts::target_for`).
    pub fn release(&self) -> crate::provision::run::Facts {
        crate::provision::run::Facts {
            os: self.os.clone(),
            arch: self.arch.clone(),
            bits: self.bits,
            distro: None,
            like: Vec::new(),
        }
    }
}

/// Trimmed stdout of a command that exited 0.
pub(crate) fn stdout(r: &dyn Runner, program: &str, args: &[&str]) -> Option<String> {
    match r.run(program, args) {
        Ok(out) if out.success => Some(out.stdout.trim().to_string()),
        _ => None,
    }
}

pub fn facts(r: &dyn Runner) -> Facts {
    let os = stdout(r, "uname", &["-s"])
        .unwrap_or_default()
        .to_lowercase();
    let arch = match stdout(r, "uname", &["-m"]).unwrap_or_default().as_str() {
        "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    };
    // A 64-bit kernel can run a 32-bit userland (Raspberry Pi OS); a 64-bit
    // binary does not run there, so the userland decides.
    let bits = match stdout(r, "getconf", &["LONG_BIT"]).as_deref() {
        Some("32") => 32,
        _ => 64,
    };
    let init = match os.as_str() {
        "darwin" => Init::Launchd,
        // systemd's own test for "booted with systemd" (sd_booted).
        _ if r
            .run("test", &["-d", "/run/systemd/system"])
            .is_ok_and(|o| o.success) =>
        {
            Init::Systemd
        }
        _ => Init::Other(stdout(r, "ps", &["-p", "1", "-o", "comm="]).unwrap_or_default()),
    };
    let uid = stdout(r, "id", &["-u"])
        .and_then(|u| u.parse().ok())
        .unwrap_or(u32::MAX);
    let login = stdout(r, "id", &["-un"]).unwrap_or_default();
    let home = passwd_home(r, &os, &login).unwrap_or_default();
    Facts {
        os,
        arch,
        bits,
        init,
        uid,
        login,
        home,
        hostname: crate::config::hostname(),
    }
}

/// `name`'s home from the user database: `getent passwd` on Linux, `dscl`
/// on macOS, which has no getent.
fn passwd_home(r: &dyn Runner, os: &str, name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let home = if os == "darwin" {
        let user = format!("/Users/{name}");
        let out = stdout(r, "dscl", &[".", "-read", &user, "NFSHomeDirectory"])?;
        out.lines()
            .find_map(|l| l.strip_prefix("NFSHomeDirectory:"))
            .map(|v| v.trim().to_string())?
    } else {
        // name:password:uid:gid:gecos:home:shell
        let line = stdout(r, "getent", &["passwd", name])?;
        line.split(':').nth(5)?.to_string()
    };
    (!home.is_empty()).then(|| PathBuf::from(home))
}
