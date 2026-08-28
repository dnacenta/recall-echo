# Spec — `recall-echo update`: self-update from GitHub releases

**Status:** implemented (PR pending)
**Target version:** 4.2.0 → 4.3.0 (new subcommand)
**Drafted:** 2026-08-28
**Baseline:** `main` @ `6fe76e1` (v4.2.0)

---

## Goal

A `recall-echo update` subcommand that replaces the running installation with the latest
GitHub release binary — the same job `install.sh` does, but built in, so an installed user
never needs to find the curl line again.

## Why this matters

Today the upgrade story is "re-run the installer you used two months ago." Nothing in the
tool itself can even say whether a newer version exists. Users who installed via
`install.sh` have no update channel at all; the v4.2.0 rollout surfaced this directly
("don't we have an update command?"). Every serious CLI (rustup, gh, deno) ships
self-update; a memory tool that wants to be ambient infrastructure should too.

---

## Design

### Command surface

```
recall-echo update              # check, download, replace, report
recall-echo update --check      # report current vs latest, change nothing; exit 0 (up to date, or ahead of latest) / 10 (update available)
recall-echo update --version v4.2.0   # pin a specific release instead of latest
recall-echo update --force      # reinstall even if already on the latest version
```

### Behaviour

1. **Resolve target version.** `--version` if given (validated: ≤64 chars of
   `[0-9A-Za-z.\-_+]`, no `..` — the tag travels into the API URL path and must
   not be able to form path segments), else GET
   `https://api.github.com/repos/dnacenta/recall-echo/releases/latest` (`tag_name`;
   for a pinned tag the response's `tag_name` must equal the request). Honour
   `GITHUB_TOKEN`/`GH_TOKEN` if set (rate limits), never require it.
2. **Compare** against `env!("CARGO_PKG_VERSION")` by parsed version ordering.
   Equal → "already up to date", exit 0 (unless `--force`). Target older than
   current (or unparseable) → refuse unless both `--version` and `--force` are
   given — a regressed `latest` must never silently downgrade an install.
   `--check` stops here and reports both versions (current ahead of latest →
   exit 0).
3. **Resolve platform** to a release asset name, same matrix as `install.sh` /
   `release.yml`: `recall-echo-{x86_64,aarch64}-unknown-linux-gnu.tar.gz`,
   `recall-echo-{x86_64,aarch64}-apple-darwin.tar.gz`. Unsupported platform → clear error
   pointing at `cargo install recall-echo --locked`.
4. **Resolve install path** as `std::env::current_exe()` with symlinks fully resolved
   (`fs::canonicalize`) — a symlinked launcher (e.g. `~/.cargo/bin/recall-echo →
   /usr/local/bin/recall-echo`) must update the target, not clobber the link.
5. **Writability preflight** on the install directory before downloading. Not writable →
   error naming the path and suggesting re-running with sufficient privileges. No
   automatic privilege escalation, ever.
6. **Download** the asset over HTTPS (reqwest, rustls: `https_only`, redirect
   limit 5, 30s read timeout, 500MB cap; the asset URL must be an https GitHub
   release host) to a temp file **in the same directory as the install path**
   (same filesystem → atomic rename). Temp names carry pid + a clock nonce;
   files are exclusive-created mode 0600 (never following a pre-planted
   symlink); stale temps from killed runs are swept at start. Extract the
   entry named `recall-echo` from the tar.gz via our own exclusive-create
   copy (archive modes/owners never honoured), staged 0700 until the swap
   widens it to 0755.
7. **Sanity check** the extracted binary: run `<new-binary> --version` (with a
   cleared environment — no tokens or provider keys reach the not-yet-trusted
   binary) and require it to print the target version. A binary that can't
   report its own version is not installed. This is a liveness check, not an
   integrity proof — checksum verification is the deferred follow-up.
8. **Atomic swap**: rename current binary to `<name>.old.<current-version>`, rename new
   binary into place. Rename works while the old binary is running (inode stays alive for
   running processes — this is how the v4.2.0 manual swap was done). On failure mid-swap,
   restore the old binary and report. Remove the `.old` file on success **only after** the
   swap verifies (`recall-echo --version` at the install path reports the target version);
   if removal fails, warn and continue (stale `.old` is cosmetic).
9. **Post-update note**: if a `serve` daemon or MCP process is likely running (best
   effort: does the runtime dir / socket exist?), print one line telling the user
   long-running processes keep the old version until restarted. No automatic restarts.

### Non-goals

- No delta updates, no signature verification beyond HTTPS + the `--version` self-check
  (release CI publishes no checksums today; adding sha256 sums to `release.yml` and
  verifying them here is a follow-up issue, not this spec).
- No update notifications in other commands (no phone-home; checking is explicit).
- No Windows support (matches installer and release matrix).
- No daemon restart orchestration.

### Implementation notes

- New module `src/update.rs`; subcommand wired in `main.rs`.
- Network + archive deps: reuse `reqwest` (rustls); add `flate2` + `tar` (small, pure
  Rust with rust_backend). Gate the subcommand behind a new default-on feature
  `self-update = ["reqwest", "dep:flate2", "dep:tar"]` so `--no-default-features` builds
  stay lean and the command is never half-present.
- The GitHub API call sets a `User-Agent` (required by GitHub) of
  `recall-echo/<version>`.
- Version comparison: strip leading `v` from tags; semver-parse via a manual
  triple-compare (no new semver dep needed) — equal/newer/older all handled; downgrades
  allowed only via explicit `--version` + `--force` prints what it's doing.

---

## Acceptance criteria

### Happy path

- [ ] On a machine running an older release, `recall-echo update` downloads the latest
      release asset for the platform, atomically replaces the binary at the resolved
      `current_exe` target, and `recall-echo --version` afterwards reports the new version.
- [ ] `recall-echo update --check` prints current and latest versions, changes nothing on
      disk, exits 0 when current == latest (or current is ahead) and 10 when an
      update is available.
- [ ] `recall-echo update` when already on the latest release changes nothing and says so.
- [ ] `recall-echo update --version vX.Y.Z` installs exactly that release tag.
- [ ] A symlinked invocation updates the symlink's target file; the symlink survives.

### Edge

- [ ] `--force` reinstalls the current version.
- [ ] `GITHUB_TOKEN`, when set, is sent as a bearer token to the API call only (never to
      the asset download redirect chain's non-GitHub hosts).
- [ ] Temp files land next to the install path and are cleaned up on every exit path
      (success, failure, Ctrl-C best-effort via tempfile-style guard).
- [ ] The old binary is preserved as `.old.<version>` during the swap and removed only
      after post-swap verification.

### Failure

- [ ] Install dir not writable → exits non-zero before any download, error names the path.
- [ ] Unsupported OS/arch → exits non-zero, suggests `cargo install recall-echo --locked`.
- [ ] Network failure / 404 asset / API rate-limit → exits non-zero with the HTTP context;
      binary on disk untouched.
- [ ] Downloaded binary failing the `--version` self-check → swap aborted, old binary
      still in place and functional.
- [ ] Requesting a `--version` tag that has no release → clear "release not found" error.

### Tests

- [ ] Unit: platform→asset-name mapping (all four targets + unsupported), tag/version
      comparison (v-prefix, equal, newer, older, garbage).
- [ ] Unit: install-path resolution follows symlinks (tempdir fixture).
- [ ] Integration: swap logic against a fake "binary" (shell script echoing a version) in
      a tempdir — happy swap, failed self-check rollback, unwritable dir preflight.
      Network layer mocked or skipped (no live GitHub calls in CI).

---

## Deploy

Ships as v4.3.0: tag → release CI builds binaries → manual `cargo publish`. Update the
VPS install (this repo's own machine) via the new command itself as the live smoke test:
`recall-echo update --force` from a 4.3.0 local build must be a no-op swap; then verify
`recall-echo update --check` against the published release.

`install.sh` stays the bootstrap path (first install); README gains an "Updating" line.
