//! What bootstrap learns about the machine before it changes anything.
//! Every probe goes through the [`Runner`], so the tests script the answers.

use std::path::PathBuf;

use crate::cli::CliError;
use crate::setup::Runner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `linux` or `darwin` (lowercased `uname -s`).
    pub os: String,
    /// `uname -m`, with macOS's `arm64` read as `aarch64`.
    pub arch: String,
    /// `getconf LONG_BIT`: 32 on a 64-bit kernel over a 32-bit userland.
    pub userland_bits: u8,
    pub init: Init,
    pub pkg: Option<Pkg>,
    /// The uid bootstrap runs as.
    pub uid: u32,
    /// `sudo -n true` works. Root needs no sudo, so it is probed there too
    /// and never assumed.
    pub sudo: bool,
    /// The login user's home, from the user database, not `$HOME`, which
    /// `sudo sh` keeps from the user who ran it.
    pub home: PathBuf,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Launchd,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkg {
    Apt,
    Dnf,
    Pacman,
    Brew,
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
    let userland_bits = match stdout(r, "getconf", &["LONG_BIT"]).as_deref() {
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
    let pkg = stdout(
        r,
        "sh",
        &[
            "-c",
            "for p in apt-get dnf pacman brew; do command -v $p >/dev/null 2>&1 && echo $p && break; done",
        ],
    )
    .and_then(|p| match p.as_str() {
        "apt-get" => Some(Pkg::Apt),
        "dnf" => Some(Pkg::Dnf),
        "pacman" => Some(Pkg::Pacman),
        "brew" => Some(Pkg::Brew),
        _ => None,
    });
    let uid = stdout(r, "id", &["-u"])
        .and_then(|u| u.parse().ok())
        .unwrap_or(u32::MAX);
    let sudo = r.run("sudo", &["-n", "true"]).is_ok_and(|o| o.success);
    let home = stdout(r, "id", &["-un"])
        .and_then(|name| passwd(r, &os, &name))
        .map(|(_, home)| home)
        .unwrap_or_default();
    Facts {
        os,
        arch,
        userland_bits,
        init,
        pkg,
        uid,
        sudo,
        home,
        hostname: crate::config::hostname(),
    }
}

/// `name`'s uid and home from the user database: `getent passwd` on
/// Linux, `dscl` on macOS, which has no getent.
pub(crate) fn passwd(r: &dyn Runner, os: &str, name: &str) -> Option<(u32, PathBuf)> {
    let (uid, home) = if os == "darwin" {
        let user = format!("/Users/{name}");
        let out = stdout(
            r,
            "dscl",
            &[".", "-read", &user, "UniqueID", "NFSHomeDirectory"],
        )?;
        let field = |key: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim().to_string())
        };
        (field("UniqueID:")?, field("NFSHomeDirectory:")?)
    } else {
        // name:password:uid:gid:gecos:home:shell
        let line = stdout(r, "getent", &["passwd", name])?;
        let fields: Vec<&str> = line.split(':').collect();
        (fields.get(2)?.to_string(), fields.get(5)?.to_string())
    };
    match (uid.parse().ok(), home.is_empty()) {
        (Some(uid), false) => Some((uid, PathBuf::from(home))),
        _ => None,
    }
}

/// The release a binary is downloaded as for this machine. `pastor` gets
/// its release triple, by `install.sh`'s rules; `herdr` gets herdr's own key.
pub fn target_for(facts: &Facts, name: &str) -> anyhow::Result<String> {
    let bits64 = facts.userland_bits == 64;
    let target = match (name, facts.os.as_str(), facts.arch.as_str()) {
        ("pastor", "linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("pastor", "linux", "aarch64") if bits64 => Some("aarch64-unknown-linux-musl"),
        ("pastor", "linux", "aarch64" | "armv7l" | "armv7") => {
            Some("armv7-unknown-linux-musleabihf")
        }
        ("pastor", "linux", "riscv64") => Some("riscv64gc-unknown-linux-musl"),
        ("pastor", "darwin", "aarch64") => Some("aarch64-apple-darwin"),
        ("pastor", "darwin", "x86_64") => Some("x86_64-apple-darwin"),
        ("herdr", "linux", "x86_64") if bits64 => Some("linux-x86_64"),
        ("herdr", "linux", "aarch64") if bits64 => Some("linux-aarch64"),
        ("herdr", "darwin", "x86_64") => Some("macos-x86_64"),
        ("herdr", "darwin", "aarch64") => Some("macos-aarch64"),
        _ => None,
    };
    let Some(target) = target else {
        let userland = if bits64 {
            ""
        } else {
            " with a 32-bit userland"
        };
        return Err(CliError::err(
            "bootstrap_unsupported_platform",
            format!(
                "{name} ships no build for {} {}{userland}",
                facts.os, facts.arch
            ),
        ));
    };
    Ok(target.to_string())
}
