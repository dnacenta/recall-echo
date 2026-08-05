// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The embedded store's process-exclusive lock, exercised for real.
//!
//! Everything the daemon exists for rests on one behavior: a second opener of
//! an embedded store fails with [`GraphError::Locked`] rather than corrupting
//! it or hanging. That detection is a match on SurrealKV's error *text*, so a
//! dependency reword would silently turn the retry path into a raw `Db` error
//! and the daemon hand-off into a hard failure. This test is the tripwire.

use recall_echo::graph::error::GraphError;
use recall_echo::graph::store;
use recall_echo::graph::GraphMemory;
use tempfile::TempDir;

#[tokio::test]
async fn a_second_opener_of_an_embedded_store_gets_a_named_lock_error() {
    let dir = TempDir::new().expect("temp dir");
    let held = GraphMemory::open_embedded(dir.path())
        .await
        .expect("first open owns the store");

    let Err(err) = GraphMemory::open_embedded(dir.path()).await else {
        panic!("a second opener must be refused while the store is held");
    };
    assert!(
        matches!(err, GraphError::Locked(_)),
        "expected GraphError::Locked, got {err:?} — SurrealKV's lock message \
         probably changed, and store::is_lock_error no longer recognizes it"
    );
    assert!(
        err.to_string().contains("locked by another process"),
        "{err}"
    );

    drop(held);
}

/// Once the owner lets go, the store is openable again — the retry loop in
/// `store::open` is waiting for exactly this.
#[tokio::test]
async fn the_store_reopens_after_the_owner_drops_it() {
    let dir = TempDir::new().expect("temp dir");
    let held = store::open(dir.path()).await.expect("first open");
    drop(held);

    store::open(dir.path()).await.expect("reopen after release");
}
