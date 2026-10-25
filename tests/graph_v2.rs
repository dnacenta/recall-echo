// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Alias rows are store data. Closing the process and opening the same
//! directory again must still find them, and writing the same pair again
//! must not fail.

use recall_echo::graph::crud;
use recall_echo::graph::store;

#[tokio::test]
async fn alias_persists_across_sessions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("graph");
    {
        let db = store::open(&path).await.expect("open");
        store::init_schema(&db).await.expect("schema");
        db.query(
            "CREATE entity:pg SET name = 'Postgres', entity_type = 'tool', abstract = 'db', overview = ''",
        )
        .await
        .expect("create")
        .check()
        .expect("create check");
        crud::put_alias(&db, "postgresql", "entity:pg")
            .await
            .expect("put alias");
        let stored = crud::get_alias(&db, "postgresql")
            .await
            .expect("get alias")
            .expect("row");
        assert!(stored == "entity:pg" || stored.ends_with(":pg"), "{stored}");
        crud::put_alias(&db, "postgresql", "entity:pg")
            .await
            .expect("retry put");
        assert_eq!(alias_count(&db).await, 1);
    }

    let db = store::open(&path).await.expect("reopen");
    store::init_schema(&db).await.expect("schema again");
    let stored = crud::get_alias(&db, "postgresql")
        .await
        .expect("get after reopen")
        .expect("alias survived");
    assert!(stored == "entity:pg" || stored.ends_with(":pg"), "{stored}");
    crud::put_alias(&db, "postgresql", "entity:pg")
        .await
        .expect("put after reopen");
    assert_eq!(alias_count(&db).await, 1);
}

async fn alias_count(db: &surrealdb::Surreal<store::Db>) -> usize {
    let mut response = db
        .query("SELECT name_folded FROM alias")
        .await
        .expect("count");
    let rows: Vec<serde_json::Value> = response.take(0).expect("rows");
    rows.len()
}
