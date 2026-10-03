//! Self-update from the GitHub releases page. Nothing here runs unless `Config.check_updates` is on.
//!
//! A running exe can't be overwritten on Windows, but it can be renamed: the new exe is downloaded
//! next to the current one, the current one is renamed to `TUFFClip.old.exe`, the new one takes its
//! place, and it is started with `--relaunch --updated` while this copy exits. The new copy waits
//! until the old exe can be deleted (= this copy is gone) before it starts.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;

const REPO: &str = "SpicyDennis/tuffclip";
pub const RELEASES_PAGE: &str = "https://github.com/SpicyDennis/tuffclip/releases/latest";
const OLD_NAME: &str = "TUFFClip.old.exe";
const PART_NAME: &str = "TUFFClip.update.part";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub size: u64,
    /// "sha256:<hex>" when GitHub provides it.
    #[serde(default)]
    pub digest: String,
}

/// GitHub is asked at most this often (unless the user clicks Check now).
pub const CHECK_EVERY_SECS: u64 = 15 * 60;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    /// When the last successful check ran (Unix seconds).
    checked_at: u64,
    latest: Option<Release>,
}

fn cache_path() -> PathBuf {
    crate::config::data_dir().join("update.json")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn parse_ver(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
}

pub fn is_newer(v: &str) -> bool {
    parse_ver(v) > parse_ver(env!("CARGO_PKG_VERSION"))
}

/// The newest release if it is newer than this copy. Unless `force`, GitHub is asked at most once
/// every 15 minutes; calls in between answer from the saved result. A failed check isn't
/// remembered, so the next one tries again.
pub fn check(force: bool) -> Result<Option<Release>> {
    let cache: Cache = std::fs::read_to_string(cache_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let age = now().saturating_sub(cache.checked_at);
    if !force && cache.checked_at > 0 && age < CHECK_EVERY_SECS {
        return Ok(cache.latest.filter(|r| is_newer(&r.version)));
    }
    let latest = fetch_latest()?;
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let _ = std::fs::write(cache_path(), serde_json::to_string_pretty(&Cache { checked_at: now(), latest: latest.clone() })?);
    Ok(latest.filter(|r| is_newer(&r.version)))
}

/// Where the release info comes from. Debug builds only: `TUFFCLIP_UPDATE_FEED` can point at a
/// local file in GitHub's release format (`file:///...`) to test updating without publishing.
fn feed_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(f) = std::env::var("TUFFCLIP_UPDATE_FEED") {
        return f;
    }
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

fn fetch_latest() -> Result<Option<Release>> {
    let out = crate::ff::cmd("curl.exe")
        .args(["-sS", "-L", "--fail", "--max-time", "20", "-H", "Accept: application/vnd.github+json", "-A"])
        .arg(concat!("TUFFClip/", env!("CARGO_PKG_VERSION")))
        .arg(feed_url())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("couldn't start curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("404") {
            bail!("No released version was found on GitHub yet.");
        }
        bail!("Couldn't reach GitHub: {}", crate::ff::tail(&err));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("GitHub sent something unexpected")?;
    let version = v["tag_name"].as_str().unwrap_or("").trim_start_matches(['v', 'V']).to_string();
    let asset = v["assets"].as_array().into_iter().flatten().find(|a| {
        let n = a["name"].as_str().unwrap_or("").to_lowercase();
        n.starts_with("tuffclip") && n.ends_with(".exe")
    });
    let (Some(a), false) = (asset, version.is_empty()) else { return Ok(None) };
    Ok(Some(Release {
        version,
        url: a["browser_download_url"].as_str().unwrap_or("").to_string(),
        size: a["size"].as_u64().unwrap_or(0),
        digest: a["digest"].as_str().unwrap_or("").to_string(),
    }))
}

/// Download `rel` next to the running exe and swap it in. Returns the path of the new exe; the
/// caller starts it and exits. `progress` gets the bytes downloaded so far.
pub fn install(rel: &Release, progress: impl Fn(u64)) -> Result<PathBuf> {
    let test_feed = cfg!(debug_assertions) && std::env::var_os("TUFFCLIP_UPDATE_FEED").is_some() && rel.url.starts_with("file:///");
    if !rel.url.starts_with("https://github.com/") && !test_feed {
        bail!("That download link doesn't point at GitHub.");
    }
    let cur = std::env::current_exe()?;
    let dir = cur.parent().ok_or_else(|| anyhow!("couldn't find TUFFClip's folder"))?.to_path_buf();
    let part = dir.join(PART_NAME);
    let _ = std::fs::remove_file(&part);
    if std::fs::File::create(&part).is_err() {
        bail!("Windows won't let TUFFClip write to its own folder ({}). Download the new version from GitHub instead.", dir.display());
    }

    let mut child = crate::ff::cmd("curl.exe")
        .args(["-L", "--fail", "--silent", "--show-error", "--connect-timeout", "20", "--speed-limit", "1", "--speed-time", "60", "-o"])
        .arg(&part)
        .arg(&rel.url)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("couldn't start curl: {e}"))?;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        progress(std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0));
        std::thread::sleep(std::time::Duration::from_millis(400));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = child.stderr.take() {
            use std::io::Read;
            let _ = s.read_to_string(&mut err);
        }
        let _ = std::fs::remove_file(&part);
        bail!("Download failed: {}", crate::ff::tail(&err));
    }
    if let Err(e) = verify(&part, rel) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    // Keep the "TUFFClip v1.2.3.exe" naming if this copy uses it; otherwise keep its own name.
    let cur_name = cur.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let versioned = format!("TUFFClip v{}.exe", env!("CARGO_PKG_VERSION"));
    let new_path = if cur_name.eq_ignore_ascii_case(&versioned) {
        dir.join(format!("TUFFClip v{}.exe", rel.version))
    } else {
        cur.clone()
    };

    let old = dir.join(OLD_NAME);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&cur, &old).context("couldn't move the running TUFFClip aside")?;
    if new_path != cur {
        let _ = std::fs::remove_file(&new_path);
    }
    if let Err(e) = std::fs::rename(&part, &new_path) {
        let _ = std::fs::rename(&old, &cur);
        let _ = std::fs::remove_file(&part);
        bail!("couldn't put the new version in place: {e}");
    }
    Ok(new_path)
}

fn verify(file: &Path, rel: &Release) -> Result<()> {
    let len = std::fs::metadata(file)?.len();
    if rel.size > 0 && len != rel.size {
        bail!("The download is incomplete ({len} of {} bytes). Try again.", rel.size);
    }
    let mut head = [0u8; 2];
    use std::io::Read;
    std::fs::File::open(file)?.read_exact(&mut head)?;
    if &head != b"MZ" {
        bail!("The download isn't a Windows program.");
    }
    if let Some(want) = rel.digest.strip_prefix("sha256:") {
        let out = crate::ff::cmd("certutil.exe").arg("-hashfile").arg(file).arg("SHA256").stdin(Stdio::null()).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let got = text
            .lines()
            .map(|l| l.replace(' ', "").to_lowercase())
            .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow!("couldn't check the download's fingerprint"))?;
        if got != want.to_lowercase() {
            bail!("The download doesn't match the fingerprint GitHub lists for it, so it wasn't installed.");
        }
    }
    Ok(())
}

/// Called first thing by a copy started with `--updated`: wait (up to 30 s) until the copy it
/// replaced has exited, which is when its renamed exe can finally be deleted.
pub fn wait_for_old() {
    let Some(old) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(OLD_NAME))) else { return };
    for _ in 0..120 {
        if !old.exists() || std::fs::remove_file(&old).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Remove what an earlier update left behind (best effort).
pub fn clean_up() {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else { return };
    let _ = std::fs::remove_file(dir.join(OLD_NAME));
    let _ = std::fs::remove_file(dir.join(PART_NAME));
}
