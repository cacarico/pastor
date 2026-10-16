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

impl Fetch for CurlFetch {
    fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
        // `--proto =https` also bars a redirect to anything else.
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--proto", "=https", "-o"])
            .arg(to)
            .arg(url)
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
    let dir = dest.parent().context("cache path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // Work beside the destination so the final rename stays on one
    // filesystem; a failed run leaves nothing but this dir, removed below.
    let work = dir.join(format!(".tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir(&work).with_context(|| format!("create {}", work.display()))?;
    let res = download(cfg, a, fetch, &work).await;
    let res = res.and_then(|(file, sum)| {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&file, &dest).with_context(|| format!("place {}", dest.display()))?;
        // The sum goes last: a crash before it leaves a file that does not
        // count as cached.
        std::fs::write(sha_file(&dest), format!("{sum}\n"))?;
        Ok(sum)
    });
    let _ = std::fs::remove_dir_all(&work);
    Ok((dest, res?))
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
            "assets": {"linux-aarch64": format!("file://{}", asset_path.display())},
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
                files: BTreeMap::new(),
                calls: Mutex::new(vec![]),
            },
        }
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
