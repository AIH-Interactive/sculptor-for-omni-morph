use super::*;
use std::sync::Arc;

fn seed(storage: &Storage, model: &str) {
    storage.library_transaction(|transaction| {
        transaction.execute(
            "INSERT INTO library_models VALUES
             (?1, 'uploader', 'Example', 'Display', 'Internal', '[\"Author\"]',
              'FIGURA', 'general', '', 'PUBLIC', 1, 'ACTIVE', 1, 1, 1, 1, 1)",
            [model],
        ).map_err(sql_error)?;
        transaction.execute(
            "INSERT INTO library_revisions VALUES
             (?1, 1, 'uploader', 'avatar', 1, ?2, 100, 'NBT', 'avatar.json', 'SERVER_VERIFIED', 'example.zip')",
            params![model, "a".repeat(64)],
        ).map_err(sql_error)?;
        transaction.execute(
            "INSERT INTO library_revision_hashes VALUES (?1, 1, 'RAW_SHA256', ?2)",
            params![model, "a".repeat(64)],
        ).map_err(sql_error)?;
        Ok(())
    }).unwrap();
}

#[test]
fn manifest_save_preserves_library_and_shared_hook_is_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    seed(&storage, "model");
    let actor = Uuid::from_u128(7);
    storage
        .transaction(|manifest| {
            manifest.owners.entry(actor).or_default();
            Ok(())
        })
        .unwrap();
    assert_eq!(count(&storage, "library_models"), 1);

    let rejected: Result<()> = storage.transaction_with_library(
        |manifest| {
            manifest.owners.entry(actor).or_default().revision = 99;
            Ok(())
        },
        |transaction| {
            transaction
                .execute("UPDATE library_models SET display_name = 'uncommitted'", [])
                .map_err(sql_error)?;
            Err(ErrorCode::Conflict)
        },
    );
    assert_eq!(rejected, Err(ErrorCode::Conflict));
    assert_eq!(storage.committed.lock().unwrap().owners[&actor].revision, 0);
    let name: String = storage
        .library_read(|database| {
            database
                .query_row("SELECT display_name FROM library_models", [], |row| {
                    row.get(0)
                })
                .map_err(sql_error)
        })
        .unwrap();
    assert_eq!(name, "Display");
    drop(storage);
    let storage = Storage::open(directory.path()).unwrap();
    assert_eq!(count(&storage, "library_models"), 1);
    assert_eq!(storage.committed.lock().unwrap().owners[&actor].revision, 0);
}

fn count(storage: &Storage, table: &str) -> i64 {
    // Test-owned constant table names only; production query identifiers are fixed.
    storage
        .library_read(|database| {
            database
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(sql_error)
        })
        .unwrap()
}

#[test]
fn version_three_upgrade_preserves_policy_and_backs_up_before_management_tables() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.sqlite3");
    let storage = Storage::open(directory.path()).unwrap();
    seed(&storage, "model");
    storage.library_transaction(|transaction| {
        set_blocked(transaction, "model", Uuid::from_u128(4), 1, true, "synthetic", 2)
    }).unwrap();
    drop(storage);
    let database = Connection::open(&path).unwrap();
    database.execute_batch(
        "DROP TABLE library_collection;
         DROP VIEW library_effective_models; DROP VIEW library_hash_blocked_models;
         DROP VIEW library_active_blocked_hashes; DROP TABLE library_wardrobe_blocks;
         DROP TABLE library_wardrobe_settings; DROP TABLE library_reports; DROP TABLE library_appeals;"
    ).unwrap();
    let (_, views) = include_str!("../library_distribution.sql").split_once("CREATE VIEW library_hash_blocked_models AS").unwrap();
    database.execute_batch(&format!("CREATE VIEW library_hash_blocked_models AS{views}")).unwrap();
    database.pragma_update(None, "user_version", 3).unwrap();
    drop(database);

    let storage = Storage::open(directory.path()).unwrap();
    assert_eq!(count(&storage, "library_models"), 1);
    assert_eq!(count(&storage, "library_blocked_hashes"), 1);
    for table in ["library_reports", "library_appeals", "library_wardrobe_blocks", "library_wardrobe_settings"] {
        assert_eq!(count(&storage, table), 0);
    }
    assert!(storage.library_hash_blocked("RAW_SHA256", &"a".repeat(64)).unwrap());
    let backup = std::fs::read_dir(directory.path()).unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|entry| entry.to_string_lossy().contains("pre-library-")).unwrap();
    let backup = Connection::open(backup).unwrap();
    assert_eq!(backup.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), 3);
    let policy: (String, String) = backup.query_row("SELECT visibility, moderation_status FROM library_models", [],
        |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(policy, ("PRIVATE".into(), "BLOCKED".into()));
    drop(storage);
    let reopened = Storage::open(directory.path()).unwrap();
    assert_eq!(count(&reopened, "library_models"), 1);
}

#[test]
fn version_two_equipment_settings_default_closed_and_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.sqlite3");
    let storage = Storage::open(directory.path()).unwrap();
    seed(&storage, "model");
    drop(storage);
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch(
            "DROP TABLE library_collection;
             DROP VIEW library_effective_models;
             DROP VIEW library_hash_blocked_models;
             DROP VIEW library_active_blocked_hashes;
             DROP TABLE library_wardrobe_blocks;
             DROP TABLE library_wardrobe_settings;
             DROP TABLE library_appeals;
             DROP TABLE library_equipment_settings;
             DROP TABLE library_verified_blobs;
             DROP TABLE library_reports;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(database);

    let storage = Storage::open(directory.path()).unwrap();
    assert_eq!(count(&storage, "library_models"), 1);
    storage
        .library_transaction(|transaction| {
            transaction.execute(
            "INSERT INTO library_resource_bindings VALUES ('uploader', 'avatar', 1, 'model', 1)",
            [],
        ).map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO library_equipment_settings(resource_owner, asset_id, revision)
             VALUES ('uploader', 'avatar', 1)",
                    [],
                )
                .map_err(sql_error)?;
            let show: bool = transaction
                .query_row(
                    "SELECT show_private_equipment FROM library_equipment_settings",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            assert!(!show);
            transaction
                .execute(
                    "UPDATE library_equipment_settings SET show_private_equipment = 1",
                    [],
                )
                .map_err(sql_error)?;
            assert!(
                transaction
                    .execute(
                        "UPDATE library_equipment_settings SET show_private_equipment = 2",
                        [],
                    )
                    .is_err()
            );
            Ok(())
        })
        .unwrap();
    storage.transaction(|_| Ok(())).unwrap();
    drop(storage);
    let storage = Storage::open(directory.path()).unwrap();
    let show: bool = storage
        .library_read(|database| {
            database
                .query_row(
                    "SELECT show_private_equipment FROM library_equipment_settings",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_error)
        })
        .unwrap();
    assert!(show);

    let backup = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("pre-library-"))
        .unwrap();
    let backup = Connection::open(backup).unwrap();
    let version: u32 = backup
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let models: i64 = backup
        .query_row("SELECT count(*) FROM library_models", [], |row| row.get(0))
        .unwrap();
    assert_eq!(models, 1);

    let failed_path = directory.path().join("failed-v2.sqlite3");
    let mut failed = Connection::open(&failed_path).unwrap();
    failed
        .execute_batch(
            "PRAGMA user_version = 2; CREATE TABLE library_equipment_settings(retained TEXT);
         INSERT INTO library_equipment_settings VALUES ('unchanged');",
        )
        .unwrap();
    assert!(migrate(&mut failed, &failed_path, 2).is_err());
    assert_eq!(
        failed
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        failed
            .query_row(
                "SELECT retained FROM library_equipment_settings",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "unchanged"
    );
}

#[test]
fn version_one_backup_and_failed_ddl_are_recoverable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("metadata.sqlite3");
    let mut database = Connection::open(&path).unwrap();
    database.execute_batch("PRAGMA user_version = 1; CREATE TABLE old_data(value TEXT); INSERT INTO old_data VALUES ('retained');").unwrap();
    migrate(&mut database, &path, 1).unwrap();
    let version: u32 = database
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let backup = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("pre-library-"))
        .unwrap();
    let restored = Connection::open(&backup).unwrap();
    assert_eq!(
        restored
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .query_row("SELECT value FROM old_data", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "retained"
    );

    let failed_path = directory.path().join("failed.sqlite3");
    let mut failed = Connection::open(&failed_path).unwrap();
    failed
        .execute_batch("PRAGMA user_version = 1; CREATE TABLE library_tags(retained TEXT);")
        .unwrap();
    assert!(migrate(&mut failed, &failed_path, 1).is_err());
    assert_eq!(
        failed
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    let model_exists: bool = failed
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'library_models')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !model_exists,
        "DDL must roll back, not leave a partial library schema"
    );
}

#[test]
fn block_hashes_persist_through_delete_and_only_explicit_unblock_releases_them() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    seed(&storage, "one");
    seed(&storage, "two");
    let actor = Uuid::from_u128(4);
    for (model, expected) in [("one", 1), ("two", 2)] {
        storage
            .library_transaction(|transaction| {
                set_blocked(transaction, model, actor, expected, true, "synthetic reason", 2)
            })
            .unwrap();
    }
    assert!(storage
        .library_hash_blocked("RAW_SHA256", &"a".repeat(64))
        .unwrap());
    storage.library_transaction(|transaction| {
        let state: (String, String, bool) = transaction.query_row(
            "SELECT moderation_status, visibility, requires_authorization FROM library_models WHERE model_id = 'one'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(sql_error)?;
        assert_eq!(state, ("BLOCKED".into(), "PRIVATE".into(), true));
        assert!(transaction.execute("UPDATE library_models SET visibility = 'PUBLIC' WHERE model_id = 'one'", []).is_err());
        transaction.execute("UPDATE library_models SET moderation_status = 'DELETED' WHERE model_id = 'one'", []).map_err(sql_error)?;
        Ok(())
    }).unwrap();
    assert!(storage
        .library_hash_blocked("RAW_SHA256", &"a".repeat(64))
        .unwrap());
    let conflict = storage
        .library_transaction(|transaction| set_blocked(transaction, "one", actor, 1, false, "", 3));
    assert_eq!(conflict, Err(ErrorCode::Conflict));
    storage
        .library_transaction(|transaction| set_blocked(transaction, "one", actor, 3, false, "", 3))
        .unwrap();
    assert!(storage
        .library_hash_blocked("RAW_SHA256", &"a".repeat(64))
        .unwrap());
    storage
        .library_transaction(|transaction| set_blocked(transaction, "two", actor, 4, false, "", 3))
        .unwrap();
    assert!(!storage
        .library_hash_blocked("RAW_SHA256", &"a".repeat(64))
        .unwrap());
    let state: (String, String) = storage
        .library_read(|database| {
            database.query_row(
        "SELECT moderation_status, visibility FROM library_models WHERE model_id = 'two'", [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(sql_error)
        })
        .unwrap();
    assert_eq!(state, ("ACTIVE".into(), "PRIVATE".into()));
    assert_eq!(count(&storage, "library_audit"), 4);
    drop(storage);
    let storage = Storage::open(directory.path()).unwrap();
    assert_eq!(count(&storage, "library_blocked_hashes"), 2);
    assert_eq!(count(&storage, "library_audit"), 4);
}

#[test]
fn concurrent_favorites_grants_and_operations_are_unique_and_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::open(directory.path()).unwrap());
    seed(&storage, "model");
    let mut threads = Vec::new();
    for _ in 0..8 {
        let storage = storage.clone();
        threads.push(std::thread::spawn(move || {
            storage.library_transaction(|transaction| {
                operation(transaction, Uuid::from_u128(1), Uuid::from_u128(2), b"request", 1, |transaction| {
                    transaction.execute("INSERT OR IGNORE INTO library_favorites VALUES ('player', 'model', 1)", []).map_err(sql_error)?;
                    transaction.execute("INSERT OR IGNORE INTO library_grants VALUES ('model', 'player', 'Example', 'owner', 1)", []).map_err(sql_error)?;
                    Ok(b"same-result".to_vec())
                })
            }).unwrap()
        }));
    }
    for thread in threads {
        assert_eq!(thread.join().unwrap(), b"same-result");
    }
    for table in ["library_favorites", "library_grants", "library_operations"] {
        assert_eq!(count(&storage, table), 1);
    }
    let conflict = storage.library_transaction(|transaction| {
        operation(
            transaction,
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            b"changed",
            2,
            |_| panic!("must not execute replay"),
        )
    });
    assert_eq!(conflict, Err(ErrorCode::Conflict));
    let failed = storage.library_transaction(|transaction| {
        operation(
            transaction,
            Uuid::from_u128(1),
            Uuid::from_u128(3),
            b"failed",
            2,
            |transaction| {
                transaction
                    .execute("DELETE FROM library_favorites", [])
                    .map_err(sql_error)?;
                Err(ErrorCode::Conflict)
            },
        )
    });
    assert_eq!(failed, Err(ErrorCode::Conflict));
    assert_eq!(count(&storage, "library_favorites"), 1);
}

#[test]
fn hash_kinds_do_not_alias_or_accept_unverified_text() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    for (kind, hash) in [
        ("unknown", "a".repeat(64)),
        ("RAW_SHA256", "A".repeat(64)),
        ("RAW_SHA256", "a".repeat(63)),
    ] {
        assert_eq!(
            storage.library_hash_blocked(kind, &hash),
            Err(ErrorCode::InvalidMessage)
        );
    }
    seed(&storage, "model");
    storage
        .library_transaction(|transaction| {
            set_blocked(transaction, "model", Uuid::from_u128(1), 1, true, "", 2)
        })
        .unwrap();
    assert!(!storage
        .library_hash_blocked("LEGACY_FIGURA_BASE64_SHA256", &"a".repeat(64))
        .unwrap());
}
