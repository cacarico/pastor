//! A checked cache of the binaries the head puts on a machine: pastor,
//! herdr, and a harness's release file, under
//! `<state>/artifacts/<name>/<version>/<target>/`. Nothing is trusted from
//! the network: every download is hashed against a published sum before it
//! is renamed into the cache, and a cached file is hashed again on each use.
//!
//! `harness_install` takes harness release files and herdr from here;
//! pastor is not installed from it yet.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::cli::CliError;
use crate::config::{ArtifactsConfig, Paths};
use crate::provision::run::Facts;

/// The herdr version installed when neither `[artifacts] herdr` nor the
/// fleet says one.
pub const DEFAULT_HERDR_VERSION: &str = "0.9.5";

/// Where herdr publishes its release index.
pub const DEFAULT_HERDR_INDEX: &str = "https://herdr.dev/latest.json";

/// Where pastor's releases are: `<this>/v<version>/` holds each target's
/// tarball and `SHA256SUMS`, as `install.sh` reads them.
pub const PASTOR_RELEASES: &str = "https://github.com/cacarico/pastor/releases/download";

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact {what}: {value:?}")]
    Invalid { what: &'static str, value: String },
    #[error("checksum mismatch for {name} {version} {target}: expected {expected}, got {got}")]
    ChecksumMismatch {
        name: String,
        version: String,
        target: String,
        expected: String,
        got: String,
    },
}

impl ArtifactError {
    pub fn code(&self) -> &'static str {
        match self {
            ArtifactError::Invalid { .. } => "invalid_artifact",
            ArtifactError::ChecksumMismatch { .. } => "artifact_checksum_mismatch",
        }
    }
}

/// The code of an artifact error inside an `anyhow::Error`, if it is one.
pub fn error_code(e: &anyhow::Error) -> Option<&'static str> {
    e.downcast_ref::<ArtifactError>().map(ArtifactError::code)
}

/// What to cache. `target` is already the right string for the name: pastor's
/// triple (`aarch64-unknown-linux-musl`) or herdr's key (`linux-aarch64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub version: String,
    pub target: String,
}

/// Every part becomes a path component, so only a plain name may pass: no
/// separator, no `.` or `..` that would climb out of the cache.
fn valid_part(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Artifact {
    pub fn new(name: &str, version: &str, target: &str) -> Result<Artifact, ArtifactError> {
        for (what, value) in [("name", name), ("version", version), ("target", target)] {
            if !valid_part(value) {
                return Err(ArtifactError::Invalid {
                    what,
                    value: value.to_string(),
                });
            }
        }
        Ok(Artifact {
            name: name.into(),
            version: version.into(),
            target: target.into(),
        })
    }
}

/// Fetches one URL into a file. The real one shells out to curl; tests copy.
/// Blocking: a caller on an async runtime moves `ensure` off its workers.
pub trait Fetch: Sync {
    fn get(&self, url: &str, to: &Path) -> anyhow::Result<()>;
}

pub struct CurlFetch;

/// The longest one download may take, in seconds. A shepherd's fetch waits
/// on the head's download, so a server that trickles bytes must not hold it
/// (and the single-flight lock behind it) forever; a release binary over a
/// slow link is well inside ten minutes.
const CURL_MAX_TIME: &str = "600";
/// Seconds to wait for the connection itself: a host that never answers is
/// given up on early, not after `CURL_MAX_TIME`.
const CURL_CONNECT_TIMEOUT: &str = "20";
/// The largest file curl will write. pastor and herdr are a few tens of MB;
/// the cap keeps a wrong or hostile URL from filling the head's disk before
/// the sum check can refuse it.
const CURL_MAX_FILESIZE: &str = "512M";

/// curl's argv for one download. `--proto =https` also bars a redirect to
/// anything else. The URL goes last and behind `--url`, so one that starts
/// with `-` is never read as an option.
fn curl_args(url: &str, to: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-fsSL",
        "--proto",
        "=https",
        "--max-time",
        CURL_MAX_TIME,
        "--connect-timeout",
        CURL_CONNECT_TIMEOUT,
        "--max-filesize",
        CURL_MAX_FILESIZE,
        "-o",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(to.into());
    args.push("--url".into());
    args.push(url.into());
    args
}

impl Fetch for CurlFetch {
    fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
        let out = std::process::Command::new("curl")
            .args(curl_args(url, to))
            .output()
            .context("run curl")?;
        if !out.status.success() {
            anyhow::bail!(
                "download of {url} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

pub fn cache_path(paths: &Paths, a: &Artifact) -> PathBuf {
    cache_root(paths)
        .join(&a.name)
        .join(&a.version)
        .join(&a.target)
        .join(&a.name)
}

fn sha_file(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    dest.with_file_name(name)
}

fn hex_sha256(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The cached file's sum, when the file is there and still hashes to what its
/// `.sha256` recorded: a truncated or edited file is fetched again.
fn cached_sum(dest: &Path) -> Option<String> {
    let want = std::fs::read_to_string(sha_file(dest)).ok()?;
    let got = hex_sha256(dest).ok()?;
    (want.trim() == got).then_some(got)
}

fn mismatch(a: &Artifact, expected: &str, got: &str) -> anyhow::Error {
    ArtifactError::ChecksumMismatch {
        name: a.name.clone(),
        version: a.version.clone(),
        target: a.target.clone(),
        expected: expected.into(),
        got: got.into(),
    }
    .into()
}

/// Where an artifact comes from: pastor's and herdr's own release sources,
/// or a file and the URL of its sum, as a harness manifest names them.
#[derive(Clone, Copy)]
enum Source<'a> {
    Known(&'a ArtifactsConfig),
    Release { url: &'a str, sum_url: &'a str },
}

/// The directory the cache keeps everything under.
pub fn cache_root(paths: &Paths) -> PathBuf {
    paths.state_dir.join("artifacts")
}

/// What the cache already holds of `a`, with its sha256: nothing is fetched.
/// A file that no longer hashes to its recorded sum is not held.
pub fn cached(paths: &Paths, a: &Artifact) -> Option<(PathBuf, String)> {
    let dest = cache_path(paths, a);
    let sum = cached_sum(&dest)?;
    Some((dest, sum))
}

/// The cached binary for `a`, fetched and checked first when it is not there.
/// Returns its path and sha256. A download that does not match its published
/// sum is never cached.
pub async fn ensure(
    paths: &Paths,
    cfg: &ArtifactsConfig,
    a: &Artifact,
    fetch: &dyn Fetch,
) -> anyhow::Result<(PathBuf, String)> {
    ensure_from(paths, Source::Known(cfg), a, fetch).await
}

/// As `ensure`, for a file published at the https `url` with its sha256 at
/// `sum_url` (a harness manifest's release source, its placeholders filled).
pub async fn ensure_release(
    paths: &Paths,
    a: &Artifact,
    url: &str,
    sum_url: &str,
    fetch: &dyn Fetch,
) -> anyhow::Result<(PathBuf, String)> {
    ensure_from(paths, Source::Release { url, sum_url }, a, fetch).await
}

/// Drops `a`'s recorded sum, so the next `ensure` fetches it again: a
/// release under a moving name, such as a harness's `latest`, is new each
/// time it is asked for.
pub fn forget(paths: &Paths, a: &Artifact) {
    let _ = std::fs::remove_file(sha_file(&cache_path(paths, a)));
}

async fn ensure_from(
    paths: &Paths,
    source: Source<'_>,
    a: &Artifact,
    fetch: &dyn Fetch,
) -> anyhow::Result<(PathBuf, String)> {
    let dest = cache_path(paths, a);
    if let Some(sum) = cached_sum(&dest) {
        return Ok((dest, sum));
    }
    // One download per artifact at a time: a second request waits here for
    // the first, then finds the file cached instead of starting another curl.
    let lock = artifact_lock(&dest);
    let guard = lock.lock().await;
    let res = ensure_locked(source, a, fetch, &dest).await;
    drop(guard);
    release_artifact_lock(&dest, lock);
    res
}

type ArtifactLocks =
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>>;

/// The in-process locks of artifacts being fetched, by cache path.
fn artifact_locks() -> &'static ArtifactLocks {
    static LOCKS: std::sync::OnceLock<ArtifactLocks> = std::sync::OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

fn artifact_lock(dest: &Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut locks = artifact_locks().lock().unwrap_or_else(|e| e.into_inner());
    locks.entry(dest.to_path_buf()).or_default().clone()
}

/// Drops `dest`'s lock from the map once no one else holds or waits on it,
/// so made-up versions a shepherd asks for do not pile up entries.
fn release_artifact_lock(dest: &Path, lock: std::sync::Arc<tokio::sync::Mutex<()>>) {
    let mut locks = artifact_locks().lock().unwrap_or_else(|e| e.into_inner());
    drop(lock);
    if locks
        .get(dest)
        .is_some_and(|l| std::sync::Arc::strong_count(l) == 1)
    {
        locks.remove(dest);
    }
}

/// `ensure` under the artifact's lock.
async fn ensure_locked(
    source: Source<'_>,
    a: &Artifact,
    fetch: &dyn Fetch,
    dest: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    // The request before this one may have just cached it.
    if let Some(sum) = cached_sum(dest) {
        return Ok((dest.to_path_buf(), sum));
    }
    let dir = dest.parent().context("cache path has no parent")?;
    let made = create_cache_dir(dir)?;
    let res = fill(source, a, fetch, dir, dest).await;
    if res.is_err() {
        remove_made(&made);
    }
    Ok((dest.to_path_buf(), res?))
}

/// Make `dir` and the parents it lacks. Returns the dirs this call made,
/// deepest first, so a failure removes them again: a made-up version leaves
/// no `<version>/<target>` behind. A `create_dir_all` that fails part way
/// removes what it made before it returns.
///
/// Another artifact of the same version shares `<name>/<version>`, and its
/// failed fill may remove that dir between this call making it and making
/// what goes under it, which `create_dir_all` reads as `NotFound`: it is
/// tried again.
fn create_cache_dir(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let made: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|d| !d.exists())
        .map(Path::to_path_buf)
        .collect();
    let mut tries = 0;
    loop {
        match std::fs::create_dir_all(dir) {
            Ok(()) => return Ok(made),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && tries < 3 => tries += 1,
            Err(e) => {
                remove_made(&made);
                return Err(anyhow::Error::from(e).context(format!("create {}", dir.display())));
            }
        }
    }
}

/// Remove the dirs `create_cache_dir` made, deepest first. `remove_dir`
/// only removes an empty dir, so one another artifact has since put a
/// file under stays (`DirectoryNotEmpty`), and the ones above it with it.
/// One already gone (that other artifact's own cleanup got there first, or
/// one `create_dir_all` never got to) is passed over, so each failure is
/// passed over and the next one up is tried.
fn remove_made(made: &[PathBuf]) {
    for d in made {
        let _ = std::fs::remove_dir(d);
    }
}

/// Downloads `a` beside `dest` in `dir`, checks it and places it.
async fn fill(
    source: Source<'_>,
    a: &Artifact,
    fetch: &dyn Fetch,
    dir: &Path,
    dest: &Path,
) -> anyhow::Result<String> {
    // Work beside the destination so the final rename stays on one
    // filesystem; a failed run leaves nothing but this dir, removed below.
    // Each call gets its own, so two fetches of one artifact never share it.
    let work = dir.join(work_dir_name());
    // Another artifact's failed fill may have removed `dir` while it was
    // still empty: make it again.
    let made = std::fs::create_dir(&work).or_else(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).and_then(|()| std::fs::create_dir(&work))
        }
        _ => Err(e),
    });
    made.with_context(|| format!("create {}", work.display()))?;
    let res = download(source, a, fetch, &work).await;
    let res = res.and_then(|(file, sum)| {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&file, dest).with_context(|| format!("place {}", dest.display()))?;
        // The sum goes last: a crash before it leaves a file that does not
        // count as cached.
        std::fs::write(sha_file(dest), format!("{sum}\n"))?;
        Ok(sum)
    });
    let _ = std::fs::remove_dir_all(&work);
    res
}

/// A work dir name no other `ensure` uses at the same time: pid, a counter
/// for calls in this process, and the clock for a pid reused after a crash.
pub(crate) fn work_dir_name() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".tmp-{}-{n}-{nanos}", std::process::id())
}

/// Fetch and check `a` into `work`; the verified file and its sum.
async fn download(
    source: Source<'_>,
    a: &Artifact,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    match (source, a.name.as_str()) {
        (Source::Release { url, sum_url }, _) => release_source(a, url, sum_url, fetch, work),
        (Source::Known(_), "pastor") => pastor_source(a, fetch, work).await,
        (Source::Known(cfg), "herdr") => herdr_source(cfg, a, fetch, work),
        (Source::Known(_), other) => Err(ArtifactError::Invalid {
            what: "name",
            value: other.to_string(),
        }
        .into()),
    }
}

/// A file at `url` whose sha256 is the first word of the file at
/// `sum_url` (a bare sum, or `sha256sum`'s `<hex>  <file>`).
fn release_source(
    a: &Artifact,
    url: &str,
    sum_url: &str,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    for u in [url, sum_url] {
        if !u.starts_with("https://") {
            return Err(ArtifactError::Invalid {
                what: "url",
                value: u.to_string(),
            }
            .into());
        }
    }
    let sum_file = work.join("sum");
    fetch.get(sum_url, &sum_file)?;
    let text = std::fs::read_to_string(&sum_file)?;
    let expected = text
        .split_whitespace()
        .next()
        .map(|w| w.trim_start_matches('*').to_ascii_lowercase())
        .filter(|w| w.len() == 64 && w.bytes().all(|b| b.is_ascii_hexdigit()));
    let out = work.join(&a.name);
    fetch.get(url, &out)?;
    let got = hex_sha256(&out)?;
    match expected {
        Some(e) if e == got => Ok((out, got)),
        Some(e) => Err(mismatch(a, &e, &got)),
        None => Err(mismatch(a, &format!("(no sha256 at {sum_url})"), &got)),
    }
}

/// The target triple this binary was built for, when it is one pastor
/// publishes a tarball for and so one `current_exe()` can stand in for.
/// Linux counts only when static: a glibc build is not the musl release.
fn head_triple() -> Option<&'static str> {
    match (
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(target_env = "musl"),
    ) {
        ("linux", "x86_64", true) => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64", true) => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64", _) => Some("aarch64-apple-darwin"),
        ("macos", "x86_64", _) => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// When the head is `a` itself, its version built for `a`'s target (`head`,
/// from `head_triple`), copy the head's binary `exe` to `out` and give its
/// sum: no download, and no release needs to exist for a build from source.
/// A head built from source counts as that version: the version is all the
/// binary says of itself.
fn from_head(
    a: &Artifact,
    head: Option<&str>,
    exe: Option<&Path>,
    out: &Path,
) -> anyhow::Result<Option<String>> {
    if a.version != env!("CARGO_PKG_VERSION") || head != Some(a.target.as_str()) {
        return Ok(None);
    }
    let exe = exe.context("find the running pastor")?;
    std::fs::copy(exe, out).with_context(|| format!("copy {}", exe.display()))?;
    Ok(Some(hex_sha256(out)?))
}

async fn pastor_source(
    a: &Artifact,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    let out = work.join("pastor");
    let exe = std::env::current_exe();
    if let Some(sum) = from_head(a, head_triple(), exe.as_deref().ok(), &out)? {
        return Ok((out, sum));
    }
    let name = format!("pastor-{}-{}", a.version, a.target);
    let tarball = format!("{name}.tar.gz");
    let base = format!("{PASTOR_RELEASES}/v{}", a.version);
    let tgz = work.join(&tarball);
    let sums = work.join("SHA256SUMS");
    fetch.get(&format!("{base}/{tarball}"), &tgz)?;
    fetch.get(&format!("{base}/SHA256SUMS"), &sums)?;
    let sums = std::fs::read_to_string(&sums)?;
    // `<hex>  <file>` per line, as `sha256sum` writes it; `*` marks binary mode.
    let expected = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, f)| f.trim().trim_start_matches('*') == tarball)
        .map(|(h, _)| h.trim().to_ascii_lowercase());
    let got = hex_sha256(&tgz)?;
    match expected {
        Some(e) if e == got => {}
        Some(e) => return Err(mismatch(a, &e, &got)),
        None => return Err(mismatch(a, "(no entry in SHA256SUMS)", &got)),
    }
    let unpack = work.join("unpack");
    std::fs::create_dir(&unpack)?;
    let st = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&unpack)
        .output()
        .await
        .context("run tar")?;
    if !st.status.success() {
        anyhow::bail!("tar failed: {}", String::from_utf8_lossy(&st.stderr).trim());
    }
    std::fs::rename(unpack.join(&name).join("pastor"), &out)
        .with_context(|| format!("{tarball} has no {name}/pastor"))?;
    let sum = hex_sha256(&out)?;
    Ok((out, sum))
}

fn herdr_source(
    cfg: &ArtifactsConfig,
    a: &Artifact,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    let index_url = cfg.herdr_index.as_deref().unwrap_or(DEFAULT_HERDR_INDEX);
    let index_file = work.join("index.json");
    fetch.get(index_url, &index_file)?;
    let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&index_file)?)
        .with_context(|| format!("{index_url} is not JSON"))?;
    let release = &index["releases"][a.version.as_str()];
    let url = release["assets"][a.target.as_str()]
        .as_str()
        .with_context(|| format!("herdr {} has no {} asset", a.version, a.target))?;
    // The URL comes from the network: only https, which also keeps one that
    // starts with `-` away from curl.
    if !url.starts_with("https://") {
        return Err(ArtifactError::Invalid {
            what: "asset url",
            value: url.to_string(),
        }
        .into());
    }
    let expected = release["sha256"][a.target.as_str()]
        .as_str()
        .with_context(|| format!("herdr {} has no {} sha256", a.version, a.target))?
        .to_ascii_lowercase();
    // Assets are static binaries, not archives.
    let out = work.join("herdr");
    fetch.get(url, &out)?;
    let got = hex_sha256(&out)?;
    if got != expected {
        return Err(mismatch(a, &expected, &got));
    }
    Ok((out, got))
}

/// The target `name` is downloaded as for a machine with these `Facts`:
/// pastor's release triple, by `install.sh`'s rules, or herdr's own key. A
/// platform it ships nothing for is `unsupported_platform`.
pub fn target_for(facts: &Facts, name: &str) -> anyhow::Result<String> {
    let bits64 = facts.bits == 64;
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
            "unsupported_platform",
            format!(
                "{name} ships no build for {} {}{userland}",
                facts.os, facts.arch
            ),
        ));
    };
    Ok(target.to_string())
}

/// The herdr version to install: the config value, else the version most
/// machines already report (ties go to the higher string), else the default.
pub fn herdr_version(cfg: &ArtifactsConfig, fleet_versions: &[String]) -> String {
    if let Some(v) = cfg.herdr.as_deref().filter(|v| !v.trim().is_empty()) {
        return v.trim().to_string();
    }
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for v in fleet_versions.iter().filter(|v| !v.is_empty()) {
        *counts.entry(v).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(v, n)| (*n, *v))
        .map_or_else(|| DEFAULT_HERDR_VERSION.to_string(), |(v, _)| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Serves URLs from a map to local files and counts the calls.
    struct MapFetch {
        files: BTreeMap<String, PathBuf>,
        calls: Mutex<Vec<String>>,
    }

    impl Fetch for MapFetch {
        fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(url.to_string());
            let from = match self.files.get(url) {
                Some(p) => p.clone(),
                None => PathBuf::from(url.strip_prefix("file://").unwrap_or(url)),
            };
            std::fs::copy(from, to)?;
            Ok(())
        }
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    struct Rig {
        _dir: tempfile::TempDir,
        paths: Paths,
        cfg: ArtifactsConfig,
        fetch: MapFetch,
    }

    /// A herdr index and asset under a tempdir, reached by `file://`.
    fn herdr_rig(asset: &[u8], sum: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let asset_path = src.join("herdr-bin");
        std::fs::write(&asset_path, asset).unwrap();
        let index = serde_json::json!({"releases": {"0.9.5": {
            "assets": {"linux-aarch64": HERDR_ASSET_URL},
            "sha256": {"linux-aarch64": sum},
        }}});
        let index_path = src.join("latest.json");
        std::fs::write(&index_path, index.to_string()).unwrap();
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let cfg = ArtifactsConfig {
            herdr: None,
            herdr_index: Some(format!("file://{}", index_path.display())),
        };
        Rig {
            _dir: dir,
            paths,
            cfg,
            fetch: MapFetch {
                files: BTreeMap::from([(HERDR_ASSET_URL.to_string(), asset_path)]),
                calls: Mutex::new(vec![]),
            },
        }
    }

    const HERDR_ASSET_URL: &str = "https://herdr.example/herdr-linux-aarch64";

    /// Writes an index whose asset URL is `url` over the rig's own.
    fn set_asset_url(rig: &Rig, url: &str) {
        let path = rig.cfg.herdr_index.as_deref().unwrap();
        let path = path.strip_prefix("file://").unwrap();
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        index["releases"]["0.9.5"]["assets"]["linux-aarch64"] = url.into();
        std::fs::write(path, index.to_string()).unwrap();
    }

    /// A fetch that takes its time, so two calls overlap.
    struct SlowFetch(MapFetch);

    impl Fetch for SlowFetch {
        fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            self.0.get(url, to)
        }
    }

    #[test]
    fn concurrent_ensure_same_artifact() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let fetch = SlowFetch(rig.fetch);
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results: Vec<_> = std::thread::scope(|s| {
            let run = || {
                barrier.wait();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(ensure(&rig.paths, &rig.cfg, &a, &fetch))
            };
            let h = [s.spawn(run), s.spawn(run)];
            h.map(|h| h.join().unwrap()).into_iter().collect()
        });
        for r in results {
            let (_, sum) = r.unwrap();
            assert_eq!(sum, sha(b"herdr-binary"));
        }
        let dest = cache_path(&rig.paths, &a);
        assert_eq!(std::fs::read(&dest).unwrap(), b"herdr-binary");
        assert_eq!(cached_sum(&dest), Some(sha(b"herdr-binary")));
        let mut left: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["herdr", "herdr.sha256"], "no work dir left");
    }

    /// Two requests for one artifact at once download it once: the second
    /// waits for the first and finds it cached.
    #[test]
    fn concurrent_ensure_downloads_once() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let fetch = SlowFetch(rig.fetch);
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            let run = || {
                barrier.wait();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(ensure(&rig.paths, &rig.cfg, &a, &fetch))
                    .unwrap()
            };
            let h = [s.spawn(run), s.spawn(run)];
            for h in h {
                h.join().unwrap();
            }
        });
        // One run fetches the index and the asset.
        let calls = fetch.0.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
    }

    /// A version the index does not know leaves no `<ver>/<target>` dir
    /// behind, nor the `<name>` one when `ensure` made it.
    #[tokio::test]
    async fn a_made_up_version_leaves_no_dir() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let a = Artifact::new("herdr", "7.7.7", "linux-aarch64").unwrap();
        ensure(&rig.paths, &rig.cfg, &a, &rig.fetch)
            .await
            .unwrap_err();
        let cache = rig.paths.state_dir.join("artifacts");
        assert!(!cache.join("herdr/7.7.7").exists());
        assert!(!cache.join("herdr").exists());

        // A dir that was already there, or holds another artifact, stays.
        let ok = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        ensure(&rig.paths, &rig.cfg, &ok, &rig.fetch).await.unwrap();
        ensure(&rig.paths, &rig.cfg, &a, &rig.fetch)
            .await
            .unwrap_err();
        assert!(!cache.join("herdr/7.7.7").exists());
        assert!(cache_path(&rig.paths, &ok).exists());
    }

    /// A `create_dir_all` that fails part way (a name too long for the
    /// filesystem, here) leaves none of the dirs it made.
    #[test]
    fn a_cache_dir_made_part_way_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "x".repeat(300);
        let dir = tmp.path().join("herdr").join(&long).join("linux-x86_64");
        assert!(create_cache_dir(&dir).is_err());
        assert!(!tmp.path().join("herdr").exists());
    }

    /// Two artifacts of one version clean up after each other: a dir the
    /// other already removed is passed over, and one it still uses stays,
    /// with everything above it.
    #[test]
    fn cleanup_tolerates_another_artifact_of_the_version() {
        let tmp = tempfile::tempdir().unwrap();
        let version = tmp.path().join("herdr/0.9.5");
        let mine = version.join("linux-aarch64");
        let made = create_cache_dir(&mine).unwrap();
        assert_eq!(made.len(), 3, "{made:?}");
        // The other's cleanup removed the shared dirs first.
        std::fs::remove_dir(&mine).unwrap();
        std::fs::remove_dir(&version).unwrap();
        remove_made(&made);
        assert!(!tmp.path().join("herdr").exists());

        // The other's target is still there: the version dir stays.
        let made = create_cache_dir(&mine).unwrap();
        std::fs::create_dir(version.join("linux-x86_64")).unwrap();
        remove_made(&made);
        assert!(!mine.exists());
        assert!(version.join("linux-x86_64").exists());
    }

    #[test]
    fn curl_args_bound_time_and_size() {
        let args = curl_args("https://x", Path::new("/w/out"));
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        for pair in [
            ["--max-time", "600"],
            ["--connect-timeout", "20"],
            ["--max-filesize", "512M"],
        ] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?} in {args:?}");
        }
        assert_eq!(&args[args.len() - 2..], ["--url", "https://x"]);
    }

    #[tokio::test]
    async fn herdr_rejects_non_https_asset() {
        for url in ["-o/tmp/x", "http://herdr.example/herdr", "file:///bin/sh"] {
            let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
            set_asset_url(&rig, url);
            let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
            let e = ensure(&rig.paths, &rig.cfg, &a, &rig.fetch)
                .await
                .unwrap_err();
            assert_eq!(error_code(&e), Some("invalid_artifact"), "{url}");
            assert!(
                !rig.fetch.calls.lock().unwrap().iter().any(|c| c == url),
                "{url} never fetched"
            );
            assert!(!cache_path(&rig.paths, &a).exists());
        }
    }

    #[test]
    fn curl_args_put_url_last_with_flag() {
        let args = curl_args("-K/etc/passwd", Path::new("/w/out"));
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-fsSL",
                "--proto",
                "=https",
                "--max-time",
                "600",
                "--connect-timeout",
                "20",
                "--max-filesize",
                "512M",
                "-o",
                "/w/out",
                "--url",
                "-K/etc/passwd"
            ]
        );
    }

    #[test]
    fn rejects_traversal() {
        for bad in ["..", ".", "a/b", "", "a b", "../x"] {
            assert!(Artifact::new(bad, "1", "t").is_err(), "{bad}");
            assert!(Artifact::new("n", bad, "t").is_err(), "{bad}");
            let e = Artifact::new("n", "1", bad).unwrap_err();
            assert_eq!(e.code(), "invalid_artifact");
        }
        assert!(Artifact::new("herdr", "0.9.5", "linux-aarch64").is_ok());
    }

    #[test]
    fn cache_path_layout() {
        let paths = Paths::new("/c", "/s");
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        assert_eq!(
            cache_path(&paths, &a),
            PathBuf::from("/s/artifacts/herdr/0.9.5/linux-aarch64/herdr")
        );
    }

    #[tokio::test]
    async fn herdr_from_index() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        let (path, got) = ensure(&rig.paths, &rig.cfg, &a, &rig.fetch).await.unwrap();
        assert_eq!(path, cache_path(&rig.paths, &a));
        assert_eq!(got, sha(b"herdr-binary"));
        assert_eq!(std::fs::read(&path).unwrap(), b"herdr-binary");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn checksum_mismatch_refused() {
        let rig = herdr_rig(b"tampered", &sha(b"the real one"));
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        let e = ensure(&rig.paths, &rig.cfg, &a, &rig.fetch)
            .await
            .unwrap_err();
        assert_eq!(error_code(&e), Some("artifact_checksum_mismatch"));
        assert!(!cache_path(&rig.paths, &a).exists(), "nothing cached");
    }

    /// A release a harness manifest names: cached when its file hashes to
    /// the sum published beside it (bare, or as `sha256sum` writes it), and
    /// refused and never cached when it does not.
    #[tokio::test]
    async fn a_release_is_checked_against_its_published_sum() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let put = |name: &str, bytes: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let (url, sum_url) = ("https://x.example/tool", "https://x.example/tool.sha256");
        let fetch = |sum: &str| MapFetch {
            files: BTreeMap::from([
                (url.to_string(), put("tool", b"tool-binary")),
                (sum_url.to_string(), put("tool.sha256", sum.as_bytes())),
            ]),
            calls: Mutex::new(vec![]),
        };
        let a = Artifact::new("harness-tool", "1.2.3", "linux-x86_64").unwrap();
        let bad = fetch(&format!("{}  tool\n", sha(b"another binary")));
        let e = ensure_release(&paths, &a, url, sum_url, &bad)
            .await
            .unwrap_err();
        assert_eq!(error_code(&e), Some("artifact_checksum_mismatch"));
        assert!(!cache_path(&paths, &a).exists(), "nothing cached");

        let good = fetch(&format!("{}\n", sha(b"tool-binary")));
        let (path, sum) = ensure_release(&paths, &a, url, sum_url, &good)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"tool-binary");
        assert_eq!(sum, sha(b"tool-binary"));
    }

    #[tokio::test]
    async fn cached_not_downloaded_twice() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        ensure(&rig.paths, &rig.cfg, &a, &rig.fetch).await.unwrap();
        let n = rig.fetch.calls.lock().unwrap().len();
        assert!(n > 0);
        ensure(&rig.paths, &rig.cfg, &a, &rig.fetch).await.unwrap();
        assert_eq!(
            rig.fetch.calls.lock().unwrap().len(),
            n,
            "second call hit cache"
        );
    }

    #[tokio::test]
    async fn corrupt_cache_is_fetched_again() {
        let rig = herdr_rig(b"herdr-binary", &sha(b"herdr-binary"));
        let a = Artifact::new("herdr", "0.9.5", "linux-aarch64").unwrap();
        let (path, _) = ensure(&rig.paths, &rig.cfg, &a, &rig.fetch).await.unwrap();
        std::fs::write(&path, b"bit rot").unwrap();
        ensure(&rig.paths, &rig.cfg, &a, &rig.fetch).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"herdr-binary");
    }

    #[tokio::test]
    async fn pastor_from_release_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let (ver, triple) = ("9.9.9", "aarch64-unknown-linux-musl");
        let name = format!("pastor-{ver}-{triple}");
        let stage = dir.path().join("stage");
        std::fs::create_dir_all(stage.join(&name)).unwrap();
        std::fs::write(stage.join(&name).join("pastor"), b"pastor-binary").unwrap();
        let tgz = dir.path().join(format!("{name}.tar.gz"));
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&stage)
            .arg(&name)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let sums = dir.path().join("SHA256SUMS");
        let line = format!(
            "{}  {name}.tar.gz\n{}  other.tar.gz\n",
            sha(&std::fs::read(&tgz).unwrap()),
            sha(b"x")
        );
        std::fs::write(&sums, line).unwrap();
        let base = format!("{PASTOR_RELEASES}/v{ver}");
        let fetch = MapFetch {
            files: BTreeMap::from([
                (format!("{base}/{name}.tar.gz"), tgz),
                (format!("{base}/SHA256SUMS"), sums),
            ]),
            calls: Mutex::new(vec![]),
        };
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let a = Artifact::new("pastor", ver, triple).unwrap();
        let (path, got) = ensure(&paths, &ArtifactsConfig::default(), &a, &fetch)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"pastor-binary");
        assert_eq!(got, sha(b"pastor-binary"));
    }

    #[tokio::test]
    async fn pastor_sums_missing_entry_is_a_mismatch_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("t.tar.gz");
        std::fs::write(&tgz, b"not a tarball").unwrap();
        let sums = dir.path().join("SHA256SUMS");
        std::fs::write(&sums, format!("{}  pastor-9.9.9-x.tar.gz\n", sha(b"other"))).unwrap();
        let base = format!("{PASTOR_RELEASES}/v9.9.9");
        let fetch = MapFetch {
            files: BTreeMap::from([
                (format!("{base}/pastor-9.9.9-x.tar.gz"), tgz),
                (format!("{base}/SHA256SUMS"), sums),
            ]),
            calls: Mutex::new(vec![]),
        };
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let a = Artifact::new("pastor", "9.9.9", "x").unwrap();
        let e = ensure(&paths, &ArtifactsConfig::default(), &a, &fetch)
            .await
            .unwrap_err();
        assert_eq!(error_code(&e), Some("artifact_checksum_mismatch"));
    }

    #[test]
    fn herdr_version_falls_back() {
        let set = ArtifactsConfig {
            herdr: Some("1.2.3".into()),
            herdr_index: None,
        };
        let fleet: Vec<String> = ["0.8.0", "0.9.0", "0.9.0"].map(String::from).to_vec();
        assert_eq!(herdr_version(&set, &fleet), "1.2.3");
        let unset = ArtifactsConfig::default();
        assert_eq!(herdr_version(&unset, &fleet), "0.9.0");
        // A tie goes to the higher, whichever order the machines come in.
        for tie in [["0.8.0", "0.9.0"], ["0.9.0", "0.8.0"]] {
            let fleet: Vec<String> = tie.map(String::from).to_vec();
            assert_eq!(herdr_version(&unset, &fleet), "0.9.0");
        }
        assert_eq!(herdr_version(&unset, &[]), DEFAULT_HERDR_VERSION);
    }

    fn linux(arch: &str, bits: u8) -> Facts {
        Facts {
            os: "linux".into(),
            arch: arch.into(),
            bits,
            distro: None,
            like: vec![],
        }
    }

    /// The head's own binary stands in for the release only when it is the
    /// version and target asked for.
    #[test]
    fn the_heads_own_binary_stands_in_for_its_release() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("pastor-here");
        std::fs::write(&exe, b"a pastor build").unwrap();
        let out = tmp.path().join("pastor");
        let version = env!("CARGO_PKG_VERSION");
        let a = Artifact::new("pastor", version, "x86_64-apple-darwin").unwrap();
        let head = Some("x86_64-apple-darwin");
        assert_eq!(
            from_head(&a, head, Some(&exe), &out).unwrap(),
            Some(sha(b"a pastor build"))
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"a pastor build");
        std::fs::remove_file(&out).unwrap();

        let older = Artifact::new("pastor", "0.0.1", "x86_64-apple-darwin").unwrap();
        let other = Artifact::new("pastor", version, "aarch64-apple-darwin").unwrap();
        for (a, head) in [(&older, head), (&other, head), (&a, None)] {
            assert_eq!(from_head(a, head, Some(&exe), &out).unwrap(), None);
        }
        assert!(!out.exists());
        let err = from_head(&a, head, None, &out).unwrap_err();
        assert!(err.to_string().contains("find the running pastor"), "{err}");
    }

    fn code(e: &anyhow::Error) -> String {
        e.downcast_ref::<CliError>()
            .map(|c| c.code.clone())
            .unwrap_or_default()
    }

    #[test]
    fn target_mapping() {
        let x86 = linux("x86_64", 64);
        assert_eq!(
            target_for(&x86, "pastor").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(target_for(&x86, "herdr").unwrap(), "linux-x86_64");
        let arm = linux("aarch64", 64);
        assert_eq!(
            target_for(&arm, "pastor").unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(target_for(&arm, "herdr").unwrap(), "linux-aarch64");
        let mac = Facts::read("Darwin\narm64\n64\n").unwrap();
        assert_eq!(target_for(&mac, "pastor").unwrap(), "aarch64-apple-darwin");
        assert_eq!(target_for(&mac, "herdr").unwrap(), "macos-aarch64");
        // A Pi with a 32-bit userland on a 64-bit kernel takes the armv7 build.
        let pi32 = Facts::read("Linux\naarch64\n32\n").unwrap();
        assert_eq!(
            target_for(&pi32, "pastor").unwrap(),
            "armv7-unknown-linux-musleabihf"
        );
        let armv7 = linux("armv7l", 32);
        assert_eq!(
            target_for(&armv7, "pastor").unwrap(),
            "armv7-unknown-linux-musleabihf"
        );
        assert_eq!(
            code(&target_for(&armv7, "herdr").unwrap_err()),
            "unsupported_platform"
        );
        let riscv = linux("riscv64", 64);
        assert_eq!(
            target_for(&riscv, "pastor").unwrap(),
            "riscv64gc-unknown-linux-musl"
        );
        let err = target_for(&riscv, "herdr").unwrap_err();
        assert_eq!(code(&err), "unsupported_platform");
        assert!(
            err.to_string()
                .contains("herdr ships no build for linux riscv64"),
            "{err}"
        );
        assert_eq!(
            code(&target_for(&x86, "tmux").unwrap_err()),
            "unsupported_platform"
        );
    }
}
