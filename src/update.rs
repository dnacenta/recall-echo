// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Self-update: replace the installed binary with a GitHub release.
//!
//! The same job `install.sh` does, built in. Resolves the latest (or a
//! pinned) release, downloads the platform asset, verifies the new binary
//! answers `--version` correctly, and atomically swaps it into the resolved
//! `current_exe` path. No privilege escalation: an unwritable install
//! directory is an error, not a sudo prompt.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::RecallError;

const REPO: &str = "dnacenta/recall-echo";

/// Exit code for `--check` when an update is available. Distinct from 1 so
/// scripts can tell "outdated" from "error".
pub const EXIT_UPDATE_AVAILABLE: i32 = 10;

pub struct UpdateOpts {
    /// Report current vs latest and change nothing.
    pub check: bool,
    /// Install this release tag instead of the latest.
    pub version: Option<String>,
    /// Proceed even when already on the requested version.
    pub force: bool,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Run the update flow. Returns the process exit code (0, or
/// [`EXIT_UPDATE_AVAILABLE`] for `--check` with an update pending).
pub async fn run(opts: UpdateOpts) -> Result<i32, RecallError> {
    let current = env!("CARGO_PKG_VERSION");

    let client = http_client()?;
    let release = fetch_release(&client, opts.version.as_deref()).await?;
    let target = normalize(&release.tag_name);

    if opts.check {
        if target == current {
            println!("recall-echo {current} is up to date");
            return Ok(0);
        }
        println!(
            "recall-echo {current} installed; {} available{}",
            release.tag_name,
            relation_note(current, &target)
        );
        return Ok(EXIT_UPDATE_AVAILABLE);
    }

    if target == current && !opts.force {
        println!(
            "recall-echo {current} is already the {} release",
            release.tag_name
        );
        return Ok(0);
    }

    let asset_name = asset_name(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        RecallError::Other(format!(
            "no release binary for {}/{} — build from source with `cargo install recall-echo --locked`",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            RecallError::Other(format!(
                "release {} has no asset named {asset_name}",
                release.tag_name
            ))
        })?;

    let install_path = resolve_install_path()?;
    let install_dir = install_path.parent().ok_or_else(|| {
        RecallError::Other(format!(
            "{} has no parent directory",
            install_path.display()
        ))
    })?;
    preflight_writable(install_dir)?;

    println!(
        "updating {} {current} \u{2192} {}{}",
        install_path.display(),
        release.tag_name,
        relation_note(current, &target)
    );

    install(
        &client,
        &asset.browser_download_url,
        &install_path,
        current,
        &target,
    )
    .await?;

    println!("updated to {}", release.tag_name);
    println!("note: long-running recall-echo processes (serve daemon, MCP) keep the old version until restarted");
    Ok(0)
}

fn http_client() -> Result<reqwest::Client, RecallError> {
    reqwest::Client::builder()
        .user_agent(concat!("recall-echo/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| RecallError::Other(format!("http client: {e}")))
}

/// GET the release metadata. `GITHUB_TOKEN`/`GH_TOKEN`, when present, is sent
/// to this API call only — never to the asset download, whose redirect chain
/// leaves api.github.com.
async fn fetch_release(
    client: &reqwest::Client,
    tag: Option<&str>,
) -> Result<Release, RecallError> {
    let url = match tag {
        Some(tag) => format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };

    let mut req = client.get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| RecallError::Other(format!("github api: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(RecallError::Other(match tag {
            Some(tag) => format!("release not found: no release tagged {tag}"),
            None => "release not found: the repository has no releases".into(),
        }));
    }
    if !status.is_success() {
        return Err(RecallError::Other(format!(
            "github api returned {status} for {url} (rate-limited? set GITHUB_TOKEN)"
        )));
    }
    resp.json::<Release>()
        .await
        .map_err(|e| RecallError::Other(format!("github api response: {e}")))
}

/// Map an OS/arch pair to the release asset name — the exact matrix
/// `release.yml` builds and `install.sh` downloads.
fn asset_name(os: &str, arch: &str) -> Option<String> {
    let target = match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return None,
    };
    Some(format!("recall-echo-{target}.tar.gz"))
}

/// Strip a leading `v` from a release tag.
fn normalize(tag: &str) -> String {
    tag.strip_prefix('v').unwrap_or(tag).to_string()
}

/// Parse `X.Y.Z` (optionally `vX.Y.Z`) into a comparable triple.
fn parse_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = tag.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// " (downgrade)" when the target is older than what's running, "" otherwise
/// or when either version doesn't parse.
fn relation_note(current: &str, target: &str) -> &'static str {
    match (parse_tag(current), parse_tag(target)) {
        (Some(cur), Some(tgt)) if tgt < cur => " (downgrade)",
        _ => "",
    }
}

/// The file to replace: `current_exe` with every symlink resolved, so a
/// symlinked launcher (`~/.cargo/bin/recall-echo → /usr/local/bin/recall-echo`)
/// updates the target and the link survives.
fn resolve_install_path() -> Result<PathBuf, RecallError> {
    let exe = std::env::current_exe()
        .map_err(|e| RecallError::Other(format!("cannot locate the running binary: {e}")))?;
    std::fs::canonicalize(&exe)
        .map_err(|e| RecallError::Other(format!("cannot resolve {}: {e}", exe.display())))
}

/// Fail before downloading anything if the install directory can't be
/// written. Probes with a real file create — permission bits alone lie under
/// ACLs and read-only mounts.
pub fn preflight_writable(dir: &Path) -> Result<(), RecallError> {
    let probe = dir.join(format!(".recall-echo-update-probe.{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(RecallError::Other(format!(
            "install directory {} is not writable ({e}) — re-run with sufficient privileges",
            dir.display()
        ))),
    }
}

/// Download, extract, verify, swap. All temp files live in the install
/// directory itself — same filesystem, so the final renames are atomic — and
/// are removed on every exit path by the guard.
async fn install(
    client: &reqwest::Client,
    asset_url: &str,
    install_path: &Path,
    current_version: &str,
    target_version: &str,
) -> Result<(), RecallError> {
    let install_dir = install_path
        .parent()
        .expect("validated by caller: install path has a parent");
    let pid = std::process::id();
    let archive_path = install_dir.join(format!(".recall-echo-update.{pid}.tar.gz"));
    let new_bin_path = install_dir.join(format!(".recall-echo-update.{pid}.bin"));
    let _guard = TempGuard(vec![archive_path.clone(), new_bin_path.clone()]);

    download(client, asset_url, &archive_path).await?;
    extract_binary(&archive_path, &new_bin_path)?;
    self_check(&new_bin_path, target_version)?;
    swap(install_path, &new_bin_path, current_version, target_version)
}

/// Removes whatever temp files still exist when the update flow exits.
struct TempGuard(Vec<PathBuf>);

impl Drop for TempGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Stream the release asset to disk. Deliberately built without any auth
/// header: the download 302s off api.github.com, and a `GITHUB_TOKEN` must
/// never travel to the redirect target.
async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<(), RecallError> {
    use std::io::Write;

    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| RecallError::Other(format!("download: {e}")))?;
    if !resp.status().is_success() {
        return Err(RecallError::Other(format!(
            "download returned {} for {url}",
            resp.status()
        )));
    }

    let mut file = std::fs::File::create(dest)?;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| RecallError::Other(format!("download: {e}")))?
    {
        file.write_all(&chunk)?;
    }
    file.flush()?;
    Ok(())
}

/// Pull the single binary out of the release tar.gz.
fn extract_binary(archive_path: &Path, dest: &Path) -> Result<(), RecallError> {
    use std::os::unix::fs::PermissionsExt;

    let file = std::fs::File::open(archive_path)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));

    for entry in archive
        .entries()
        .map_err(|e| RecallError::Other(format!("release archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| RecallError::Other(format!("release archive: {e}")))?;
        if entry.header().entry_type().is_file() {
            entry
                .unpack(dest)
                .map_err(|e| RecallError::Other(format!("release archive: {e}")))?;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
            return Ok(());
        }
    }
    Err(RecallError::Other(
        "release archive contains no regular file".into(),
    ))
}

/// A binary that can't report the version it is supposed to be is not
/// installed. Runs `<binary> --version` and requires the expected version to
/// appear as a whitespace-delimited word of its stdout.
pub fn self_check(binary: &Path, expected_version: &str) -> Result<(), RecallError> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| RecallError::Other(format!("cannot run {}: {e}", binary.display())))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success()
        && stdout
            .split_whitespace()
            .any(|word| word == expected_version)
    {
        return Ok(());
    }
    Err(RecallError::Other(format!(
        "{} --version reported {:?}, expected {expected_version}",
        binary.display(),
        stdout.trim()
    )))
}

/// Atomically replace `install` with `new_bin`. The running binary keeps its
/// inode, so this is safe while a daemon or the updater itself is executing
/// the old file. The old binary survives as `.old.<version>` until the swap
/// verifies; any failure after the first rename restores it.
pub fn swap(
    install: &Path,
    new_bin: &Path,
    current_version: &str,
    target_version: &str,
) -> Result<(), RecallError> {
    let name = install
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RecallError::Other(format!("bad install path {}", install.display())))?;
    let old = install.with_file_name(format!("{name}.old.{current_version}"));

    std::fs::rename(install, &old)
        .map_err(|e| RecallError::Other(format!("cannot move current binary aside: {e}")))?;

    if let Err(e) = std::fs::rename(new_bin, install) {
        let _ = std::fs::rename(&old, install);
        return Err(RecallError::Other(format!(
            "cannot move new binary into place ({e}); previous version restored"
        )));
    }

    match self_check(install, target_version) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_file(&old) {
                eprintln!(
                    "warning: could not remove backup {} ({e}) — safe to delete",
                    old.display()
                );
            }
            Ok(())
        }
        Err(check_err) => {
            let _ = std::fs::remove_file(install);
            match std::fs::rename(&old, install) {
                Ok(()) => Err(RecallError::Other(format!(
                    "installed binary failed verification ({check_err}); previous version restored"
                ))),
                Err(restore_err) => Err(RecallError::Other(format!(
                    "installed binary failed verification ({check_err}) and restoring failed \
                     ({restore_err}) — previous binary preserved at {}",
                    old.display()
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_maps_all_release_targets() {
        assert_eq!(
            asset_name("linux", "x86_64").as_deref(),
            Some("recall-echo-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            asset_name("linux", "aarch64").as_deref(),
            Some("recall-echo-aarch64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            asset_name("macos", "x86_64").as_deref(),
            Some("recall-echo-x86_64-apple-darwin.tar.gz")
        );
        assert_eq!(
            asset_name("macos", "aarch64").as_deref(),
            Some("recall-echo-aarch64-apple-darwin.tar.gz")
        );
    }

    #[test]
    fn asset_name_rejects_unsupported() {
        assert_eq!(asset_name("windows", "x86_64"), None);
        assert_eq!(asset_name("linux", "riscv64"), None);
    }

    #[test]
    fn parse_tag_strips_v_and_orders() {
        assert_eq!(parse_tag("v4.2.0"), Some((4, 2, 0)));
        assert_eq!(parse_tag("4.2.0"), Some((4, 2, 0)));
        assert!(parse_tag("v4.10.0") > parse_tag("v4.9.9"));
        assert!(parse_tag("v5.0.0") > parse_tag("v4.99.99"));
        assert_eq!(parse_tag("main"), None);
        assert_eq!(parse_tag("v4.2"), None);
        assert_eq!(parse_tag(""), None);
    }

    #[test]
    fn relation_note_flags_downgrades_only() {
        assert_eq!(relation_note("4.3.0", "4.2.0"), " (downgrade)");
        assert_eq!(relation_note("4.2.0", "4.3.0"), "");
        assert_eq!(relation_note("4.2.0", "4.2.0"), "");
        assert_eq!(relation_note("4.2.0", "garbage"), "");
    }

    #[test]
    fn install_path_follows_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-bin");
        std::fs::write(&real, b"x").unwrap();
        let link = dir.path().join("link-bin");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = std::fs::canonicalize(&link).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&real).unwrap());
    }

    #[test]
    fn preflight_rejects_unwritable_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = preflight_writable(&ro);
        // Root bypasses permission bits, so only assert the rejection when the
        // directory is genuinely unwritable for this process.
        if std::fs::write(ro.join("root-check"), b"").is_err() {
            assert!(result.is_err());
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(preflight_writable(&ro).is_ok());
    }
}
