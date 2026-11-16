# Advisory triage

Recorded 2026-11-16 against `Cargo.lock` SHA-256
`97863af872bb1b9a548fe979a0544972df989d5aa9f61968c5a2492b0d2d850e`.
That is the hash of this lock. The open tables below are a scan of this file.
The previous record, on main `f8743d9` (commit `21edd0f`), hashed
`70695675c4134e66b73e563aee2ec73064f29a3353401b13988d4ce042c90071`.

Scan source:

- OSV `querybatch` on 2026-11-16 for the 639 locked package versions. The
  semver-compatible ids from the previous record are absent. The one GHSA
  alias still returned is named on its RustSec row: `GHSA-4w2j-m93h-cj5j`
  (`RUSTSEC-2026-0185`).
- `cargo audit` 0.23.0 against RustSec advisory-db
  `02e2517f457a359fc6d3440ea4560c8ea188fee8` (committed 2026-11-15), the same
  snapshot as the previous record. It reports the same 19 ids. It also warns
  that `core2` 0.4.0 and `spin` 0.9.8 are yanked. `core2` is the unmaintained
  row below. `spin` 0.9.8 has no advisory id. `lazy_static` 1.5.0 and
  `heapless` 0.7.17 depend on it, and the version is unchanged from the
  previous lock.

19 advisories. Each one is listed once. A GHSA that is only an alias is named
on the RustSec row, and `audit.toml` ignores the RustSec id. `webpki-roots`
0.26.11 (ureq's tls feature) is in the lock and has no open advisory. The 1.0
line is 1.0.12. `tar` 0.4.46 is past `GHSA-3pv8-6f4r-ffg2` and past
`RUSTSEC-2018-0002`, `RUSTSEC-2021-0080`, `RUSTSEC-2026-0067` and
`RUSTSEC-2026-0068` (each patched in `>= 0.4.45`).

`RUSTSEC-2026-0452`, `RUSTSEC-2026-0453` and `RUSTSEC-2026-0454` come first.
They are open and pending a later decision. The embedded path can reach them.
This record does not upgrade the store and does not add a CI job. An automated
advisory scan is tracked in #56.

## Open: store majors, embedded path reachable

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2026-0452 | surrealkv | 0.21.3 | Default feature `embedded` turns on `surrealdb/kv-surrealkv`. Embedded open, read, scan and write decode SurrealKV SSTables. | Yes. `InternalKeyRef::from_encoded` checks the 16-byte minimum only with `debug_assert`. In a release build `user_key`, `timestamp` and `read_u64_be` then run off the front of a short key. `BlockIterator::key()` feeds that path. | OPEN. Store major pending a later decision. Embedded path reachable. Fix is surrealkv >= 1.2.0. |
| RUSTSEC-2026-0453 | surrealdb | 3.2.4 | Direct dependency, pinned to the 3.2 series because the on-disk format is tied to it. Default feature `embedded` selects the `kv-surrealkv` backend. | Yes. That backend's `count`, scans, point reads and writes move borrowed state through `affinitypool::spawn_local`. Leaking the returned future is a use-after-free on the transaction. | OPEN. Store major pending a later decision. Embedded path reachable. Fix is surrealdb >= 3.3.0. The 3.2 pin stays until that decision. |
| RUSTSEC-2026-0454 | surrealdb-core | 3.2.4 | `surrealdb` 3.2.4 depends on `surrealdb-core` `^3.2.4`, which depends on `reblessive` `^0.4.3` with the `tree` feature. | Yes. `dbs::Executor` runs every embedded SurrealQL statement on `reblessive::tree::TreeStack`. Those types implement `Send` and `Sync` unsoundly, so a duplicated driver or a `!Send` capture can race and double-free. | OPEN. Store major pending a later decision. Embedded path reachable. Fix is surrealdb-core >= 4.0.0. |

## Other majors

A fix here changes the major version. It is not a lockfile edit and it is not
the semver PR. The risk is not accepted.

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2023-0071 | rsa | 0.9.10 | `surrealdb-core` → `jsonwebtoken` 10.3.0 → `rsa`. | The Marvin leak is timing of RSA private-key operations. Server mode signs in with a database username and password. This crate does not perform RSA private-key decryption. The code is compiled into the store's JWT stack. | Major bump to rsa >= 0.10.0. Not a lockfile edit. Pending a later decision. |
| RUSTSEC-2026-0235 | rkyv | 0.7.46 | Optional `rkyv` feature of `rust_decimal` 1.42.1 (`^0.7.46`). `surrealdb-core` enables `maths` and `serde-str` on `rust_decimal`, not `rkyv`. | No. The archive validator is not in the default compile graph. The bug is an out-of-bounds read when an archive's `Rc` or `Arc` pointers share an address and differ in metadata. | Major bump to rkyv >= 0.8.17. Not a lockfile edit. Pending a later decision. |
| RUSTSEC-2026-0387 | crypto-bigint | 0.5.5 | `jsonwebtoken` → `p256` / `p384` → `elliptic-curve` 0.13.8 → `crypto-bigint` 0.5.5. | The variable-time paths are subtraction, division, zero-count and Montgomery multiplication that are meant to be constant-time. This crate does not run those secret-dependent operations. They are compiled into the store's JWT stack. | Major bump to crypto-bigint >= 0.7.6. Not a lockfile edit. Pending a later decision. |

## Fixed by this PR

These ids were in the previous record. This lock bumps each package inside the
requirement already in `Cargo.toml`, and the rescan does not report them.
`audit.toml` no longer ignores them. `webpki-roots` 0.26.11 and `rand` 0.9.5
stayed on their existing lines. `surrealdb` 3.2.4, `surrealdb-core` 3.2.4,
`surrealkv` 0.21.3, `rsa` 0.9.10, `rkyv` 0.7.46 and `crypto-bigint` 0.5.5 did
not move.

`ammonia` 4.2.5 is inside `surrealdb-core` 3.2.4's `^4.1.2`. That bump carries
the HTML stack it requires: `cssparser` 0.35.0 to 0.38.0, `html5ever` 0.35.0
to 0.40.1, `markup5ever` 0.35.0 to 0.40.0, `phf` 0.11.3 to 0.14.0 (0.13.1
stays), `phf_codegen` 0.11.3 to 0.14.0, `phf_generator` 0.11.3 to 0.14.0,
`phf_shared` 0.11.3 to 0.14.0 (0.13.1 stays), `string_cache` 0.8.9 to 0.11.0,
`string_cache_codegen` 0.5.4 to 0.11.2, `tendril` 0.4.3 to 0.5.1 and
`web_atoms` 0.1.3 to 0.3.0. `cssparser-macros` 0.6.1, `futf` 0.1.5, `mac`
0.1.1, `match_token` 0.35.0 and `phf_macros` 0.11.3 left the lock with the old
parser. `phf_macros` 0.13.1 stays.

| Package | Was | Now | IDs fixed by this PR |
|---|---|---|---|
| rustls | 0.23.37 | 0.23.46 | RUSTSEC-2026-0285 (GHSA-2mjx-qc3c-rqvc), RUSTSEC-2026-0346 (GHSA-c8m5-c952-6cm4), RUSTSEC-2026-0348 (GHSA-h2v8-394m-p7vv), RUSTSEC-2026-0349 (GHSA-w44r-93j7-qv28) |
| rustls-webpki | 0.103.10 | 0.103.17 | RUSTSEC-2026-0098 (GHSA-965h-392x-2mh5), RUSTSEC-2026-0099 (GHSA-xgp8-3hg3-c2mh), RUSTSEC-2026-0104 (GHSA-82j2-j2ch-gfr8), RUSTSEC-2026-0382 |
| webpki-roots | 1.0.6 | 1.0.12 | RUSTSEC-2026-0381 |
| h2 | 0.4.13 | 0.4.20 | RUSTSEC-2026-0258 |
| tar | 0.4.45 | 0.4.46 | GHSA-3pv8-6f4r-ffg2 |
| tokio | 1.53.1 | 1.54.1 | RUSTSEC-2026-0386 |
| crossbeam-epoch | 0.9.18 | 0.9.21 | RUSTSEC-2026-0204 |
| anyhow | 1.0.102 | 1.0.104 | RUSTSEC-2026-0190 |
| event-listener | 5.4.1 | 5.4.2 | RUSTSEC-2026-0221 |
| rand | 0.8.5 | 0.8.8 | RUSTSEC-2026-0097 (GHSA-cq8v-f236-94qc) |
| rustix | 1.1.4 | 1.1.6 | RUSTSEC-2026-0486 |
| ammonia | 4.1.2 | 4.2.5 | RUSTSEC-2021-0157, RUSTSEC-2021-0158, RUSTSEC-2026-0193 (GHSA-9jh8-v38h-cvhr), RUSTSEC-2026-0213 (GHSA-m6mh-2hw2-555x), RUSTSEC-2026-0373, RUSTSEC-2026-0533 |

## In the lock, HTTP/3 off

`quinn` is an optional dependency of `reqwest` feature `http3`. Our `reqwest`
features are `json`, `rustls-tls`, `charset` and `http2`. `quinn-proto` and
`quinn-udp` are in the lock and are not in the default compile graph. The
weak functions are QUIC receive and send paths, so this build cannot call them.

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2026-0185 (GHSA-4w2j-m93h-cj5j) | quinn-proto | 0.11.14 | `reqwest` feature `http3` → `quinn` → `quinn-proto`. That feature is off. | No. Out-of-order stream reassembly can grow without a bound when a peer leaves gaps. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off, not because the id was missed. Patched in quinn-proto >= 0.11.15. |
| RUSTSEC-2026-0338 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. A zero-length STREAM frame during an unordered read panics. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.18. |
| RUSTSEC-2026-0339 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. Already-retired connection ids can grow the retire queue without the usual limit. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.17. |
| RUSTSEC-2026-0340 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. With ACK_FREQUENCY sending enabled, a peer minimum ACK delay can panic. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.18. |
| RUSTSEC-2026-0342 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. A zero-length DATAGRAM bypasses the receive-buffer limit. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.17. |
| RUSTSEC-2026-0523 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. A MAX_STREAM_DATA frame can open streams past the advertised limit. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.19. |
| RUSTSEC-2026-0524 | quinn-proto | 0.11.14 | Same `quinn-proto` 0.11.14, HTTP/3 off. | No. A long Retry token can panic a debug build or try to allocate more than 10 GiB in a release client. The QUIC stack is not compiled. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.11.19. |
| RUSTSEC-2026-0538 | quinn-udp | 0.5.14 | `reqwest` feature `http3` → `quinn` → `quinn-udp`. That feature is off. | No. On targets that read one datagram per `recvmsg`, a truncated datagram can leave the receive loop spinning. Linux uses `recvmmsg`. The QUIC stack is not compiled either way. | HTTP/3 is off. Ignored because the feature is off. Patched in >= 0.5.16 and < 0.6.0, or >= 0.6.3. |

## Unmaintained or yanked, no patched version

No patched release exists. The advisory names no weak function beyond the
crate being unmaintained. `core2` is also fully yanked. `spin` 0.9.8 is
yanked and has no advisory id, so `audit.toml` has nothing to ignore.
`lazy_static` 1.5.0 and `heapless` 0.7.17 depend on it. The version is the
same as the previous lock.

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2026-0105 | core2 | 0.4.0 | Default graph: `fastembed` → `image` (default features; `default-formats` includes `avif`) → `ravif` → `rav1e` → `bitstream-io` → `core2`. | The advisory names no weak function. The crate is unmaintained and every published version is yanked. We embed text only (`TextEmbedding` in `src/graph/embed.rs`) and never decode or encode AVIF, so our calls do not enter the AVIF codec that compiles `core2`. | No patched version. |
| RUSTSEC-2024-0436 | paste | 1.0.15 | `fastembed` → `tokenizers` → `paste`, and `macro_rules_attribute` → `paste`. `rav1e` is on the default image AVIF path above and also depends on `paste`. | No vulnerable function is named. The crate is unmaintained. Tokenizers uses it as a compile-time helper. | No patched version. |
| RUSTSEC-2025-0119 | number_prefix | 0.4.0 | fastembed → `hf-hub`. The `ureq` and `tokio` features of hf-hub both pull `indicatif`, which depends on `number_prefix`. | No vulnerable function is named. The crate is unmaintained. It formats the numeric prefix on the download progress bar. | No patched version. |
| RUSTSEC-2023-0089 | atomic-polyfill | 1.0.3 | `heapless` 0.7.17, which rstar 0.9.3, 0.10.0 and 0.11.0 list. Those versions are recorded on `geo-types`. The store compiles `geo` 0.32.0, which uses rstar 0.12.2 and `heapless` 0.8.0. The default graph does not compile atomic-polyfill. | No vulnerable function is named. The crate is unmaintained. | No patched version. |
| RUSTSEC-2026-0441 | pin-utils | 0.1.0 | `wasmtimer`, a dependency of `surrealdb` and `surrealdb-core` under `cfg(target_family = "wasm")`. The default native graph does not compile it. | No vulnerable function is named. The crate is unmaintained. | No patched version. |
