# Advisory triage

Recorded 2026-11-16 against `Cargo.lock` SHA-256
`70695675c4134e66b73e563aee2ec73064f29a3353401b13988d4ce042c90071`
on main `f8743d9`. That is the hash of this lock. The table below is a scan of
this file, not a copy of an older lock.

Scan source:

- RustSec advisory-db `02e2517f457a359fc6d3440ea4560c8ea188fee8` (committed
  2026-11-15). Every locked crate name and version was matched against
  non-withdrawn advisories. A version is affected when it matches neither a
  `patched` nor an `unaffected` requirement.
- OSV `querybatch` on 2026-11-16 for the same 644 locked package versions.
  OSV returned those RustSec advisories, GHSA aliases of eleven of them, and
  one id that this RustSec snapshot does not contain: `GHSA-3pv8-6f4r-ffg2`
  (`tar` 0.4.45, fixed in 0.4.46).

42 advisories. Each one is listed once. A GHSA that is only an alias is named
on the RustSec row, and `audit.toml` ignores the RustSec id. `audit.toml`
ignores `GHSA-3pv8-6f4r-ffg2` on its own row. `webpki-roots` 0.26.11 (ureq's
tls feature) is in the lock and has no open advisory. `tar` 0.4.45 is already
past `RUSTSEC-2018-0002`, `RUSTSEC-2021-0080`, `RUSTSEC-2026-0067` and
`RUSTSEC-2026-0068` (each patched in `>= 0.4.45`).

`RUSTSEC-2026-0452`, `RUSTSEC-2026-0453` and `RUSTSEC-2026-0454` come first.
They are open and pending a later decision. The embedded path can reach them.
This record does not upgrade the store, does not edit `Cargo.lock`, and does
not add a CI job. An automated advisory scan is tracked in #56.

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

## Semver-compatible fixes, next PR

The bump is the next PR. Ignoring these ids records the review. It does not
accept the risk for good. `surrealdb-core` 3.2.4 depends on `ammonia` `^4.1.2`,
which allows >= 4.1.9 and >= 4.2.5, so ammonia is in this group.

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2026-0285 (GHSA-2mjx-qc3c-rqvc) | rustls | 0.23.37 | `reqwest` feature `rustls-tls` (default features `llm` and `self-update`) and fastembed feature `hf-hub-rustls-tls`. | Yes. A TLS 1.3 peer can place a handshake message in the wrong encryption level inside the same record as a key-changing message. Our HTTPS clients accept that record. | Bump rustls to >= 0.23.46 in the next PR. This id is fixed in >= 0.23.45. |
| RUSTSEC-2026-0346 (GHSA-c8m5-c952-6cm4) | rustls | 0.23.37 | Same rustls 0.23.37 stack as RUSTSEC-2026-0285. | Only if client session values are debug-formatted. `Debug` on `Tls12ClientSessionValue` and `Tls13ClientSessionValue` prints the master secret or the resumption PSK. This crate does not format those values. | Bump rustls to >= 0.23.46 in the next PR. |
| RUSTSEC-2026-0348 (GHSA-h2v8-394m-p7vv) | rustls | 0.23.37 | Same rustls 0.23.37 stack as RUSTSEC-2026-0285. | No. Brotli certificate decompression is rustls feature `brotli`, which this stack leaves off, so the decompressor is not compiled. | Bump rustls to >= 0.23.46 in the next PR. |
| RUSTSEC-2026-0349 (GHSA-w44r-93j7-qv28) | rustls | 0.23.37 | Same rustls 0.23.37 stack as RUSTSEC-2026-0285. | Yes. The client stores the server-selected key-exchange group from the ServerHello before the server is authenticated, and a later ClientHello to that name can drop the hybrid share. | Bump rustls to >= 0.23.46 in the next PR. |
| RUSTSEC-2026-0098 (GHSA-965h-392x-2mh5) | rustls-webpki | 0.103.10 | `rustls` 0.23.37 uses `rustls-webpki` for certificate verification on the HTTPS paths above. | Yes, when a verified chain carries a URI name constraint. Those constraints were ignored and therefore accepted. | Bump rustls-webpki to >= 0.103.16 in the next PR. This id is fixed in >= 0.103.12. |
| RUSTSEC-2026-0099 (GHSA-xgp8-3hg3-c2mh) | rustls-webpki | 0.103.10 | Same `rustls-webpki` 0.103.10 as RUSTSEC-2026-0098. | Yes, when a verified chain combines a permitted DNS name constraint with a wildcard name. That pair was accepted. | Bump rustls-webpki to >= 0.103.16 in the next PR. This id is fixed in >= 0.103.12. |
| RUSTSEC-2026-0104 (GHSA-82j2-j2ch-gfr8) | rustls-webpki | 0.103.10 | Same `rustls-webpki` 0.103.10 as RUSTSEC-2026-0098. | Only if a certificate revocation list is parsed. An empty `BIT STRING` in `onlySomeReasons` panics in `BorrowedCertRevocationList::from_der` and `OwnedCertRevocationList::from_der`. Our HTTP client does not configure a CRL. | Bump rustls-webpki to >= 0.103.16 in the next PR. This id is fixed in >= 0.103.13. |
| RUSTSEC-2026-0382 | rustls-webpki | 0.103.10 | Same `rustls-webpki` 0.103.10 as RUSTSEC-2026-0098, with trust anchors from `webpki-roots` 1.0.6. | Yes. Verification can accept a path through distrusted emSign intermediates when the configured roots allow it, and 1.0.6 still bundles `emSign ECC Root CA - G3`. | Bump rustls-webpki to >= 0.103.16 in the next PR, together with the webpki-roots bump below. |
| RUSTSEC-2026-0381 | webpki-roots | 1.0.6 | `reqwest` feature `rustls-tls` enables `rustls-tls-webpki-roots`, which depends on `webpki-roots` 1.0.6. ureq's tls feature uses the separate 0.26.11 line, which is already fixed. | Yes. The bundled roots include `emSign ECC Root CA - G3`, so an HTTPS connection can trust a chain through the distrusted intermediates. | Bump the 1.0 line to webpki-roots >= 1.0.10 in the next PR. |
| RUSTSEC-2026-0258 | h2 | 0.4.13 | `reqwest` feature `http2` (enabled on our dependency, and again by hf-hub) → `hyper` → `h2`. | Yes. A peer can queue empty DATA frames without a bound while a stream is not drained, which grows memory or panics on overflow. | Bump h2 to >= 0.4.16 in the next PR. |
| GHSA-3pv8-6f4r-ffg2 | tar | 0.4.45 | Direct dependency of the default `self-update` feature. `extract_binary` in `src/update.rs` parses the release archive with `Archive::entries`. Also a build dependency of `ort-sys` feature `download-binaries`, which fastembed's default `ort-download-binaries` turns on. | Yes, on a crafted archive. `extract_binary` copies only a regular file named `recall-echo` and writes it with our own exclusive-create. It does not call `unpack` or `unpack_in`. A PAX header is applied to the next header instead of the file, so `entries()` can desynchronize and yield the wrong bytes under that name. The name filter limits the older unpack bugs and leaves this parser bug in place. | Bump tar to >= 0.4.46 in the next PR. |
| RUSTSEC-2026-0386 | tokio | 1.53.1 | Direct dependency. Enabled features include `net`. | The unsound constructors are `AsyncFd::new`, `with_interest`, `try_new` and `try_with_interest`, which accept any `AsRawFd`. This crate does not call them. | Bump tokio to >= 1.53.3 in the next PR. |
| RUSTSEC-2026-0204 | crossbeam-epoch | 0.9.18 | `crossbeam-deque` (rayon-core and affinitypool) and `crossbeam-skiplist` (surrealkv). | `Display` for `Atomic` and `Shared` dereferences the pointer, including a null pointer. `Debug` does not. This crate does not display those pointers. | Bump crossbeam-epoch to >= 0.9.20 in the next PR. |
| RUSTSEC-2026-0190 | anyhow | 1.0.102 | `surrealdb` and `surrealdb-core` depend on anyhow `^1.0.102`. | `Error::downcast_mut` after `Error::context` is unsound. This crate uses `thiserror` and does not call `downcast_mut`. | Bump anyhow to >= 1.0.103 in the next PR. |
| RUSTSEC-2026-0221 | event-listener | 5.4.1 | `surrealdb-core` → `async-channel` → `event-listener-strategy` → `event-listener`. | `StackSlot` is unconditionally `Send` and `Sync`, so a `!Send` tag can cross threads through the `listener!` macro. This crate does not use that macro. The store's async channels compile it. | Bump event-listener to >= 5.4.2 in the next PR. |
| RUSTSEC-2026-0097 (GHSA-cq8v-f236-94qc) | rand | 0.8.5 | `jsonwebtoken` 10.3.0 depends on rand 0.8.5. The lock also carries rand 0.9.5, which is outside this advisory. | Only when `thread_rng` is enabled, a custom logger calls `thread_rng` while the RNG reseeds, and trace logging is on or `getrandom` cannot reseed. This crate does not install that logger. | Bump the 0.8 line to rand >= 0.8.6 in the next PR. |
| RUSTSEC-2026-0486 | rustix | 1.1.4 | `tempfile` depends on rustix. The `xattr` crate, which `tar` also lists, calls the same module. Our direct `tar` dependency turns default features off, so that unpack path does not enable tar's xattr feature. | `getxattr`, `lgetxattr`, `fgetxattr`, `listxattr`, `llistxattr` and `flistxattr` mark a zero-length buffer initialized for as many bytes as the kernel's size query reports. This crate does not call them. | Bump rustix to >= 1.1.6 in the next PR. |
| RUSTSEC-2021-0157 | ammonia | 4.1.2 | `surrealdb-core` 3.2.4 depends on ammonia `^4.1.2`. The only call sites are `string::html::encode` (`ammonia::clean_text`) and `string::html::sanitize` (`ammonia::clean`). | No. Ingest and query do not call those HTML helpers. The bug is unescaped quotes in CSS `url()`, and only when style filtering is enabled. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. Both sit in `^4.1.2`. This id is fixed earlier, in >= 4.1.7. |
| RUSTSEC-2021-0158 | ammonia | 4.1.2 | Same ammonia 4.1.2 call sites as RUSTSEC-2021-0157. | No. Ingest and query do not call the HTML helpers. SVG `animateMotion`, `animateTransform` and `animateColor` were not neutralized. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. This id is fixed in >= 4.1.7. |
| RUSTSEC-2026-0193 (GHSA-9jh8-v38h-cvhr) | ammonia | 4.1.2 | Same ammonia 4.1.2 call sites as RUSTSEC-2021-0157. | No. Ingest and query do not call the HTML helpers. If the affected MathML tags are enabled, `annotation-xml` handling can inject script. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. This id is fixed in >= 4.1.3. |
| RUSTSEC-2026-0213 (GHSA-m6mh-2hw2-555x) | ammonia | 4.1.2 | Same ammonia 4.1.2 call sites as RUSTSEC-2021-0157. | No. Ingest and query do not call the HTML helpers. SVG `animate` and `set` can leave a `javascript:` link. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. This id is fixed in >= 4.1.4. |
| RUSTSEC-2026-0373 | ammonia | 4.1.2 | Same ammonia 4.1.2 call sites as RUSTSEC-2021-0157. | No. Ingest and query do not call the HTML helpers. CSS `url` filtering misses some case variants when CSS sanitization is on. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. This id is fixed in >= 4.1.8. |
| RUSTSEC-2026-0533 | ammonia | 4.1.2 | Same ammonia 4.1.2 call sites as RUSTSEC-2021-0157. | No. Ingest and query do not call the HTML helpers. Embedded SVG and MathML can be arranged so HTML and foreign XML parse differently, which is a mutation XSS. | Bump ammonia in the next PR to >= 4.1.9 and < 4.2.0, or to >= 4.2.5. This id is the one that needs 4.1.9 on the 4.1 line. 4.2.0 through 4.2.4 stay affected. |

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
crate being unmaintained. `core2` is also fully yanked.

| ID | Crate | Version | How it is reached | Can our use hit the weak function? | Action |
|---|---|---|---|---|---|
| RUSTSEC-2026-0105 | core2 | 0.4.0 | Default graph: `fastembed` → `image` (default features; `default-formats` includes `avif`) → `ravif` → `rav1e` → `bitstream-io` → `core2`. | The advisory names no weak function. The crate is unmaintained and every published version is yanked. We embed text only (`TextEmbedding` in `src/graph/embed.rs`) and never decode or encode AVIF, so our calls do not enter the AVIF codec that compiles `core2`. | No patched version. |
| RUSTSEC-2024-0436 | paste | 1.0.15 | `fastembed` → `tokenizers` → `paste`, and `macro_rules_attribute` → `paste`. `rav1e` is on the default image AVIF path above and also depends on `paste`. | No vulnerable function is named. The crate is unmaintained. Tokenizers uses it as a compile-time helper. | No patched version. |
| RUSTSEC-2025-0119 | number_prefix | 0.4.0 | fastembed → `hf-hub`. The `ureq` and `tokio` features of hf-hub both pull `indicatif`, which depends on `number_prefix`. | No vulnerable function is named. The crate is unmaintained. It formats the numeric prefix on the download progress bar. | No patched version. |
| RUSTSEC-2023-0089 | atomic-polyfill | 1.0.3 | `heapless` 0.7.17, which rstar 0.9.3, 0.10.0 and 0.11.0 list. Those versions are recorded on `geo-types`. The store compiles `geo` 0.32.0, which uses rstar 0.12.2 and `heapless` 0.8.0. The default graph does not compile atomic-polyfill. | No vulnerable function is named. The crate is unmaintained. | No patched version. |
| RUSTSEC-2026-0441 | pin-utils | 0.1.0 | `wasmtimer`, a dependency of `surrealdb` and `surrealdb-core` under `cfg(target_family = "wasm")`. The default native graph does not compile it. | No vulnerable function is named. The crate is unmaintained. | No patched version. |
