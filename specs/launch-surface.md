# Spec — Launch surface: README, docs, site, demo, policies, listings

**Status:** draft
**Target version:** 5.0.0 (docs ship with the release; listings after)
**Drafted:** 2026-09-02
**Baseline:** on top of the v5 code specs

---

## Goal

A developer who lands on the repo, the crate, the docs site or the pulse-null.com page
understands in ten seconds what pulse-recall does, installs it in two commands, finds the
answer to every first-week question without opening an issue, and can see what leaves their
machine.

## Why this matters

Audit 2026-09-02: README is 815 lines, of which 750 are an internal design document; no
demo; `docs/index.html` is two majors stale and GitHub Pages is not enabled; GitHub
description still says "three-layer" and "cargo install"; no CHANGELOG, SECURITY.md,
uninstall page, privacy statement, per-CLI quickstart or troubleshooting; the docs.rs build
is broken; 0 stars. Marketing assets scored 0/5.

---

## Design

### 1. README (≤ 180 lines)

Order: one-line problem → two-command install → 30-second demo (gif) → "what makes it
different" (five bullets, mechanism-led: confidence + provenance, cross-CLI, zero marginal
cost, local and redacted, MCP) → "compared with your CLI's built-in memory" (five-row
table: scope, retrieval, cross-machine, cross-CLI, corrections) → the five day-to-day
commands → links to docs → license. The benchmark gets one honest sentence with a link,
not a headline number.

### 2. Docs (`docs/`, served by GitHub Pages from `docs/`)

| Page | Content |
| --- | --- |
| `index.html` | landing: same copy as README top, install, demo, links; replaces the stale page |
| `quickstart/claude-code.md`, `codex.md`, `gemini.md`, `grok.md`, `cursor.md`, `zed.md` | one page each: install, init, what got registered, first query, how to check it works |
| `configuration.md` | every key of `.pulse-recall.toml` with default and effect (generated from a `config --schema` dump to stay honest) |
| `architecture.md` | the current README design section, moved verbatim then trimmed |
| `bayesian-confidence.md` | exists |
| `data-and-privacy.md` | what is captured (per source), what is redacted and how, what leaves the machine and to whom under each provider, where data lives, sizes, how to export/forget/uninstall, zero telemetry |
| `troubleshooting.md` | daemon will not start, store locked, model download offline, Gemini hook stdout, "no memory here", Gatekeeper on macOS, glibc on Alpine/NixOS |
| `uninstall.md` | the command and what it leaves |
| `pulse-null.md` | the plugin section moved out of README |
| `benchmarks/` | exists |

Plain Markdown rendered by Pages' default Jekyll, no site generator to maintain.

### 3. pulse-null.com

A `/recall` page on the existing landing (lab box, `/docker/pulse-null-landing`): headline,
install, demo, three differentiators, link to the Pages docs and the repo. Same visual
system as the landing. Deployed by Vigil over `ssh blog-root`.

### 4. Demo

Scripted asciinema recording on the VPS: `init` → a short fake session captured → `status`
→ `what-do-you-know --about <topic>` → an MCP query from inside `claude -p`. Rendered to
gif with `agg`, ≤ 3 MB, committed under `docs/assets/`. The script is checked in so the
demo can be re-recorded per release.

### 5. Policies and hygiene

- `CHANGELOG.md` (Keep a Changelog), seeded from tags v3.11.0 → v4.3.0 with one line each and
  a full 5.0.0 section (breaking changes first, with the migration note).
- `SECURITY.md`: report address, 90-day disclosure, supported versions (latest minor).
- `CONTRIBUTING.md`: DCO sign-off replaces the informal relicense clause; commit
  convention; how to run the suite.
- `SUPPORT.md`: issues for bugs, discussions for questions, versioning policy (semver,
  breaking only on majors, one-major compat for renamed surfaces).
- GitHub: description, homepage (Pages URL), topics; crates.io `homepage` and
  `documentation`.

### 6. Listings (after the tag)

- Claude Code plugin: `.claude-plugin/plugin.json` with the three hooks and the MCP server,
  submitted to `claude-plugins-official/external_plugins` via the directory form.
- MCP registry: `server.json` (`io.github.dnacenta/pulse-recall`), package type `mcpb`
  pointing at the release tarball with `fileSha256`; if the registry requires "mcp" in the
  asset URL, `release.yml` additionally uploads `pulse-recall-mcp-<target>.tar.gz`.
- `awesome-mcp-servers` PR; `awesome-claude-code` issue (bot-only PRs); `awesome-rust` PR.
- Launch copy (Show HN title + first comment, r/rust post, X thread) lives in
  `/opt/pulse-vault/pulse-recall/launch/`, not in the repo.

---

## Acceptance criteria

- AC1: `wc -l README.md` ≤ 180; the demo gif renders on GitHub; no mention of pulse-null, vigil, LEARNING.md or THOUGHTS.md remains in README.
- AC2: GitHub Pages serves `docs/` at the repo's Pages URL; every page in §2 exists and every internal link resolves (link checker in CI, `lychee` or equivalent, on `docs/**`).
- AC3: `configuration.md` lists every key that `config --schema` prints (test compares the two).
- AC4: `data-and-privacy.md` names, per provider preset, the exact host chunks are sent to.
- AC5: `CHANGELOG.md` has a `## [5.0.0]` section whose "Breaking" list matches the hidden-alias and rename tables in the specs.
- AC6: `SECURITY.md`, `SUPPORT.md`, DCO paragraph present; GitHub repo description and homepage updated (checked via `gh api`).
- AC7: pulse-null.com/recall is live over HTTPS and links to the Pages docs.
- AC8: The demo script re-runs cleanly on the VPS and produces a gif under 3 MB.
- AC9: Plugin manifest validates with `claude plugin validate` (or the current equivalent); `server.json` validates against the registry schema.

## Out of scope

Paid SKU pages, pricing, account system (Sync spec, separate repo). Blog posts beyond the
launch copy drafts.

## Delivery

Branch `docs/RE-<n>-launch-surface`. Docs merge before the tag; listings and pulse-null.com
after the tag is public.
