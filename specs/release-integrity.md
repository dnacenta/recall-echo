# Spec — Release integrity and distribution

**Status:** draft
**Target version:** 5.0.0 (this spec cuts the release)
**Drafted:** 2026-09-02
**Baseline:** on top of rename, trust-boundary, front-door, data-safety
**Closes:** #49

---

## Goal

Every binary a user runs was built by CI from a tag, is verifiable before it executes, and
can be installed through the channels developers already use.

## Why this matters

`update` and `install.sh` trust TLS-to-GitHub plus a `--version` liveness check
(`update.rs:363-391`, `install.sh:52-57`). A compromised release asset installs cleanly.
CI tests only on Ubuntu, runs no `cargo audit`, and the docs.rs build for 4.3.0 failed. No
binstall metadata, no Homebrew tap.

---

## Design

### 1. Release workflow

On `v*` tag, `release.yml`:

1. Builds the four targets (unchanged) plus, for `v5.*`, the legacy-named tarballs from the
   rename spec.
2. Generates `SHA256SUMS` over every asset.
3. Signs `SHA256SUMS` with **minisign** (secret key in repo secret `MINISIGN_KEY`,
   passphrase in `MINISIGN_PASSPHRASE`) → `SHA256SUMS.minisig`. The public key is a
   constant in `update.rs` and printed by `pulse-recall update --pubkey`.
4. Runs `actions/attest-build-provenance` on the tarballs.
5. Uploads all of it; release notes generated from `CHANGELOG.md`'s section for the tag.

### 2. `update` verifies before it executes

Order: download `SHA256SUMS` + `.minisig` → verify signature with the baked key (fail:
abort, name the key id) → download asset → compute sha256 → compare to the line for that
asset name (fail: abort) → extract → existing self-check → swap. `--insecure-skip-verify`
does not exist. If a release predates signing (≤ 4.3.0) and the user pins it with
`--version`, refuse with a message; there is no reason to go back.

### 3. `install.sh` verifies the digest

Downloads `SHA256SUMS`, checks with `sha256sum` or `shasum -a 256`, aborts on mismatch or
if neither tool exists (message names both). Signature verification is documented as
`pulse-recall update` territory. Also: use the `releases/latest/download/<asset>` redirect
instead of parsing the API JSON with `sed` (no unauthenticated API call, no rate limit).

### 4. CI

- `cargo audit` and `cargo deny check advisories licenses` jobs.
- macOS test job (`macos-latest`, `cargo test --no-default-features --features embedded,llm`
  to keep it under the runner budget; full suite stays on Ubuntu).
- Two Ubuntu test invocations: default features and `--features pulse-null` (front-door AC3).
- docs.rs: `[package.metadata.docs.rs] no-default-features = true, features = ["llm"]`
  and `ort` build gated so the docs build never downloads binaries; verified by
  `cargo doc --no-default-features --features llm` in CI.

### 5. Distribution

- `[package.metadata.binstall]` with `pkg-url` template matching the asset names, so
  `cargo binstall pulse-recall` works from the first release.
- Homebrew tap `dnacenta/homebrew-tap` with `Formula/pulse-recall.rb` (bottle-less, URL +
  sha256 per platform). `release.yml` opens a PR against the tap with the new version and
  digests. `brew install dnacenta/tap/pulse-recall`.
- crates.io: `homepage` and `documentation` fields set; publish `pulse-recall` 5.0.0 as a
  manual step after the tag's CI is green.

### 6. Cut v5.0.0

`Cargo.toml` already at 5.0.0 from the rename. Tag on the merge of the last v5 PR; the
launch-surface spec's CHANGELOG entry must exist first.

### 7. VPS migration (post-release, by Vigil)

`pulse-recall update` is not available from a 4.x binary named `recall-echo` on this box…
it is: the legacy asset makes `recall-echo update` install the 5.0.0 binary in place at
`/usr/local/bin/recall-echo`. Then: copy to `/usr/local/bin/pulse-recall`, turn
`recall-echo` into a symlink (the sudoers-pinned `recall-deploy` wrapper keeps working),
`pulse-recall init` as root (`/root/.claude`) and as pulse (`/home/pulse/entity`), restart
`echo.service` only after the pulse-null follow-up (dependency rename) is merged and built.

---

## Acceptance criteria

- AC1: A `v5.0.0-rc.1` pre-release tag produces 8 tarballs, `SHA256SUMS`, `SHA256SUMS.minisig`, and provenance attestations (checked with `gh release view` and `gh attestation verify`).
- AC2: `update` against that release verifies signature and digest before extraction (test: mock server serving a tampered asset → abort before any file is executed; tampered `.minisig` → abort before download).
- AC3: `update --version v4.3.0` refuses with the "unsigned release" message.
- AC4: `install.sh` with a tampered `SHA256SUMS` aborts; with neither sha tool present aborts naming both.
- AC5: CI is green on Ubuntu (both feature sets), macOS, audit, deny, and `cargo doc` job.
- AC6: docs.rs builds 5.0.0 (checked after publish).
- AC7: `cargo binstall pulse-recall` on a clean Linux box installs the release binary.
- AC8: `brew install dnacenta/tap/pulse-recall` on macOS installs 5.0.0 (D verifies on the Mac).
- AC9: On the VPS, `recall-echo update` from 4.3.0 installs 5.0.0 and `recall-echo --version` prints `pulse-recall 5.0.0` plus the migration line.

## Out of scope

Windows builds. musl builds (follow-up issue; NixOS users can `cargo install`). Sigstore
keyless signing (attestation covers provenance; minisign covers offline verification).

## Delivery

Branch `feat/RE-<n>-release-integrity`. Merged last before the tag. The minisign keypair is
generated by D on the Mac (or by Vigil on the VPS with the secret key handed to D and
deleted from the box) — never committed.
