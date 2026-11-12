//! Self-update from GitHub Releases. Only a build installed at its canonical location replaces
//! itself (macOS: `/Applications/MyMail.app`, Linux: `~/.local/bin/mymail`); `cargo run` never does.
//!
//! Trust anchor: HTTPS to GitHub plus the asset's SHA-256 `digest` from the releases API. The digest
//! catches truncated or corrupt downloads; whoever controls the release also controls the digest.
use crate::gmail;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;
type Ver = (u64, u64, u64);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "tonibergholm/mymail";
const MAX_DOWNLOAD: u64 = 200 << 20;
const STAGE_PREFIX: &str = ".mymail-update-";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET: Option<&str> = Some("MyMail-macos-arm64.zip");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<&str> = Some("mymail-linux-x86_64.tar.gz");
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
const ASSET: Option<&str> = None;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        let s = |n| Some(Duration::from_secs(n));
        ureq::Agent::config_builder()
            .https_only(!cfg!(feature = "update-test"))
            .timeout_connect(s(10))
            .timeout_per_read(s(30))
            .timeout_per_call(s(300))
            .build()
            .into()
    })
}

// ---------- versions ----------

fn num(p: &str) -> Option<u64> {
    p.bytes().all(|b| b.is_ascii_digit()).then_some(())?;
    p.parse().ok()
}

/// `v1.2.3` or `1.2.3`; anything else (prerelease, extra parts, signs) is rejected.
fn parse_version(s: &str) -> Option<Ver> {
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.');
    let v = (num(it.next()?)?, num(it.next()?)?, num(it.next()?)?);
    it.next().is_none().then_some(v)
}

/// The version of the build installed at `target`, which may be newer than the running one
/// (installed, waiting for a restart). Asking the binary beats a state file that can go stale.
fn installed(target: &Path) -> Option<Ver> {
    let out = Command::new(exe_in(target))
        .arg("--version")
        .output()
        .ok()?;
    parse_version(
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .strip_prefix("mymail ")?,
    )
}

fn newest_known(target: &Path) -> Ver {
    let running = parse_version(VERSION).unwrap_or_default();
    running.max(installed(target).unwrap_or_default())
}

// ---------- release lookup ----------

struct Candidate {
    version: String,
    ver: Ver,
    url: String,
    sha256: String,
}

/// `None` when the release has no asset for this platform.
fn candidate(release: &Value, asset: &str) -> Result<Option<Candidate>> {
    let tag = release["tag_name"].as_str().ok_or("no tag_name")?;
    let ver = parse_version(tag).ok_or_else(|| format!("unsupported release tag {tag}"))?;
    let Some(a) = release["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"] == asset))
    else {
        return Ok(None);
    };
    let sha256 = a["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("asset has no sha256 digest")?
        .to_lowercase();
    let url = a["browser_download_url"]
        .as_str()
        .ok_or("no download url")?;
    Ok(Some(Candidate {
        version: format!("{}.{}.{}", ver.0, ver.1, ver.2),
        ver,
        url: url.into(),
        sha256,
    }))
}

fn api_url() -> String {
    #[cfg(feature = "update-test")]
    if let Ok(u) = std::env::var("MYMAIL_UPDATE_API") {
        return u;
    }
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

fn latest(target: &Path) -> Result<Option<Candidate>> {
    let Some(asset) = ASSET else { return Ok(None) };
    let release: Value = agent()
        .get(api_url())
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(err)?
        .body_mut()
        .read_json()
        .map_err(err)?;
    Ok(candidate(&release, asset)?.filter(|c| c.ver > newest_known(target)))
}

// ---------- where we may install ----------

/// macOS: the `.app` bundle. Linux: the executable. `None` for any other (development) build.
/// Fixed at first use (`init`, at startup): once an update replaces the file, Linux reports the
/// running executable as "... (deleted)", so the path can't be worked out again later.
fn target() -> Option<PathBuf> {
    static T: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let home = dirs::home_dir()?.canonicalize().ok()?;
        installed_path(std::env::current_exe().ok()?, home)
    })
    .clone()
}

pub fn init() {
    let _ = target();
}

#[cfg(target_os = "macos")]
fn installed_path(exe: PathBuf, home: PathBuf) -> Option<PathBuf> {
    if !exe.ends_with("MyMail.app/Contents/MacOS/mymail") {
        return None;
    }
    let bundle = exe.parent()?.parent()?.parent()?.to_path_buf();
    let dir = bundle.parent()?;
    let ok = cfg!(feature = "update-test")
        || dir == Path::new("/Applications")
        || dir == home.join("Applications");
    ok.then_some(bundle)
}

#[cfg(not(target_os = "macos"))]
fn installed_path(exe: PathBuf, home: PathBuf) -> Option<PathBuf> {
    (exe == home.join(".local/bin/mymail")).then_some(exe)
}

/// Exclusive across instances, so two copies of the app never swap at once.
fn lock() -> Result<Option<File>> {
    let dir = gmail::dir();
    std::fs::create_dir_all(&dir).map_err(err)?;
    let f = File::create(dir.join("update.lock")).map_err(err)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(err(e)),
    }
}

// ---------- unpack, validate, swap ----------

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn run_ok(cmd: &mut Command) -> Result<std::process::Output> {
    let out = cmd.output().map_err(err)?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

/// Unpacks into `stage` and returns the new bundle (macOS) or executable (Linux).
#[cfg(target_os = "macos")]
fn unpack(archive: &Path, stage: &Path) -> Result<PathBuf> {
    let out = stage.join("new");
    std::fs::create_dir(&out).map_err(err)?;
    run_ok(
        Command::new("ditto")
            .arg("-x")
            .arg("-k")
            .arg(archive)
            .arg(&out),
    )?;
    let names: Vec<_> = std::fs::read_dir(&out)
        .map_err(err)?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    if names != ["MyMail.app"] {
        return Err("unexpected archive layout".into());
    }
    let app = out.join("MyMail.app");
    if !is_regular_file(&exe_in(&app)) {
        return Err("archive has no executable".into());
    }
    run_ok(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app),
    )?;
    Ok(app)
}

#[cfg(not(target_os = "macos"))]
fn unpack(archive: &Path, stage: &Path) -> Result<PathBuf> {
    let list = run_ok(Command::new("tar").arg("-tzf").arg(archive))?;
    if String::from_utf8_lossy(&list.stdout).trim() != "mymail" {
        return Err("unexpected archive layout".into());
    }
    run_ok(
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(stage)
            .arg("mymail"),
    )?;
    let exe = stage.join("mymail");
    if !is_regular_file(&exe) {
        return Err("archive has no executable".into());
    }
    std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(err)?;
    Ok(exe)
}

fn exe_in(new: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        new.join("Contents/MacOS/mymail")
    } else {
        new.to_path_buf()
    }
}

/// Atomic, so a crash never leaves the app missing. macOS: the old bundle ends up at `new`'s
/// path and stays until `cleanup` removes it.
#[cfg(target_os = "macos")]
fn swap(new: &Path, target: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(err);
    let (a, b) = (c(new)?, c(target)?);
    // SAFETY: both are valid NUL-terminated paths that outlive the call.
    let rc = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "can't swap in the new version: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn swap(new: &Path, target: &Path) -> Result<()> {
    std::fs::rename(new, target).map_err(err)
}

fn stage_and_swap(target: &Path, stage: &Path, c: &Candidate) -> Result<()> {
    let bytes = agent()
        .get(&c.url)
        .call()
        .map_err(err)?
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(err)?;
    if sha256_hex(&bytes) != c.sha256 {
        return Err("download does not match its digest".into());
    }
    let archive = stage.join("update.archive");
    std::fs::write(&archive, bytes).map_err(err)?;
    let new = unpack(&archive, stage)?;
    // Also proves the right architecture and version before it replaces anything.
    let out = run_ok(Command::new(exe_in(&new)).arg("--version"))?;
    if String::from_utf8_lossy(&out.stdout).trim() != format!("mymail {}", c.version) {
        return Err("downloaded build reports a different version".into());
    }
    swap(&new, target)
}

fn install(target: &Path, c: &Candidate) -> Result<()> {
    let stage = target
        .parent()
        .ok_or("bad path")?
        .join(format!("{STAGE_PREFIX}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir(&stage).map_err(err)?;
    let r = stage_and_swap(target, &stage, c);
    // On macOS a successful swap leaves the old bundle in `stage`; `cleanup` removes it later.
    if r.is_err() || cfg!(not(target_os = "macos")) {
        let _ = std::fs::remove_dir_all(&stage);
    }
    r
}

// ---------- entry points ----------

/// Installs a newer release if there is one. `Ok(Some(version))` means it is in place and a
/// restart will use it. An unreachable GitHub or an odd release is not an error: try again later.
pub fn run() -> Result<Option<String>> {
    let Some(target) = target() else {
        return Ok(None);
    };
    let c = match latest(&target) {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(None),
        Err(e) => {
            eprintln!("update check: {e}");
            return Ok(None);
        }
    };
    let Some(_lock) = lock()? else {
        return Ok(None);
    };
    if c.ver <= newest_known(&target) {
        return Ok(None); // another instance installed it while we downloaded
    }
    install(&target, &c)?;
    Ok(Some(c.version))
}

/// Removes old bundles left by earlier updates (only ones over a day old, in case the old
/// process is still running).
pub fn cleanup() {
    let Some(parent) = target().and_then(|t| t.parent().map(Path::to_path_buf)) else {
        return;
    };
    let Ok(Some(_lock)) = lock() else { return };
    for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        let stale = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
            t.elapsed()
                .is_ok_and(|age| age > Duration::from_secs(86400))
        });
        if stale && e.file_name().to_string_lossy().starts_with(STAGE_PREFIX) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Starts a helper that reopens the app once this process has exited. The caller quits on `Ok`.
pub fn relaunch() -> Result<()> {
    let target = target().ok_or("not an installed build")?;
    let open: Vec<std::ffi::OsString> = if cfg!(target_os = "macos") {
        vec!["open".into(), target.into()]
    } else {
        vec![target.into()]
    };
    Command::new("sh")
        .args([
            "-c",
            r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; shift; exec "$@""#,
            "sh",
        ])
        .arg(std::process::id().to_string())
        .args(open)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mymail-upd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn versions_compare_numerically_and_reject_odd_tags() {
        assert!(parse_version("v0.10.0") > parse_version("v0.9.0"));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        for bad in [
            "v1.2",
            "v1.2.3-rc1",
            "vv1.2.3",
            "1.2.3.4",
            "1.+2.3",
            "99999999999999999999.0.0",
            "",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn candidate_needs_a_platform_asset_and_a_sha256_digest() {
        let digest = format!("sha256:{}", "AB".repeat(32));
        let rel = |assets: Value| json!({"tag_name": "v1.4.0", "assets": assets});
        let ok = rel(json!([
            {"name": "other.zip", "browser_download_url": "https://x/other"},
            {"name": "a.zip", "browser_download_url": "https://x/a", "digest": digest},
        ]));
        let c = candidate(&ok, "a.zip").unwrap().unwrap();
        assert_eq!(
            (c.version.as_str(), c.url.as_str()),
            ("1.4.0", "https://x/a")
        );
        assert_eq!(c.sha256, "ab".repeat(32));
        assert!(candidate(&ok, "missing.zip").unwrap().is_none());
        for bad in [
            json!(null),
            json!("md5:abc"),
            json!(format!("sha256:{}", "ab".repeat(31))),
        ] {
            let r = rel(
                json!([{"name": "a.zip", "browser_download_url": "https://x/a", "digest": bad}]),
            );
            assert!(candidate(&r, "a.zip").is_err());
        }
        assert!(candidate(&json!({"tag_name": "nightly", "assets": []}), "a.zip").is_err());
    }

    #[test]
    fn digest_is_lowercase_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    #[cfg(all(target_os = "macos", not(feature = "update-test")))]
    fn only_the_canonical_bundle_updates() {
        let home = PathBuf::from("/Users/me");
        let exe = |d: &str| PathBuf::from(format!("{d}/MyMail.app/Contents/MacOS/mymail"));
        let got = |d: &str| installed_path(exe(d), home.clone());
        assert_eq!(
            got("/Applications"),
            Some("/Applications/MyMail.app".into())
        );
        assert_eq!(
            got("/Users/me/Applications"),
            Some("/Users/me/Applications/MyMail.app".into())
        );
        assert_eq!(got("/repo/target/release"), None);
        assert_eq!(
            installed_path("/Applications/Other.app/Contents/MacOS/mymail".into(), home),
            None
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn only_the_installed_executable_updates() {
        let home = PathBuf::from("/home/me");
        let got = |p: &str| installed_path(p.into(), home.clone());
        assert_eq!(
            got("/home/me/.local/bin/mymail"),
            Some("/home/me/.local/bin/mymail".into())
        );
        // What Linux reports for the running exe after an update replaced it.
        assert_eq!(got("/home/me/.local/bin/mymail (deleted)"), None);
        assert_eq!(got("/repo/target/release/mymail"), None);
    }

    #[test]
    fn second_lock_holder_is_refused() {
        let p = tmp("lock").join("l");
        let (a, b) = (File::create(&p).unwrap(), File::create(&p).unwrap());
        a.try_lock().unwrap();
        assert!(matches!(b.try_lock(), Err(TryLockError::WouldBlock)));
    }

    #[test]
    fn swap_replaces_the_target_and_keeps_the_old_one() {
        let d = tmp("swap");
        #[cfg(target_os = "macos")]
        {
            // Directory exchange: the old bundle ends up at the staged path.
            let (target, new) = (d.join("MyMail.app"), d.join("new"));
            for (dir, v) in [(&target, "old"), (&new, "new")] {
                std::fs::create_dir(dir).unwrap();
                std::fs::write(dir.join("v"), v).unwrap();
            }
            swap(&new, &target).unwrap();
            assert_eq!(std::fs::read_to_string(target.join("v")).unwrap(), "new");
            assert_eq!(std::fs::read_to_string(new.join("v")).unwrap(), "old");
        }
        #[cfg(not(target_os = "macos"))]
        {
            let (target, new) = (d.join("mymail"), d.join("mymail.new"));
            std::fs::write(&target, "old").unwrap();
            std::fs::write(&new, "new").unwrap();
            swap(&new, &target).unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        }
    }
}
