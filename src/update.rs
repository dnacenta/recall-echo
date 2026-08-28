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

    if let Some(tag) = opts.version.as_deref() {
        validate_tag(tag)?;
    }

    let client = http_client()?;
    let release = fetch_release(&client, opts.version.as_deref()).await?;
    let target = normalize(&release.tag_name);
    let ordering = compare(current, &target);

    if opts.check {
        return Ok(match ordering {
            Some(std::cmp::Ordering::Equal) => {
                println!("recall-echo {current} is up to date");
                0
            }
            Some(std::cmp::Ordering::Less) => {
                println!(
                    "recall-echo {current} installed; {} available",
                    release.tag_name
                );
                EXIT_UPDATE_AVAILABLE
            }
            Some(std::cmp::Ordering::Greater) => {
                println!(
                    "recall-echo {current} is ahead of the {} release",
                    release.tag_name
                );
                0
            }
            None => {
                println!(
                    "recall-echo {current} installed; release {} has an unrecognized version format",
                    release.tag_name
                );
                EXIT_UPDATE_AVAILABLE
            }
        });
    }

    match ordering {
        Some(std::cmp::Ordering::Equal) if !opts.force => {
            println!(
                "recall-echo {current} is already the {} release",
                release.tag_name
            );
            return Ok(0);
        }
        Some(std::cmp::Ordering::Equal) | Some(std::cmp::Ordering::Less) => {}
        // Downgrades (and unparseable versions) install only when the user
        // both named the release and forced it — a regressed `latest` must
        // never silently roll an install back.
        Some(std::cmp::Ordering::Greater) | None => {
            if !(opts.version.is_some() && opts.force) {
                return Err(RecallError::Other(format!(
                    "refusing to replace {current} with {} (downgrade or unrecognized version) — \
                     pass both --version {} and --force to do it anyway",
                    release.tag_name, release.tag_name
                )));
            }
        }
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
        // An inactivity timeout, not a total one: a 40MB asset on a slow link
        // is legitimate, a stalled socket is not.
        .read_timeout(std::time::Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
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
    let token = ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|t| !t.is_empty()));
    if let Some(token) = token {
        req = req.bearer_auth(token);
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
    let release = resp
        .json::<Release>()
        .await
        .map_err(|e| RecallError::Other(format!("github api response: {e}")))?;
    if let Some(tag) = tag {
        if release.tag_name != tag {
            return Err(RecallError::Other(format!(
                "github api returned release {} for requested tag {tag}",
                release.tag_name
            )));
        }
    }
    Ok(release)
}

/// The asset URL comes out of API JSON — pin it to GitHub's release hosts
/// before fetching, so a poisoned response can't point the download anywhere
/// else.
fn validate_asset_url(url: &str) -> Result<(), RecallError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| RecallError::Other(format!("bad asset url {url:?}: {e}")))?;
    let host_ok = matches!(
        parsed.host_str(),
        Some(host) if host == "github.com" || host == "api.github.com"
            || host.ends_with(".githubusercontent.com")
    );
    if parsed.scheme() == "https" && host_ok {
        Ok(())
    } else {
        Err(RecallError::Other(format!(
            "refusing asset url {url} — not an https GitHub release host"
        )))
    }
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

/// A user-supplied tag travels into the API URL path, so only allow
/// characters that cannot form path segments or escapes — `/`, `%` and `..`
/// would let a crafted tag retarget the request at a different repository.
fn validate_tag(tag: &str) -> Result<(), RecallError> {
    let charset_ok = !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if charset_ok && !tag.contains("..") {
        Ok(())
    } else {
        Err(RecallError::Other(format!(
            "invalid release tag {tag:?} — expected something like v4.3.0"
        )))
    }
}

/// `current` compared to `target`: `Less` means an update is available,
/// `Greater` means the running binary is newer than the target (installing it
/// would be a downgrade). `None` when either side doesn't parse and the
/// strings differ.
fn compare(current: &str, target: &str) -> Option<std::cmp::Ordering> {
    if current == target {
        return Some(std::cmp::Ordering::Equal);
    }
    match (parse_tag(target), parse_tag(current)) {
        (Some(t), Some(c)) => Some(c.cmp(&t)),
        _ => None,
    }
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
    resolve_from(&exe)
}

fn resolve_from(exe: &Path) -> Result<PathBuf, RecallError> {
    std::fs::canonicalize(exe)
        .map_err(|e| RecallError::Other(format!("cannot resolve {}: {e}", exe.display())))
}

/// Fail before downloading anything if the install directory can't be
/// written. Probes with a real file create — permission bits alone lie under
/// ACLs and read-only mounts.
#[doc(hidden)]
pub fn preflight_writable(dir: &Path) -> Result<(), RecallError> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".recall-echo-update-probe.{}.{nonce:08x}",
        std::process::id()
    ));
    match open_private_new(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(RecallError::Other(format!(
            "install directory {} is not writable ({e}) — re-run with sufficient privileges",
            dir.display()
        ))),
    }
}

/// A very generous ceiling on the compressed asset and the extracted binary.
/// Real assets are ~40MB compressed / ~120MB extracted; anything past this is
/// a hostile or broken release, aborted before it can fill the filesystem
/// that holds the install directory.
const MAX_ARTIFACT_BYTES: u64 = 500 * 1024 * 1024;

/// Download, extract, verify, swap. All temp files live in the install
/// directory itself — same filesystem, so the final renames are atomic — and
/// are removed on every exit path by the guard (plus a sweep for temps a
/// killed earlier run left behind).
async fn install(
    client: &reqwest::Client,
    asset_url: &str,
    install_path: &Path,
    current_version: &str,
    target_version: &str,
) -> Result<(), RecallError> {
    validate_asset_url(asset_url)?;
    let install_dir = install_path
        .parent()
        .expect("validated by caller: install path has a parent");
    sweep_stale_temps(install_dir);

    // Pid plus a clock nonce: not guessable ahead of time the way a bare pid
    // is, and unique enough that concurrent updaters can't collide.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let stem = format!(".recall-echo-update.{}.{nonce:08x}", std::process::id());
    let archive_path = install_dir.join(format!("{stem}.tar.gz"));
    let new_bin_path = install_dir.join(format!("{stem}.bin"));
    let _guard = TempGuard(vec![archive_path.clone(), new_bin_path.clone()]);

    download(client, asset_url, &archive_path).await?;
    extract_binary(&archive_path, &new_bin_path)?;
    self_check(&new_bin_path, target_version)?;
    swap(install_path, &new_bin_path, current_version, target_version)
}

/// Remove `.recall-echo-update.*` temps that a killed run left behind (the
/// Drop guard can't fire on SIGKILL). A temp whose embedded pid is still
/// alive is left alone.
fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(".recall-echo-update") else {
            continue;
        };
        let pid: String = rest
            .trim_start_matches(|c: char| !c.is_ascii_digit())
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        // /proc only exists on Linux; elsewhere the liveness probe fails and
        // the temp is treated as stale, which at worst aborts a concurrent
        // updater cleanly.
        let alive = !pid.is_empty() && Path::new("/proc").join(&pid).exists();
        if !alive {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

    // create_new + 0600: never follow a pre-planted symlink, never leave the
    // half-written archive readable or writable by anyone else.
    let file = open_private_new(dest)?;
    let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut written: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| RecallError::Other(format!("download: {e}")))?
    {
        written += chunk.len() as u64;
        if written > MAX_ARTIFACT_BYTES {
            return Err(RecallError::Other(format!(
                "download exceeded {MAX_ARTIFACT_BYTES} bytes — refusing to continue"
            )));
        }
        writer.write_all(&chunk)?;
    }
    writer.flush()?;
    Ok(())
}

/// Exclusive-create a mode-0600 file — refuses to touch anything that
/// already exists at the path, symlink or otherwise.
fn open_private_new(path: &Path) -> Result<std::fs::File, RecallError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| RecallError::Other(format!("cannot create {}: {e}", path.display())))
}

/// Pull the `recall-echo` binary out of the release tar.gz. The entry is
/// selected by name, written via our own exclusive-create (so the archive's
/// own modes, owner, and timestamps are never honoured), and staged private
/// (0700) until the swap widens it.
fn extract_binary(archive_path: &Path, dest: &Path) -> Result<(), RecallError> {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    let file = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(archive_path)?);
    let mut archive = tar::Archive::new(flate2::bufread::GzDecoder::new(file));

    for entry in archive
        .entries()
        .map_err(|e| RecallError::Other(format!("release archive: {e}")))?
    {
        let mut entry = entry.map_err(|e| RecallError::Other(format!("release archive: {e}")))?;
        let is_binary = entry.header().entry_type().is_file()
            && entry
                .path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n == "recall-echo"))
                .unwrap_or(false);
        if !is_binary {
            continue;
        }
        let mut out = std::io::BufWriter::with_capacity(1 << 20, open_private_new(dest)?);
        let copied = std::io::copy(&mut (&mut entry).take(MAX_ARTIFACT_BYTES + 1), &mut out)
            .map_err(|e| RecallError::Other(format!("release archive: {e}")))?;
        if copied > MAX_ARTIFACT_BYTES {
            return Err(RecallError::Other(format!(
                "extracted binary exceeded {MAX_ARTIFACT_BYTES} bytes — refusing to continue"
            )));
        }
        out.flush()?;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }
    Err(RecallError::Other(
        "release archive contains no `recall-echo` binary".into(),
    ))
}

/// A binary that can't report the version it is supposed to be is not
/// installed. Runs `<binary> --version` and requires the expected version to
/// appear as a whitespace-delimited word of its stdout.
#[doc(hidden)]
pub fn self_check(binary: &Path, expected_version: &str) -> Result<(), RecallError> {
    // env_clear: the not-yet-trusted binary runs without inheriting tokens
    // or provider keys from our environment.
    let output = std::process::Command::new(binary)
        .arg("--version")
        .env_clear()
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
#[doc(hidden)]
pub fn swap(
    install: &Path,
    new_bin: &Path,
    current_version: &str,
    target_version: &str,
) -> Result<(), RecallError> {
    use std::os::unix::fs::PermissionsExt;

    let name = install
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RecallError::Other(format!("bad install path {}", install.display())))?;
    let old = install.with_file_name(format!("{name}.old.{current_version}"));

    // The staged binary was private (0700) until this moment; widen it only
    // as it goes live.
    std::fs::set_permissions(new_bin, std::fs::Permissions::from_mode(0o755))?;

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
        assert_eq!(
            resolve_from(&link).unwrap(),
            std::fs::canonicalize(&real).unwrap()
        );
    }

    #[test]
    fn validate_tag_rejects_path_tricks() {
        assert!(validate_tag("v4.3.0").is_ok());
        assert!(validate_tag("4.3.0-rc.1+build").is_ok());
        assert!(validate_tag("../../../attacker/evil/releases/latest").is_err());
        assert!(validate_tag("v4%2e3").is_err());
        assert!(validate_tag("v4..3").is_err());
        assert!(validate_tag("").is_err());
        assert!(validate_tag(&"v".repeat(65)).is_err());
    }

    #[test]
    fn compare_orders_and_flags_unparseable() {
        use std::cmp::Ordering;
        assert_eq!(compare("4.2.0", "4.3.0"), Some(Ordering::Less));
        assert_eq!(compare("4.3.0", "4.2.0"), Some(Ordering::Greater));
        assert_eq!(compare("4.3.0", "4.3.0"), Some(Ordering::Equal));
        assert_eq!(compare("4.3.0", "garbage"), None);
        // String-equal beats unparseable.
        assert_eq!(compare("garbage", "garbage"), Some(Ordering::Equal));
    }

    #[test]
    fn asset_url_pinned_to_github_hosts() {
        assert!(validate_asset_url(
            "https://github.com/dnacenta/recall-echo/releases/download/v4.2.0/x.tar.gz"
        )
        .is_ok());
        assert!(validate_asset_url("https://objects.githubusercontent.com/some/asset").is_ok());
        assert!(validate_asset_url("http://github.com/insecure").is_err());
        assert!(validate_asset_url("https://evil.com/payload.tar.gz").is_err());
        assert!(validate_asset_url("https://githubusercontent.com.evil.com/x").is_err());
    }

    #[test]
    fn preflight_rejects_non_directory_target() {
        // Fails for any uid — the "directory" is a regular file, so the probe
        // create can't succeed even as root.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(preflight_writable(&file).is_err());
    }

    #[test]
    fn extract_takes_binary_by_name_and_stages_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("asset.tar.gz");

        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive_path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        for (name, body) in [
            ("LICENSE", b"mpl".as_slice()),
            ("recall-echo", b"#!/bin/sh\n"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o777);
            header.set_cksum();
            builder.append_data(&mut header, name, body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let dest = dir.path().join("staged");
        extract_binary(&archive_path, &dest).unwrap();
        // Picked by name (not the first entry), staged 0700 regardless of the
        // archive's own mode.
        assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\n");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn extract_errors_without_named_binary() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("asset.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive_path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "README", b"x".as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let err = extract_binary(&archive_path, &dir.path().join("staged")).unwrap_err();
        assert!(err.to_string().contains("no `recall-echo` binary"));
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
