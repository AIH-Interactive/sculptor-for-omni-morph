use super::*;
use rusqlite::{params, Connection, OptionalExtension};

fn read_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| ErrorCode::Internal)
}

pub(super) fn open(root: &Path, path: &Path) -> Result<(Connection, Manifest)> {
    let mut database = Connection::open(path).map_err(sql_error)?;
    database
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sql_error)?;
    let version: u32 = database
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    if version > 2 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    database.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;
        CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS owners (
            uuid TEXT PRIMARY KEY, revision INTEGER NOT NULL, legacy_sha256 BLOB);
        CREATE TABLE IF NOT EXISTS avatars (
            owner_uuid TEXT NOT NULL REFERENCES owners(uuid), asset_id TEXT NOT NULL,
            revision INTEGER NOT NULL, model_type INTEGER NOT NULL, distribution_format INTEGER NOT NULL,
            raw_sha256 BLOB NOT NULL, raw_length INTEGER NOT NULL, descriptor BLOB NOT NULL,
            tombstone INTEGER NOT NULL CHECK(tombstone IN (0,1)),
            PRIMARY KEY(owner_uuid, asset_id, revision));
        CREATE TABLE IF NOT EXISTS equipment (
            owner_uuid TEXT NOT NULL REFERENCES owners(uuid), slot TEXT NOT NULL,
            asset_id TEXT NOT NULL, revision INTEGER NOT NULL,
            PRIMARY KEY(owner_uuid, slot),
            FOREIGN KEY(owner_uuid, asset_id, revision) REFERENCES avatars(owner_uuid, asset_id, revision));
        CREATE TABLE IF NOT EXISTS operations (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL, fingerprint BLOB NOT NULL,
            result BLOB NOT NULL, expires INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS ysm_state (
            owner_uuid TEXT PRIMARY KEY REFERENCES owners(uuid), record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS users (
            uuid TEXT PRIMARY KEY, profile TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS upload_permissions (
            uuid TEXT PRIMARY KEY, allowed INTEGER NOT NULL CHECK(allowed IN (0,1)));
    ").map_err(sql_error)?;

    let instance: Option<String> = database
        .query_row(
            "SELECT value FROM metadata WHERE key = 'instance_id'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let manifest = if let Some(instance_id) = instance {
        load(&database, instance_id)?
    } else {
        // Import once, in the same transaction as the instance marker. Never rewrite the source.
        let legacy_path = root.join("manifest.json");
        let manifest = if legacy_path.exists() {
            serde_json::from_slice::<Manifest>(
                &fs::read(legacy_path).map_err(|_| ErrorCode::Internal)?,
            )
            .map_err(|_| ErrorCode::Internal)?
        } else {
            Manifest {
                schema_version: 1,
                instance_id: faster_hex::hex_string(&crate::utils::rand()[..16]),
                owners: BTreeMap::new(),
                operations: BTreeMap::new(),
                ysm: BTreeMap::new(),
            }
        };
        if manifest.schema_version != 1 {
            return Err(ErrorCode::UnsupportedVersion);
        }
        save(&mut database, &manifest, root)?;
        manifest
    };
    super::library::migrate(&mut database, path, version)?;
    Ok((database, manifest))
}

fn load(database: &Connection, instance_id: String) -> Result<Manifest> {
    let mut manifest = Manifest {
        schema_version: 1,
        instance_id,
        owners: BTreeMap::new(),
        operations: BTreeMap::new(),
        ysm: BTreeMap::new(),
    };
    let mut statement = database
        .prepare("SELECT uuid, revision, legacy_sha256 FROM owners")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                read_unsigned(row, 1)?,
                row.get(2)?,
            ))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (uuid, revision, legacy_sha256) = row.map_err(sql_error)?;
        let uuid = Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?;
        manifest.owners.insert(
            uuid,
            Owner {
                revision,
                legacy_sha256,
                ..Default::default()
            },
        );
    }
    let mut statement = database
        .prepare("SELECT owner_uuid, asset_id, revision, descriptor, tombstone FROM avatars")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                read_unsigned(row, 2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (uuid, id, revision, descriptor, tombstone) = row.map_err(sql_error)?;
        let uuid = Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?;
        let avatar =
            AvatarDescriptor::decode(descriptor.as_slice()).map_err(|_| ErrorCode::Internal)?;
        if avatar.owner != uuid.to_string() || avatar.asset_id != id || avatar.revision != revision
        {
            return Err(ErrorCode::Internal);
        }
        manifest
            .owners
            .get_mut(&uuid)
            .ok_or(ErrorCode::Internal)?
            .assets
            .entry(id)
            .or_default()
            .insert(
                revision,
                StoredAvatar {
                    descriptor,
                    tombstone,
                },
            );
    }
    let mut statement = database
        .prepare("SELECT owner_uuid, slot, asset_id, revision FROM equipment")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                read_unsigned(row, 3)?,
            ))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (uuid, slot, id, revision) = row.map_err(sql_error)?;
        let uuid = Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?;
        let owner = manifest.owners.get_mut(&uuid).ok_or(ErrorCode::Internal)?;
        let selected = Some(AssetRef { id, revision });
        match slot.as_str() {
            "primary" => owner.primary = selected,
            "figura_compatibility" => owner.compatibility = selected,
            _ => return Err(ErrorCode::Internal),
        }
    }
    let mut statement = database
        .prepare("SELECT id, kind, fingerprint, result, expires FROM operations")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Operation {
                    kind: row.get(1)?,
                    fingerprint: row.get(2)?,
                    result: row.get(3)?,
                    expires: read_unsigned(row, 4)?,
                },
            ))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (id, operation) = row.map_err(sql_error)?;
        manifest.operations.insert(id, operation);
    }
    let mut statement = database
        .prepare("SELECT owner_uuid, record FROM ysm_state")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    for row in rows {
        let (uuid, record) = row.map_err(sql_error)?;
        manifest.ysm.insert(
            Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?,
            serde_json::from_str(&record).map_err(|_| ErrorCode::Internal)?,
        );
    }
    for (uuid, owner) in &manifest.owners {
        equipment(*uuid, Some(owner))?;
    }
    Ok(manifest)
}

pub(super) fn save(database: &mut Connection, manifest: &Manifest, root: &Path) -> Result<()> {
    save_with(database, manifest, root, |_| Ok(()))
}

pub(super) fn save_with(
    database: &mut Connection,
    manifest: &Manifest,
    root: &Path,
    library_change: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
) -> Result<()> {
    let transaction = database.transaction().map_err(sql_error)?;
    transaction
        .execute_batch(
            "DELETE FROM equipment; DELETE FROM avatars; DELETE FROM ysm_state;
        DELETE FROM owners; DELETE FROM operations;",
        )
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT INTO metadata(key, value) VALUES ('instance_id', ?1)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&manifest.instance_id],
        )
        .map_err(sql_error)?;
    for (uuid, owner) in &manifest.owners {
        let uuid = uuid.to_string();
        transaction
            .execute(
                "INSERT INTO owners VALUES (?1, ?2, ?3)",
                params![uuid, sql_integer(owner.revision)?, owner.legacy_sha256],
            )
            .map_err(sql_error)?;
        // Old manifests knew owners but did not persist account profiles. Preserve those UUIDs.
        let profile = serde_json::to_string(&crate::auth::Userinfo {
            uuid: Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?,
            ..Default::default()
        })
        .map_err(|_| ErrorCode::Internal)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO users VALUES (?1, ?2)",
                params![uuid, profile],
            )
            .map_err(sql_error)?;
        for (id, history) in &owner.assets {
            for (revision, stored) in history {
                let avatar = AvatarDescriptor::decode(stored.descriptor.as_slice())
                    .map_err(|_| ErrorCode::Internal)?;
                if avatar.owner != uuid || avatar.asset_id != *id || avatar.revision != *revision {
                    return Err(ErrorCode::Internal);
                }
                transaction
                    .execute(
                        "INSERT INTO avatars VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            uuid,
                            id,
                            sql_integer(*revision)?,
                            avatar.model_type,
                            avatar.distribution_format,
                            avatar.raw_sha256,
                            sql_integer(avatar.raw_length)?,
                            stored.descriptor,
                            stored.tombstone
                        ],
                    )
                    .map_err(sql_error)?;
            }
        }
        for (slot, selected) in [
            ("primary", &owner.primary),
            ("figura_compatibility", &owner.compatibility),
        ] {
            if let Some(selected) = selected {
                transaction
                    .execute(
                        "INSERT INTO equipment VALUES (?1, ?2, ?3, ?4)",
                        params![uuid, slot, selected.id, sql_integer(selected.revision)?],
                    )
                    .map_err(sql_error)?;
            }
        }
    }
    for (id, operation) in &manifest.operations {
        transaction
            .execute(
                "INSERT INTO operations VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    operation.kind,
                    operation.fingerprint,
                    operation.result,
                    sql_integer(operation.expires)?
                ],
            )
            .map_err(sql_error)?;
    }
    for (uuid, record) in &manifest.ysm {
        let record = serde_json::to_string(record).map_err(|_| ErrorCode::Internal)?;
        transaction
            .execute(
                "INSERT INTO ysm_state VALUES (?1, ?2)",
                params![uuid.to_string(), record],
            )
            .map_err(sql_error)?;
    }
    #[cfg(test)]
    tests::pause_before_replace(root);
    #[cfg(not(test))]
    let _ = root;
    library_change(&transaction)?;
    transaction.commit().map_err(sql_error)
}

pub(super) fn sql_error(error: rusqlite::Error) -> ErrorCode {
    tracing::error!(error = %error, "SQLite metadata operation failed");
    ErrorCode::Internal
}
