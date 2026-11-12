use phab_core::Phid;
use phab_db::edges;
use phab_db::schema::CONTRACT;
use phab_db::testing::TestDatabase;

#[tokio::test]
async fn schema_contract_matches_quickstart() {
    let mut applications: Vec<&str> = CONTRACT.iter().map(|table| table.database).collect();
    applications.sort_unstable();
    applications.dedup();
    let Some(test_db) = TestDatabase::create(&applications).await else {
        return;
    };
    let database = test_db.database();
    let mut missing = Vec::new();
    for contract in CONTRACT {
        let schema = format!("{}_{}", database.namespace(), contract.database);
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT CAST(COLUMN_NAME AS CHAR) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        )
        .bind(&schema)
        .bind(contract.table)
        .fetch_all(database.pool())
        .await
        .unwrap();
        for column in contract.columns {
            if !columns.iter().any(|existing| existing == column) {
                missing.push(format!("{}.{}.{column}", contract.database, contract.table));
            }
        }
    }
    test_db.drop_databases().await;
    assert!(missing.is_empty(), "missing columns: {missing:?}");
}

#[tokio::test]
async fn reads_config_entries_and_edges() {
    let Some(test_db) = TestDatabase::create(&["config", "user", "project"]).await else {
        return;
    };
    test_db
        .execute(
            "INSERT INTO `{ns}_config`.config_entry \
             (phid, namespace, configKey, value, isDeleted, dateCreated, dateModified) VALUES \
             ('PHID-CONF-1', 'default', 'policy.allow-public', 'true', 0, 1, 1), \
             ('PHID-CONF-2', 'default', 'phabricator.base-uri', '\"http://x.test/\"', 1, 1, 1), \
             ('PHID-CONF-3', 'other', 'mysql.host', '\"elsewhere\"', 0, 1, 1)",
        )
        .await;
    test_db
        .execute(
            "INSERT INTO `{ns}_user`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-USER-alice', 22, 'PHID-DREV-1', 10, 0), \
             ('PHID-USER-alice', 22, 'PHID-DREV-2', 20, 0), \
             ('PHID-USER-alice', 14, 'PHID-PROJ-1', 20, 0); \
             INSERT INTO `{ns}_project`.edge (src, type, dst, dateCreated, seq) VALUES \
             ('PHID-PROJ-1', 22, 'PHID-DREV-3', 5, 0)",
        )
        .await;

    let database = test_db.database();
    let entries = database.load_config_entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries["policy.allow-public"], serde_json::json!(true));

    let sources = [Phid::new("PHID-USER-alice"), Phid::new("PHID-PROJ-1")];
    let subscribed =
        edges::load_destination_phids(database, &sources, &[edges::SUBSCRIBED_TO_OBJECT], None)
            .await
            .unwrap();
    assert_eq!(
        subscribed,
        [
            Phid::new("PHID-DREV-2"),
            Phid::new("PHID-DREV-1"),
            Phid::new("PHID-DREV-3")
        ]
    );

    let only = [Phid::new("PHID-DREV-1")];
    let filtered = edges::load_destination_phids(
        database,
        &sources,
        &[edges::SUBSCRIBED_TO_OBJECT],
        Some(&only),
    )
    .await
    .unwrap();
    assert_eq!(filtered, only);

    let unknown = edges::load_edges(database, &[Phid::new("PHID-ZZZZ-1")], &[1], None).await;
    assert!(unknown.is_err());

    test_db.drop_databases().await;
}
