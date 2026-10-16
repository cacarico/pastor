//! A checked cache of the binaries the head puts on a machine: pastor and
//! herdr. Nothing is trusted from the network: every download is hashed
//! against a published sum before it is renamed into the cache, and a cached
//! file is hashed again on each use.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::config::{Paths, ProvisionConfig};

/// The herdr version installed when neither `[provision] herdr` nor the fleet
/// says one.
pub const DEFAULT_HERDR_VERSION: &str = "0.9.5";

/// Where herdr publishes its release index.
pub const DEFAULT_HERDR_INDEX: &str = "https://herdr.dev/latest.json";

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
    paths
        .state_dir
        .join("artifacts")
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

/// The cached binary for `a`, fetched and checked first when it is not there.
/// Returns its path and sha256. A download that does not match its published
/// sum is never cached.
pub async fn ensure(
    paths: &Paths,
    cfg: &ProvisionConfig,
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
    let res = ensure_locked(cfg, a, fetch, &dest).await;
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
    cfg: &ProvisionConfig,
    a: &Artifact,
    fetch: &dyn Fetch,
    dest: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    // The request before this one may have just cached it.
    if let Some(sum) = cached_sum(dest) {
        return Ok((dest.to_path_buf(), sum));
    }
    let dir = dest.parent().context("cache path has no parent")?;
    // The dirs this call makes, deepest first, so a failure removes them
    // again: a made-up version leaves no `<version>/<target>` behind.
    let made: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|d| !d.exists())
        .map(Path::to_path_buf)
        .collect();
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let res = fill(cfg, a, fetch, dir, dest).await;
    if res.is_err() {
        // `remove_dir` only removes an empty dir, so one another artifact
        // has since put a file under stays.
        for d in &made {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
        }
    }
    Ok((dest.to_path_buf(), res?))
}

/// Downloads `a` beside `dest` in `dir`, checks it and places it.
async fn fill(
    cfg: &ProvisionConfig,
    a: &Artifact,
    fetch: &dyn Fetch,
    dir: &Path,
    dest: &Path,
) -> anyhow::Result<String> {
    // Work beside the destination so the final rename stays on one
    // filesystem; a failed run leaves nothing but this dir, removed below.
    // Each call gets its own, so two fetches of one artifact never share it.
    let work = dir.join(work_dir_name());
    std::fs::create_dir(&work).with_context(|| format!("create {}", work.display()))?;
    let res = download(cfg, a, fetch, &work).await;
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
    cfg: &ProvisionConfig,
    a: &Artifact,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    match a.name.as_str() {
        "pastor" => pastor_source(a, fetch, work).await,
        "herdr" => herdr_source(cfg, a, fetch, work),
        other => Err(ArtifactError::Invalid {
            what: "name",
            value: other.to_string(),
        }
        .into()),
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

async fn pastor_source(
    a: &Artifact,
    fetch: &dyn Fetch,
    work: &Path,
) -> anyhow::Result<(PathBuf, String)> {
    let out = work.join("pastor");
    // The head is already the version asked for, on the same target: no
    // download needed, and no release needs to exist for a dev build.
    if a.version == env!("CARGO_PKG_VERSION") && head_triple() == Some(a.target.as_str()) {
        let exe = std::env::current_exe().context("find the running pastor")?;
        std::fs::copy(&exe, &out).with_context(|| format!("copy {}", exe.display()))?;
        let sum = hex_sha256(&out)?;
        return Ok((out, sum));
    }
    let name = format!("pastor-{}-{}", a.version, a.target);
    let tarball = format!("{name}.tar.gz");
    let base = format!(
        "https://github.com/cacarico/pastor/releases/download/v{}",
        a.version
    );
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
    cfg: &ProvisionConfig,
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

/// The herdr version to install: the config value, else the version most
/// machines already report (ties go to the higher string), else the default.
pub fn herdr_version(cfg: &ProvisionConfig, fleet_versions: &[String]) -> String {
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
        cfg: ProvisionConfig,
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
        let cfg = ProvisionConfig {
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
        let base = format!("https://github.com/cacarico/pastor/releases/download/v{ver}");
        let fetch = MapFetch {
            files: BTreeMap::from([
                (format!("{base}/{name}.tar.gz"), tgz),
                (format!("{base}/SHA256SUMS"), sums),
            ]),
            calls: Mutex::new(vec![]),
        };
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let a = Artifact::new("pastor", ver, triple).unwrap();
        let (path, got) = ensure(&paths, &ProvisionConfig::default(), &a, &fetch)
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
        let base = "https://github.com/cacarico/pastor/releases/download/v9.9.9";
        let fetch = MapFetch {
            files: BTreeMap::from([
                (format!("{base}/pastor-9.9.9-x.tar.gz"), tgz),
                (format!("{base}/SHA256SUMS"), sums),
            ]),
            calls: Mutex::new(vec![]),
        };
        let paths = Paths::new(dir.path().join("cfg"), dir.path().join("state"));
        let a = Artifact::new("pastor", "9.9.9", "x").unwrap();
        let e = ensure(&paths, &ProvisionConfig::default(), &a, &fetch)
            .await
            .unwrap_err();
        assert_eq!(error_code(&e), Some("artifact_checksum_mismatch"));
    }

    #[test]
    fn herdr_version_falls_back() {
        let set = ProvisionConfig {
            herdr: Some("1.2.3".into()),
            herdr_index: None,
        };
        let fleet: Vec<String> = ["0.8.0", "0.9.0", "0.9.0"].map(String::from).to_vec();
        assert_eq!(herdr_version(&set, &fleet), "1.2.3");
        let unset = ProvisionConfig::default();
        assert_eq!(herdr_version(&unset, &fleet), "0.9.0");
        assert_eq!(herdr_version(&unset, &[]), DEFAULT_HERDR_VERSION);
    }
}
