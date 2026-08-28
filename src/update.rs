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
fn preflight_writable(dir: &Path) -> Result<(), RecallError> {
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

async fn install(
    _client: &reqwest::Client,
    _asset_url: &str,
    _install_path: &Path,
    _current_version: &str,
    _target_version: &str,
) -> Result<(), RecallError> {
    Err(RecallError::Other(
        "download and swap are not implemented yet (next increment)".into(),
    ))
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
