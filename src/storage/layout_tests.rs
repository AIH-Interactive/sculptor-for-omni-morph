use super::*;
use std::sync::Arc;

#[test]
fn relocates_wal_database_and_original_models_without_overwriting_new_data() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let avatars = data.join("avatars");
    let old = avatars.join("cloud-v1");
    let owner = Uuid::from_u128(9901);
    let storage = Arc::new(Storage::open(&old).unwrap());
    let manager = crate::auth::UManager::open(storage.clone()).unwrap();
    manager
        .insert_user(
            owner,
            crate::auth::Userinfo {
                uuid: owner,
                nickname: "WAL account".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let raw = b"original distribution bytes";
    let hash = sha256(raw);
    fs::write(storage.blob_path(&hash).unwrap(), raw).unwrap();
    fs::write(avatars.join(format!("{owner}.moon")), b"legacy source").unwrap();
    fs::create_dir_all(avatars.join("temp")).unwrap();
    fs::write(
        avatars.join("temp").join(format!("{owner}.moon")),
        b"temporary source",
    )
    .unwrap();

    // Keep the old connection alive: committed account data may still be in its WAL.
    let migrated =
        Arc::new(Storage::open_avatars(&avatars, data.join("metadata.sqlite3")).unwrap());
    assert!(data.join("metadata.sqlite3").is_file());
    assert_eq!(
        migrated.upload_path(owner, owner).parent().unwrap(),
        avatars.join("upload")
    );
    assert_eq!(
        migrated.blob_path(&hash).unwrap().parent().unwrap(),
        avatars.join("models")
    );
    assert_eq!(fs::read(migrated.blob_path(&hash).unwrap()).unwrap(), raw);
    assert_eq!(
        fs::read(avatars.join("models").join(format!("{owner}.moon"))).unwrap(),
        b"legacy source"
    );
    assert_eq!(
        fs::read(avatars.join("upload").join(format!("{owner}.moon"))).unwrap(),
        b"temporary source"
    );
    assert_eq!(
        crate::auth::UManager::open(migrated.clone())
            .unwrap()
            .get_by_uuid(&owner)
            .unwrap()
            .nickname,
        "WAL account"
    );
    assert_eq!(migrated.instance_id(), storage.instance_id());
    assert_eq!(
        fs::read(storage.blob_path(&hash).unwrap()).unwrap(),
        raw,
        "migration retains the original files"
    );
    drop(migrated);
    let temporary = avatars.join("upload").join(format!("{owner}.moon"));
    fs::remove_file(&temporary).unwrap();
    fs::write(
        avatars.join("models").join(format!("{owner}.moon")),
        b"new canonical model",
    )
    .unwrap();
    let reopened = Storage::open_avatars(&avatars, data.join("metadata.sqlite3")).unwrap();
    assert_eq!(
        fs::read(avatars.join("models").join(format!("{owner}.moon"))).unwrap(),
        b"new canonical model"
    );
    assert_eq!(reopened.instance_id(), storage.instance_id());
    assert!(
        !temporary.exists(),
        "consumed uploads must not reappear from backups"
    );
}

#[test]
fn new_layout_imports_old_manifest_and_does_not_clean_unknown_schema_uploads() {
    let directory = tempfile::tempdir().unwrap();
    let avatars = directory.path().join("avatars");
    let old = avatars.join("cloud-v1");
    fs::create_dir_all(&old).unwrap();
    let manifest = Manifest {
        schema_version: 1,
        instance_id: "retained-instance".into(),
        owners: BTreeMap::new(),
        operations: BTreeMap::new(),
        ysm: BTreeMap::new(),
    };
    let original = serde_json::to_vec(&manifest).unwrap();
    fs::write(old.join("manifest.json"), &original).unwrap();
    let database = directory.path().join("metadata.sqlite3");
    let storage = Storage::open_avatars(&avatars, &database).unwrap();
    assert_eq!(storage.instance_id(), "retained-instance");
    drop(storage);
    assert_eq!(fs::read(old.join("manifest.json")).unwrap(), original);
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.pragma_update(None, "user_version", 4).unwrap();
    let pending = avatars.join("upload/future.part");
    fs::write(&pending, b"keep").unwrap();
    assert_eq!(
        Storage::open_avatars(&avatars, directory.path().join("metadata.sqlite3")).unwrap_err(),
        ErrorCode::UnsupportedVersion
    );
    assert_eq!(fs::read(pending).unwrap(), b"keep");
}
