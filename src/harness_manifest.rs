//! Harness manifests: what pastor knows about an agent's CLI, as data. A
//! manifest names the harness's binary, how to read its version, how to check
//! its login (the command only, no secret), the sources it can be installed
//! from, in order, and where it keeps its settings, skills, hooks,
//! instructions and MCP servers.
//!
//! The harnesses pastor ships are in `harnesses/<id>.toml` in the repository,
//! embedded at build time. A file `<id>.toml` in the config dir's
//! `harnesses/` directory replaces the built-in with that id, or adds a
//! harness, with no Rust. The id is the agent `kind` the manifest describes.
//!
//! Nothing here runs a command or installs anything: it parses, validates and
//! answers questions (`version_command`, `paths`). The version pins, the
//! machine check and the bundle engine read from it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Context;
use serde::Deserialize;

use crate::config::Paths;

/// The built-in harnesses, by id, as their manifests read in the repository.
/// A test checks each parses and its id matches its file name.
pub const BUILTIN: &[(&str, &str)] = &[
    ("agy", include_str!("../harnesses/agy.toml")),
    ("claude", include_str!("../harnesses/claude.toml")),
    ("codex", include_str!("../harnesses/codex.toml")),
    ("opencode", include_str!("../harnesses/opencode.toml")),
];

/// The placeholders a release URL may hold: the harness version and the
/// machine's OS and architecture, as `uname` names them in lowercase.
pub const PLACEHOLDERS: [&str; 3] = ["version", "os", "arch"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    id: String,
    name: Option<String>,
    binary: Option<String>,
    #[serde(default)]
    bin_dirs: Vec<String>,
    version: Option<VersionFile>,
    login: Option<LoginFile>,
    #[serde(default)]
    install: Vec<InstallFile>,
    mcp: Option<McpFile>,
    update: Option<UpdateFile>,
    paths: PathsFile,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    method: McpMethod,
    file: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum McpMethod {
    Plugin,
    File,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateFile {
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionFile {
    args: Vec<String>,
    #[serde(default)]
    parse: Parse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginFile {
    check: Vec<String>,
    start: Option<Vec<String>>,
    pick: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
    code_after: Option<String>,
    paste: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, tag = "source", rename_all = "snake_case")]
enum InstallFile {
    Release {
        url: String,
        sha256_url: String,
    },
    Npm {
        package: String,
    },
    Script {
        url: String,
        #[serde(default)]
        shell: Shell,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathsFile {
    home_env: Option<String>,
    home: String,
    skills: Option<String>,
    hooks: Option<String>,
    instructions: Option<String>,
    settings: Option<String>,
    mcp: Option<String>,
    mcp_default: Option<String>,
    #[serde(default)]
    secrets: Vec<String>,
}

/// How a CLI's `--version` output is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parse {
    /// The first word of the last line that looks like a version
    /// (`2.1.310 (Claude Code)` is `2.1.310`): `harness::parse_version`.
    #[default]
    VersionWord,
}

/// The shell a vendor's installer script is piped into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    #[default]
    Sh,
    Bash,
}

/// One way to install the harness. A manifest lists them in the order to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// A file published at an https URL with the URL of its sha256 sum; the
    /// head downloads it once through the artifact cache and checks it.
    Release { url: String, sha256_url: String },
    /// An npm package.
    Npm { package: String },
    /// The vendor's installer, fetched over https and run by `shell` with
    /// `args`, in which `{version}` is the pin.
    Script {
        url: String,
        shell: Shell,
        args: Vec<String>,
    },
}

/// How to read a harness's version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The arguments that make the binary print its version.
    pub args: Vec<String>,
    pub parse: Parse,
}

/// How a person signs the harness in on a machine (`pastor login`): the
/// command pastor types into a pane there, and how to read what it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    /// The interactive login, run as the machine's login user.
    pub argv: Vec<String>,
    /// Text a list of ways to sign in shows, and the keys that pick one,
    /// pressed once when the pane shows it (agy opens on such a list).
    pub pick: Option<(String, Vec<String>)>,
    /// Text the device code follows: the code is the rest of its line when
    /// that is one word, else the first word of the next line with text
    /// (`login::read_code`). `None` for a login that shows a URL only.
    pub code_after: Option<String>,
    /// Text the pane shows while it waits for a code pasted back from the
    /// browser (`pastor login code`).
    pub paste: Option<String>,
}

/// Where a harness keeps what pastor places or reads, relative to its home
/// (`home`, or the value of `home_env` in an agent's env).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessPaths {
    /// The env var that moves the home, when the harness has one.
    pub home_env: Option<String>,
    /// The home when that var is unset, with a leading `~`.
    pub home: String,
    pub skills: Option<String>,
    pub hooks: Option<String>,
    pub instructions: Option<String>,
    /// The settings file.
    pub settings: Option<String>,
    /// The file that holds the MCP servers.
    pub mcp: Option<String>,
    /// Where that file is when the home is the default one, with a leading
    /// `~`, if it is not in the home then.
    pub mcp_default: Option<String>,
    /// The files that hold the harness's credentials, relative to the home
    /// or with a leading `~`; `*` in a name stands for any characters.
    /// `bundle apply` and `bundle adopt` refuse them and never read them.
    pub secrets: Vec<String>,
}

impl HarnessPaths {
    /// The home of an agent with `env`: its home var's value, else the
    /// default. `~` stays for the machine to expand.
    pub fn home_for(&self, env: &BTreeMap<String, String>) -> String {
        self.home_env
            .as_ref()
            .and_then(|var| env.get(var))
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| self.home.clone())
    }

    /// The secret files of the home at `home`, each with a leading `~` or
    /// as the home gives it.
    pub fn secrets_in(&self, home: &str) -> Vec<String> {
        self.secrets
            .iter()
            .map(|s| match s.starts_with('~') {
                true => s.clone(),
                false => format!("{}/{s}", home.trim_end_matches('/')),
            })
            .collect()
    }
}

/// How pastor places a harness's MCP servers from a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPlacement {
    /// A local plugin pastor writes into each home, in Claude's plugin
    /// format: its `.mcp.json` names each server with its env as `${VAR}`,
    /// which the harness expands, and two keys in `paths.settings` (which
    /// this needs) enable it. The file `paths.mcp` names is one a running
    /// harness rewrites, so pastor stays out of it.
    Plugin,
    /// A JSON file of `mcpServers` shared by every home of the kind, with a
    /// leading `~` and allowed outside the home; a kind with `paths.home_env`
    /// has several homes, which cannot share it, so it is refused there. The
    /// harness takes env values only, not names, so a server that needs env by
    /// name is skipped.
    File(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    /// The CLI on the machine's PATH.
    pub binary: Option<String>,
    /// Directories, with a leading `~`, where an installer puts it and a
    /// non-interactive shell's PATH may lack.
    pub bin_dirs: Vec<String>,
    pub version: Option<Version>,
    /// The command that exits 0 when the harness is signed in. It reads no
    /// credential and takes none.
    pub login_check: Option<Vec<String>>,
    /// The interactive login `pastor login` starts; `None` for a harness
    /// pastor cannot sign in.
    pub login_start: Option<LoginStart>,
    pub install: Vec<Install>,
    /// How bundle MCP servers are placed; none for a harness pastor does not
    /// place them for.
    pub mcp: Option<McpPlacement>,
    /// Env that keeps the harness from updating itself, set for an agent of
    /// a kind its machine pins to a version, so the pin holds. Empty when
    /// the harness has no such switch.
    pub update_env: BTreeMap<String, String>,
    pub paths: HarnessPaths,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A name a shell takes literally.
fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// A relative path inside a home: no root, no `..`.
fn inside(what: &str, p: &str) -> Result<(), String> {
    if p.is_empty()
        || p.starts_with('/')
        || p.starts_with('~')
        || p.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(format!(
            "{what}: {p:?} is not a path inside the home (no leading /, ~ or .., no empty part)"
        ));
    }
    Ok(())
}

/// A `~/...` path with nothing odd in it.
fn tilde(what: &str, p: &str) -> Result<(), String> {
    match p.strip_prefix("~/") {
        Some(rest) if !rest.is_empty() && !rest.split('/').any(|x| x.is_empty() || x == "..") => {
            Ok(())
        }
        _ => Err(format!("{what}: {p:?} must be a path starting with ~/")),
    }
}

/// An https URL, with only the placeholders in `allowed` in braces.
fn https(what: &str, url: &str, allowed: &[&str]) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("{what}: {url:?} is not an https URL"))?;
    if rest.is_empty()
        || rest
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '"')
    {
        return Err(format!("{what}: {url:?} is not a usable URL"));
    }
    let mut left = rest;
    while let Some(open) = left.find(['{', '}']) {
        if left.as_bytes()[open] == b'}' {
            return Err(format!("{what}: {url:?} has a stray }}"));
        }
        let close = left[open..]
            .find('}')
            .ok_or_else(|| format!("{what}: {url:?} has a {{ never closed"))?
            + open;
        let name = &left[open + 1..close];
        if !allowed.contains(&name) {
            return Err(format!(
                "{what}: {url:?} has {{{name}}}; the placeholders are {}",
                allowed
                    .iter()
                    .map(|p| format!("{{{p}}}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        left = &left[close + 1..];
    }
    Ok(())
}

/// An env var name: capitals, digits and `_`, not starting with a digit.
fn env_name(var: &str) -> bool {
    var.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && var
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// An argument to a vendor's installer: plain words, and `{version}`, which
/// is the pin. It reaches a shell on the machine, quoted.
fn script_arg(what: &str, arg: &str) -> Result<(), String> {
    let rest = arg.replace("{version}", "");
    if arg.is_empty()
        || !rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '=' | '/'))
    {
        return Err(format!(
            "{what}: {arg:?} is not a plain argument (letters, digits, . _ - + = / and {{version}})"
        ));
    }
    Ok(())
}

/// Text to look for on a pane: one line, not blank.
fn screen_text(what: &str, text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.contains(['\n', '\0']) {
        return Err(format!("{what}: needs one line of text the pane shows"));
    }
    Ok(())
}

/// `[login]`'s keys for `pastor login`, checked: `pick` and `keys` go
/// together, and none of them without `start`.
fn login_start(
    start: Option<Vec<String>>,
    pick: Option<String>,
    keys: Vec<String>,
    code_after: Option<String>,
    paste: Option<String>,
) -> Result<Option<LoginStart>, String> {
    let Some(argv) = start else {
        if pick.is_some() || !keys.is_empty() || code_after.is_some() || paste.is_some() {
            return Err("login: pick, keys, code_after and paste need login.start".into());
        }
        return Ok(None);
    };
    nonempty_argv("login.start", &argv)?;
    let pick = match (pick, keys.is_empty()) {
        (Some(text), false) => {
            screen_text("login.pick", &text)?;
            if keys.iter().any(|k| k.trim().is_empty()) {
                return Err("login.keys: a key name is empty".into());
            }
            Some((text, keys))
        }
        (None, true) => None,
        _ => return Err("login: pick and keys go together".into()),
    };
    for (what, text) in [("login.code_after", &code_after), ("login.paste", &paste)] {
        if let Some(text) = text {
            screen_text(what, text)?;
        }
    }
    Ok(Some(LoginStart {
        argv,
        pick,
        code_after,
        paste,
    }))
}

fn nonempty_argv(what: &str, argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err(format!("{what}: needs a command"));
    }
    if argv.iter().any(|a| a.contains('\0')) {
        return Err(format!("{what}: an argument holds a NUL"));
    }
    Ok(())
}

impl Manifest {
    /// Parse and validate one manifest file's text.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let f: ManifestFile = toml::from_str(text).map_err(|e| e.to_string())?;
        if !valid_id(&f.id) {
            return Err(format!(
                "id: {:?} is not a lowercase word of letters, digits and dashes",
                f.id
            ));
        }
        if let Some(b) = &f.binary
            && !plain(b)
        {
            return Err(format!("binary: {b:?} is not a plain command name"));
        }
        if f.binary.is_none() {
            for (set, what) in [
                (f.version.is_some(), "version"),
                (f.login.is_some(), "login"),
                (!f.install.is_empty(), "install"),
            ] {
                if set {
                    return Err(format!("{what}: needs the harness's `binary`"));
                }
            }
        }
        for d in &f.bin_dirs {
            tilde("bin_dirs", d)?;
            // These reach a shell on the machine, inside double quotes.
            if !d["~/".len()..].split('/').all(plain) {
                return Err(format!(
                    "bin_dirs: {d:?} is a plain path (letters, digits and . _ - +) starting with ~/"
                ));
            }
        }
        let version = match f.version {
            Some(v) => {
                if v.args.is_empty() || v.args.iter().any(|a| !plain(a.trim_start_matches('-'))) {
                    return Err(
                        "version.args: needs the arguments that print the version, such as [\"--version\"]"
                            .into(),
                    );
                }
                Some(Version {
                    args: v.args,
                    parse: v.parse,
                })
            }
            None => None,
        };
        let (login_check, login_start) = match f.login {
            Some(l) => {
                nonempty_argv("login.check", &l.check)?;
                (
                    Some(l.check),
                    login_start(l.start, l.pick, l.keys, l.code_after, l.paste)?,
                )
            }
            None => (None, None),
        };
        let mut install = Vec::new();
        for (i, step) in f.install.into_iter().enumerate() {
            let at = format!("install[{i}]");
            install.push(match step {
                InstallFile::Release { url, sha256_url } => {
                    https(&format!("{at}.url"), &url, &PLACEHOLDERS)?;
                    https(&format!("{at}.sha256_url"), &sha256_url, &PLACEHOLDERS)?;
                    Install::Release { url, sha256_url }
                }
                InstallFile::Npm { package } => {
                    let name = package.strip_prefix('@').unwrap_or(&package);
                    let fits = name.split('/').count() <= 2
                        && name.split('/').all(plain)
                        && (package.starts_with('@') == name.contains('/'));
                    if !fits {
                        return Err(format!("{at}.package: {package:?} is not an npm package"));
                    }
                    Install::Npm { package }
                }
                InstallFile::Script { url, shell, args } => {
                    https(&format!("{at}.url"), &url, &[])?;
                    for a in &args {
                        script_arg(&format!("{at}.args"), a)?;
                    }
                    Install::Script { url, shell, args }
                }
            });
        }
        let mcp = match f.mcp {
            None => None,
            Some(McpFile {
                method: McpMethod::Plugin,
                file: None,
            }) => Some(McpPlacement::Plugin),
            Some(McpFile {
                method: McpMethod::Plugin,
                file: Some(_),
            }) => return Err("mcp.file: the plugin method names no file".into()),
            Some(McpFile {
                method: McpMethod::File,
                file: Some(file),
            }) => {
                tilde("mcp.file", &file)?;
                Some(McpPlacement::File(file))
            }
            Some(McpFile {
                method: McpMethod::File,
                file: None,
            }) => return Err("mcp.file: the file method needs a file".into()),
        };
        let update_env = f.update.map(|u| u.env).unwrap_or_default();
        if f.binary.is_none() && !update_env.is_empty() {
            return Err("update: needs the harness's `binary`".into());
        }
        for (var, value) in &update_env {
            if !env_name(var) {
                return Err(format!("update.env: {var:?} is not an env var name"));
            }
            if value.contains('\0') {
                return Err(format!("update.env.{var}: holds a NUL"));
            }
        }
        let p = f.paths;
        tilde("paths.home", &p.home)?;
        if let Some(var) = &p.home_env
            && !env_name(var)
        {
            return Err(format!("paths.home_env: {var:?} is not an env var name"));
        }
        for (what, v) in [
            ("paths.skills", &p.skills),
            ("paths.hooks", &p.hooks),
            ("paths.instructions", &p.instructions),
            ("paths.settings", &p.settings),
            ("paths.mcp", &p.mcp),
        ] {
            if let Some(v) = v {
                inside(what, v)?;
            }
        }
        match &mcp {
            // One file for the kind cannot follow a home that moves: each
            // home would claim the same keys in it.
            Some(McpPlacement::File(_)) if p.home_env.is_some() => {
                return Err("mcp.method: the file method is one file for every home of the kind, so it cannot go with paths.home_env, which gives a kind several homes".into());
            }
            // The plugin is enabled by keys in the harness's settings file.
            Some(McpPlacement::Plugin) if p.settings.is_none() => {
                return Err("mcp.method: the plugin method enables its plugin in the settings file, so it needs paths.settings".into());
            }
            _ => {}
        }
        for s in &p.secrets {
            if s.starts_with('~') {
                tilde("paths.secrets", s)?;
            } else {
                inside("paths.secrets", s)?;
            }
        }
        if let Some(d) = &p.mcp_default {
            tilde("paths.mcp_default", d)?;
            if p.mcp.is_none() {
                return Err("paths.mcp_default: needs paths.mcp".into());
            }
        }
        Ok(Manifest {
            name: f.name.unwrap_or_else(|| f.id.clone()),
            id: f.id,
            binary: f.binary,
            bin_dirs: f.bin_dirs,
            version,
            login_check,
            login_start,
            install,
            mcp,
            update_env,
            paths: HarnessPaths {
                home_env: p.home_env,
                home: p.home,
                skills: p.skills,
                hooks: p.hooks,
                instructions: p.instructions,
                settings: p.settings,
                mcp: p.mcp,
                mcp_default: p.mcp_default,
                secrets: p.secrets,
            },
        })
    }

    /// The command that prints, as its last line, the version of this
    /// harness's CLI on the machine, or `none` when it is not there. ssh runs
    /// it in a shell whose PATH usually lacks the directories installers put
    /// the CLI in, so `bin_dirs` are added. `None` for a harness whose
    /// version pastor cannot read.
    pub fn version_command(&self) -> Option<String> {
        let version = self.version.as_ref()?;
        let bin = self.binary.as_deref()?;
        let dirs: String = self
            .bin_dirs
            .iter()
            .map(|d| format!("\"$HOME/{}\":", d.trim_start_matches("~/")))
            .collect();
        let args: String = version.args.iter().map(|a| format!(" {a}")).collect();
        Some(format!(
            "PATH={dirs}\"$PATH\"; command -v {bin} >/dev/null 2>&1 && {bin}{args} || printf none"
        ))
    }

    /// The PATH assignment that puts `bin_dirs` first, for a command run
    /// in a shell over ssh.
    fn path_prefix(&self) -> String {
        let dirs: String = self
            .bin_dirs
            .iter()
            .map(|d| format!("\"$HOME/{}\":", d.trim_start_matches("~/")))
            .collect();
        format!("PATH={dirs}\"$PATH\"; export PATH;")
    }

    /// The login check (`login.check`) as one command for the machine's
    /// shell, with `bin_dirs` on the PATH. `None` with no check.
    pub fn login_check_command(&self) -> Option<String> {
        let argv = self.login_check.as_ref()?;
        Some(format!("{} {}", self.path_prefix(), shell_words(argv)))
    }

    /// The interactive login (`login.start`) as one line to type into a
    /// pane's shell, whatever shell that is: `sh -c '<command>'`, with
    /// `bin_dirs` on the PATH. `None` for a harness with no `login.start`.
    pub fn login_start_line(&self) -> Option<String> {
        let start = self.login_start.as_ref()?;
        let command = format!("{} exec {}", self.path_prefix(), shell_words(&start.argv));
        Some(format!(
            "sh -c {}",
            crate::herdr::transport::shell_quote(&command)
        ))
    }

    /// The output of that command, read for the version: `None` for `none`
    /// and for output with no version in it.
    pub fn parse_version(&self, output: &str) -> Option<String> {
        match self.version.as_ref()?.parse {
            Parse::VersionWord => crate::harness::parse_version(output),
        }
    }
}

/// `argv` as words for a POSIX shell.
fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|a| crate::herdr::transport::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every harness pastor knows: the built-in ones, and those the user put in
/// the config dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harnesses {
    by_id: BTreeMap<String, Manifest>,
}

fn builtin_map() -> BTreeMap<String, Manifest> {
    BUILTIN
        .iter()
        .map(|(id, text)| {
            let m = Manifest::parse(text).unwrap_or_else(|e| panic!("built-in harness {id}: {e}"));
            assert_eq!(m.id, *id, "built-in harness {id}: id mismatch");
            (id.to_string(), m)
        })
        .collect()
}

static BUILTINS: LazyLock<Harnesses> = LazyLock::new(|| Harnesses {
    by_id: builtin_map(),
});

impl Default for Harnesses {
    fn default() -> Harnesses {
        Harnesses::builtin().clone()
    }
}

impl Harnesses {
    /// Only what ships in the binary.
    pub fn builtin() -> &'static Harnesses {
        &BUILTINS
    }

    /// The built-ins, then each `<id>.toml` in the config dir's `harnesses/`
    /// directory: one with a built-in's id replaces it, another adds a
    /// harness. A file that does not load fails the load with its path and
    /// the reason, rather than leaving the built-in in its place unseen.
    /// Plain files only; names starting with `.` and other extensions are
    /// skipped, and a missing directory is no files.
    pub fn load(paths: &Paths) -> anyhow::Result<Harnesses> {
        let mut by_id = builtin_map();
        let root = paths.harnesses_dir();
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Harnesses { by_id }),
            Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
        };
        let mut files: Vec<_> = entries
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("read {}", root.display()))?
            .into_iter()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|x| x == "toml")
                    && p.file_name()
                        .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
                    && p.is_file()
            })
            .collect();
        files.sort();
        for path in files {
            let m = load_file(&path)?;
            by_id.insert(m.id.clone(), m);
        }
        Ok(Harnesses { by_id })
    }

    /// These with `manifest` added, or in place of the one with its id.
    pub fn with(mut self, manifest: Manifest) -> Harnesses {
        self.by_id.insert(manifest.id.clone(), manifest);
        self
    }

    /// Only the manifests of `kinds`: what an actor that reads those needs,
    /// so a manifest it never reads does not replace it when edited.
    pub fn only<'a>(&self, kinds: impl IntoIterator<Item = &'a str>) -> Harnesses {
        Harnesses {
            by_id: kinds
                .into_iter()
                .filter_map(|k| self.by_id.get(k).map(|m| (k.to_string(), m.clone())))
                .collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Manifest> {
        self.by_id.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Manifest> {
        self.by_id.values()
    }

    /// The manifests whose version pastor can read, which is what a pin can
    /// name.
    pub fn versioned(&self) -> impl Iterator<Item = &Manifest> {
        self.iter().filter(|m| m.version.is_some())
    }
}

/// Read and validate one manifest file, whose name must be `<id>.toml`.
pub fn load_file(path: &Path) -> anyhow::Result<Manifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let m = Manifest::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem != m.id {
        anyhow::bail!(
            "{}: id {:?} does not match the file name {stem:?}",
            path.display(),
            m.id
        );
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = "id = \"tool\"\n[paths]\nhome = \"~/.tool\"\n";

    /// `extra` (top-level keys, then tables) before `[paths]`.
    fn with(extra: &str) -> String {
        format!("id = \"tool\"\n{extra}\n[paths]\nhome = \"~/.tool\"\n")
    }

    fn err(text: &str) -> String {
        Manifest::parse(text).unwrap_err()
    }

    #[test]
    fn the_built_in_manifests_load_and_match_their_ids() {
        let all = Harnesses::builtin();
        for kind in crate::provision::AGENT_KINDS {
            assert!(all.get(kind).is_some(), "{kind}");
        }
        for (id, _) in BUILTIN {
            assert_eq!(all.get(id).unwrap().id, *id);
        }
    }

    #[test]
    fn claude_and_agy_say_how_to_install_ask_and_sign_in() {
        let all = Harnesses::builtin();
        for kind in ["claude", "agy"] {
            let m = all.get(kind).unwrap();
            assert_eq!(m.binary.as_deref(), Some(kind));
            assert!(m.version.is_some(), "{kind} version");
            assert!(m.login_check.is_some(), "{kind} login");
            assert!(m.version_command().unwrap().contains(kind));
        }
        let claude = all.get("claude").unwrap();
        assert!(matches!(claude.install[0], Install::Npm { .. }));
        assert!(
            matches!(&claude.install[1], Install::Script { args, .. } if args == &["{version}"])
        );
        assert_eq!(claude.update_env["DISABLE_AUTOUPDATER"], "1");
        assert!(all.get("agy").unwrap().update_env.is_empty());
        assert_eq!(
            claude.login_check.as_deref().unwrap(),
            ["claude", "auth", "status", "--text"]
        );
    }

    /// Claude and agy say how `pastor login` starts them; codex does not.
    /// The line typed into a pane runs under `sh` with `bin_dirs` on the
    /// PATH, whatever the pane's shell is.
    #[test]
    fn claude_and_agy_say_how_to_start_their_login() {
        let all = Harnesses::builtin();
        let claude = all.get("claude").unwrap();
        let start = claude.login_start.as_ref().unwrap();
        assert_eq!(start.argv, ["claude", "auth", "login"]);
        assert_eq!(start.paste.as_deref(), Some("Paste code here"));
        assert_eq!(
            claude.login_start_line().unwrap(),
            "sh -c 'PATH=\"$HOME/.local/bin\":\"$HOME/.cargo/bin\":\"$PATH\"; export PATH; exec claude auth login'"
        );
        let agy = all.get("agy").unwrap().login_start.clone().unwrap();
        assert_eq!(agy.argv, ["agy"]);
        assert_eq!(
            agy.pick,
            Some(("Select login method:".into(), vec!["Enter".into()]))
        );
        assert_eq!(all.get("codex").unwrap().login_start, None);
        assert!(
            claude
                .login_check_command()
                .unwrap()
                .ends_with("export PATH; claude auth status --text")
        );
    }

    #[test]
    fn the_login_start_keys_are_checked() {
        let login = |rest: &str| {
            with(&format!(
                "binary = \"t\"\n[login]\ncheck = [\"t\", \"ok\"]\n{rest}"
            ))
        };
        let m = Manifest::parse(&login(
            "start = [\"t\", \"login\"]\ncode_after = \"Enter this code\"\n",
        ))
        .unwrap();
        let start = m.login_start.unwrap();
        assert_eq!(start.code_after.as_deref(), Some("Enter this code"));
        assert_eq!(start.pick, None);
        for (rest, want) in [
            ("start = []\n", "login.start: needs a command"),
            ("code_after = \"x\"\n", "need login.start"),
            (
                "start = [\"t\"]\npick = \"Pick:\"\n",
                "pick and keys go together",
            ),
            (
                "start = [\"t\"]\nkeys = [\"Enter\"]\n",
                "pick and keys go together",
            ),
            (
                "start = [\"t\"]\npick = \"Pick:\"\nkeys = [\"\"]\n",
                "a key name is empty",
            ),
            (
                "start = [\"t\"]\npaste = \" \"\n",
                "login.paste: needs one line",
            ),
        ] {
            let e = err(&login(rest));
            assert!(e.contains(want), "{rest}: {e}");
        }
    }

    #[test]
    fn the_version_command_adds_the_install_dirs_to_the_path() {
        let m = Harnesses::builtin().get("claude").unwrap();
        assert_eq!(
            m.version_command().unwrap(),
            "PATH=\"$HOME/.local/bin\":\"$HOME/.cargo/bin\":\"$PATH\"; command -v claude >/dev/null 2>&1 && claude --version || printf none"
        );
        // No version, no command: what pastor cannot read it does not ask.
        assert_eq!(
            Harnesses::builtin().get("codex").unwrap().version_command(),
            None
        );
        assert_eq!(
            Harnesses::builtin()
                .get("claude")
                .unwrap()
                .parse_version("2.1.310 (Claude Code)\n")
                .as_deref(),
            Some("2.1.310")
        );
    }

    #[test]
    fn mcp_placement_is_data_in_the_manifest() {
        let all = Harnesses::builtin();
        assert_eq!(all.get("claude").unwrap().mcp, Some(McpPlacement::Plugin));
        assert_eq!(
            all.get("agy").unwrap().mcp,
            Some(McpPlacement::File(
                "~/.gemini/config/mcp_config.json".into()
            ))
        );
        assert_eq!(all.get("codex").unwrap().mcp, None);
        assert_eq!(all.get("opencode").unwrap().mcp, None);

        let ok = |extra: &str| Manifest::parse(&with(extra)).unwrap().mcp;
        assert_eq!(
            Manifest::parse(
                "id = \"tool\"\n[mcp]\nmethod = \"plugin\"\n[paths]\nhome = \"~/.tool\"\nsettings = \"s.json\"\n"
            )
            .unwrap()
            .mcp,
            Some(McpPlacement::Plugin)
        );
        assert_eq!(
            ok("[mcp]\nmethod = \"file\"\nfile = \"~/.x/mcp.json\""),
            Some(McpPlacement::File("~/.x/mcp.json".into()))
        );
        assert_eq!(ok(""), None);

        for (extra, why) in [
            ("[mcp]\nmethod = \"file\"", "needs a file"),
            (
                "[mcp]\nmethod = \"plugin\"\nfile = \"~/.x/m.json\"",
                "names no file",
            ),
            (
                "[mcp]\nmethod = \"file\"\nfile = \"/etc/m.json\"",
                "must be a path starting with ~/",
            ),
            (
                "[mcp]\nmethod = \"file\"\nfile = \"~/../m.json\"",
                "must be a path starting with ~/",
            ),
            ("[mcp]\nmethod = \"carrier-pigeon\"", "method"),
            ("[mcp]\nmethod = \"plugin\"", "needs paths.settings"),
            (
                "[mcp]\nmethod = \"file\"\nfile = \"~/m.json\"\nextra = 1",
                "extra",
            ),
        ] {
            let e = err(&with(extra));
            assert!(e.contains(why), "{extra:?}: {e}");
        }
        // One file for the kind cannot go with homes that move.
        let e = err(
            "id = \"tool\"\n[mcp]\nmethod = \"file\"\nfile = \"~/.x/mcp.json\"\n[paths]\nhome_env = \"TOOL_HOME\"\nhome = \"~/.tool\"\n",
        );
        assert!(e.contains("cannot go with paths.home_env"), "{e}");
    }

    #[test]
    fn the_paths_are_the_homes_the_bundle_engine_places_into() {
        let all = Harnesses::builtin();
        let claude = &all.get("claude").unwrap().paths;
        assert_eq!(claude.home, "~/.claude");
        assert_eq!(claude.home_env.as_deref(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(claude.skills.as_deref(), Some("skills"));
        assert_eq!(claude.hooks.as_deref(), Some("hooks"));
        assert_eq!(claude.instructions.as_deref(), Some("CLAUDE.md"));
        assert_eq!(claude.settings.as_deref(), Some("settings.json"));
        assert_eq!(claude.mcp.as_deref(), Some(".claude.json"));
        assert_eq!(claude.mcp_default.as_deref(), Some("~/.claude.json"));
        let agy = &all.get("agy").unwrap().paths;
        assert_eq!(agy.home, "~/.gemini/antigravity-cli");
        assert_eq!(agy.home_env, None);
        assert_eq!(agy.settings.as_deref(), Some("settings.json"));
        assert_eq!((agy.skills.as_ref(), agy.hooks.as_ref()), (None, None));
        let codex = &all.get("codex").unwrap().paths;
        assert_eq!(codex.home_env.as_deref(), Some("CODEX_HOME"));
        assert_eq!(codex.instructions.as_deref(), Some("AGENTS.md"));
        let opencode = &all.get("opencode").unwrap().paths;
        assert_eq!(opencode.home, "~/.config/opencode");
        assert_eq!(opencode.skills, None);
    }

    /// Each built-in names its login files, in its home or with `~`; a path
    /// that leaves the home is refused.
    #[test]
    fn the_secret_files_are_in_the_home_or_under_tilde() {
        let all = Harnesses::builtin();
        let secrets = |kind: &str, home: &str| all.get(kind).unwrap().paths.secrets_in(home);
        assert_eq!(
            secrets("claude", "~/.claude-main"),
            ["~/.claude-main/.credentials.json"]
        );
        assert_eq!(secrets("codex", "~/.codex"), ["~/.codex/auth.json"]);
        assert_eq!(
            secrets("agy", "~/.gemini/antigravity-cli"),
            ["~/.gemini/antigravity-cli/antigravity-oauth-token"]
        );
        assert_eq!(
            secrets("opencode", "~/.oc"),
            ["~/.local/share/opencode/auth.json"]
        );
        let m = Manifest::parse(&format!(
            "{MIN}secrets = [\"keys/*.json\", \"~/.tool-auth\"]\n"
        ))
        .unwrap();
        assert_eq!(
            m.paths.secrets_in("/srv/tool/"),
            ["/srv/tool/keys/*.json", "~/.tool-auth"]
        );
        for bad in ["../auth.json", "/etc/auth", "~/../auth"] {
            let e = err(&format!("{MIN}secrets = [\"{bad}\"]\n"));
            assert!(e.contains("paths.secrets"), "{bad}: {e}");
        }
    }

    #[test]
    fn the_home_follows_the_agents_env() {
        let claude = &Harnesses::builtin().get("claude").unwrap().paths;
        assert_eq!(claude.home_for(&BTreeMap::new()), "~/.claude");
        let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".into(), "~/.claude-main".into())]);
        assert_eq!(claude.home_for(&env), "~/.claude-main");
        let empty = BTreeMap::from([("CLAUDE_CONFIG_DIR".into(), String::new())]);
        assert_eq!(claude.home_for(&empty), "~/.claude");
        let agy = &Harnesses::builtin().get("agy").unwrap().paths;
        assert_eq!(agy.home_for(&env), "~/.gemini/antigravity-cli");
    }

    #[test]
    fn a_full_manifest_parses_with_its_sources_in_order() {
        let m = Manifest::parse(&with(
            r#"
name = "Tool"
binary = "tool"
bin_dirs = ["~/.tool/bin"]
[version]
args = ["--version"]
[login]
check = ["tool", "whoami"]
[[install]]
source = "release"
url = "https://example.org/{version}/tool-{os}-{arch}"
sha256_url = "https://example.org/{version}/tool-{os}-{arch}.sha256"
[[install]]
source = "npm"
package = "@org/tool"
[[install]]
source = "script"
url = "https://example.org/install.sh"
shell = "bash"
args = ["--channel", "{version}"]
"#,
        ))
        .unwrap();
        assert_eq!(m.name, "Tool");
        assert_eq!(m.install.len(), 3);
        assert!(matches!(m.install[0], Install::Release { .. }));
        assert_eq!(
            m.install[1],
            Install::Npm {
                package: "@org/tool".into()
            }
        );
        assert_eq!(
            m.install[2],
            Install::Script {
                url: "https://example.org/install.sh".into(),
                shell: Shell::Bash,
                args: vec!["--channel".into(), "{version}".into()],
            }
        );
        assert_eq!(Manifest::parse(MIN).unwrap().name, "tool");
    }

    #[test]
    fn a_manifest_that_does_not_make_sense_says_why() {
        for (text, says) in [
            (
                "id = \"Tool\"\n[paths]\nhome = \"~/.t\"\n".to_string(),
                "id:",
            ),
            (with("surprise = 1\n"), "unknown field"),
            (with("binary = \"a b\"\n"), "binary:"),
            (
                with("[version]\nargs = [\"--version\"]\n"),
                "needs the harness's `binary`",
            ),
            (
                with("binary = \"t\"\n[version]\nargs = []\n"),
                "version.args",
            ),
            (
                with("binary = \"t\"\n[login]\ncheck = []\n"),
                "login.check: needs a command",
            ),
            (
                with("binary = \"t\"\nbin_dirs = [\"/usr/bin\"]\n"),
                "bin_dirs",
            ),
            (
                with("binary = \"t\"\nbin_dirs = [\"~/x\\\";echo INJECTED;\\\"\"]\n"),
                "plain path",
            ),
            (
                with("binary = \"t\"\nbin_dirs = [\"~/a b\"]\n"),
                "plain path",
            ),
            (
                with("binary = \"t\"\nbin_dirs = [\"~/$HOME\"]\n"),
                "plain path",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"release\"\nurl = \"http://x.org/t\"\nsha256_url = \"https://x.org/t.sha256\"\n",
                ),
                "not an https URL",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"release\"\nurl = \"https://x.org/{tag}\"\nsha256_url = \"https://x.org/t.sha256\"\n",
                ),
                "has {tag}",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"release\"\nurl = \"https://x.org/t\"\n",
                ),
                "sha256_url",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"script\"\nurl = \"https://x.org/{version}\"\n",
                ),
                "has {version}",
            ),
            (
                with("binary = \"t\"\n[[install]]\nsource = \"npm\"\npackage = \"a b\"\n"),
                "not an npm package",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"script\"\nurl = \"https://x.org/i.sh\"\nargs = [\"$(id)\"]\n",
                ),
                "not a plain argument",
            ),
            (
                with(
                    "binary = \"t\"\n[[install]]\nsource = \"script\"\nurl = \"https://x.org/i.sh\"\nargs = [\"{tag}\"]\n",
                ),
                "not a plain argument",
            ),
            (
                with("binary = \"t\"\n[update]\nenv = { lower = \"1\" }\n"),
                "update.env",
            ),
            (with("[update]\nenv = { A = \"1\" }\n"), "update: needs"),
            (
                with("binary = \"t\"\n[[install]]\nsource = \"apt\"\npackage = \"t\"\n"),
                "unknown variant",
            ),
            (
                "id = \"tool\"\n[paths]\nhome = \"/root/.t\"\n".to_string(),
                "paths.home",
            ),
            (
                with("").replace("[paths]", "[paths]\nskills = \"../x\""),
                "paths.skills",
            ),
            (
                with("").replace("[paths]", "[paths]\nhooks = \"/abs\""),
                "paths.hooks",
            ),
            (
                with("").replace("[paths]", "[paths]\nhome_env = \"lower\""),
                "home_env",
            ),
            (
                with("").replace("[paths]", "[paths]\nmcp_default = \"~/.t.json\""),
                "needs paths.mcp",
            ),
        ] {
            let got = err(&text);
            assert!(got.contains(says), "{says:?} not in {got:?}");
        }
    }

    #[test]
    fn a_file_in_the_config_dir_replaces_a_built_in_or_adds_a_harness() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        // No directory: just the built-ins.
        assert_eq!(&Harnesses::load(&paths).unwrap(), Harnesses::builtin());

        let root = paths.harnesses_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("claude.toml"),
            "id = \"claude\"\n[paths]\nhome = \"~/.claude-mine\"\nskills = \"s\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tool.toml"),
            "id = \"tool\"\nbinary = \"tool\"\n[version]\nargs = [\"-V\"]\n[paths]\nhome = \"~/.tool\"\n",
        )
        .unwrap();
        // Not read: a dotfile, another extension, a directory.
        std::fs::write(root.join(".hidden.toml"), "not toml at all").unwrap();
        std::fs::write(root.join("notes.txt"), "not toml at all").unwrap();
        std::fs::create_dir(root.join("dir.toml")).unwrap();

        let all = Harnesses::load(&paths).unwrap();
        let claude = all.get("claude").unwrap();
        assert_eq!(claude.paths.home, "~/.claude-mine");
        assert_eq!(claude.version, None, "a replacement is the whole manifest");
        assert_eq!(all.get("tool").unwrap().paths.home, "~/.tool");
        assert!(all.get("agy").is_some(), "the others stay");
        let versioned: Vec<_> = all.versioned().map(|m| m.id.as_str()).collect();
        assert_eq!(versioned, ["agy", "tool"]);
    }

    #[test]
    fn a_broken_file_in_the_config_dir_fails_the_load_with_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let root = paths.harnesses_dir();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("tool.toml"), "id = \"tool\"\n").unwrap();
        let e = Harnesses::load(&paths).unwrap_err().to_string();
        assert!(e.contains("tool.toml") && e.contains("paths"), "{e}");

        std::fs::write(root.join("tool.toml"), MIN).unwrap();
        std::fs::write(root.join("other.toml"), MIN).unwrap();
        let e = Harnesses::load(&paths).unwrap_err().to_string();
        assert!(e.contains("does not match the file name"), "{e}");
    }
}
