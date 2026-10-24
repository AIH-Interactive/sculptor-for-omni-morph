//! Owner-private provenance for already uploaded native resources; no catalog publication.
use super::*;
use crate::service::{
    outfit::{AccessoryMetadata, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalBinding {
    pub reference: ResourceRef,
    pub asset_id: String,
    pub asset_revision: u64,
    pub raw_sha256: String,
    pub resource_kind: ResourceKind,
    pub accessory_metadata: Option<AccessoryMetadata>,
}

impl LocalBinding {
    pub(super) fn validate(&self, limits: &OutfitLimits) -> Result<()> {
        limits.validate()?;
        self.reference.validate()?;
        if !matches!(self.reference, ResourceRef::Local { .. })
            || self.asset_revision == 0
            || self.asset_revision > i64::MAX as u64
            || self.resource_kind == ResourceKind::Outfit
        {
            return Err(ErrorCode::InvalidMessage);
        }
        crate::service::library::text(&self.asset_id, 128, false)?;
        library::validate_hash("RAW_SHA256", &self.raw_sha256)?;
        match (self.resource_kind, self.accessory_metadata.as_ref()) {
            (ResourceKind::Avatar, None) => {}
            (ResourceKind::Accessory, Some(metadata)) => {
                metadata.validate_with_limits(limits)?;
                if serde_json::to_vec(metadata)
                    .map_err(|_| ErrorCode::InvalidMessage)?
                    .len()
                    > 16384
                {
                    return Err(ErrorCode::InvalidMessage);
                }
            }
            _ => return Err(ErrorCode::InvalidMessage),
        }
        Ok(())
    }

    /// Native ownership and immutable identity, independent of render/use admission.
    pub(super) fn validate_native_identity(
        &self,
        database: &Connection,
        owner: Uuid,
    ) -> Result<()> {
        let native: Option<(Vec<u8>, i32)> = database
            .query_row(
                "SELECT raw_sha256, model_type FROM avatars
                 WHERE owner_uuid = ?1 AND asset_id = ?2 AND revision = ?3 AND tombstone = 0",
                params![
                    owner.to_string(),
                    self.asset_id,
                    library::sql_integer(self.asset_revision)?
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite::sql_error)?;
        let (raw_hash, model_type) = native.ok_or(ErrorCode::NotFound)?;
        let ResourceRef::Local { format, .. } = &self.reference else {
            return Err(ErrorCode::InvalidMessage);
        };
        let expected_type = match format.as_str() {
            "FIGURA" => 1,
            "YSM_PLAIN" => 2,
            "YSM_ENCRYPTED" => 3,
            _ => return Err(ErrorCode::InvalidMessage),
        };
        if faster_hex::hex_string(&raw_hash) != self.raw_sha256 || model_type != expected_type {
            return Err(ErrorCode::Conflict);
        }
        let bound: bool = database
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_resource_bindings
                 WHERE resource_owner = ?1 AND asset_id = ?2)",
                params![owner.to_string(), self.asset_id],
                |row| row.get(0),
            )
            .map_err(sqlite::sql_error)?;
        if bound {
            return Err(ErrorCode::Forbidden);
        }
        Ok(())
    }

    /// Explicit bindings and modern Apply must also pass current use policy.
    pub(super) fn validate_native(&self, database: &Connection, owner: Uuid) -> Result<()> {
        self.validate_native_identity(database, owner)?;
        if library_wardrobe::banned(database, owner, &self.asset_id, self.asset_revision)? {
            return Err(ErrorCode::Forbidden);
        }
        Ok(())
    }
}

pub(crate) fn resolve_local(
    database: &Connection,
    owner: Uuid,
    reference: &ResourceRef,
    kind: ResourceKind,
    limits: &OutfitLimits,
) -> Result<LocalBinding> {
    let ResourceRef::Local {
        local_id,
        content_hash,
        format,
    } = reference
    else {
        return Err(ErrorCode::InvalidMessage);
    };
    let encoded: Option<String> = database
        .query_row(
            "SELECT binding_json FROM outfit_local_resources
         WHERE owner_uuid = ?1 AND local_id = ?2 AND source_hash = ?3 AND model_type = ?4",
            params![
                owner.to_string(),
                local_id.to_string(),
                content_hash,
                format
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite::sql_error)?;
    let binding: LocalBinding = serde_json::from_str(&encoded.ok_or(ErrorCode::NotFound)?)
        .map_err(|_| ErrorCode::Internal)?;
    binding.validate(limits)?;
    if binding.reference != *reference || binding.resource_kind != kind {
        return Err(ErrorCode::Conflict);
    }
    binding.validate_native(database, owner)?;
    Ok(binding)
}

pub(super) fn bind(
    transaction: &rusqlite::Transaction<'_>,
    owner: Uuid,
    input: &LocalBinding,
    limits: &OutfitLimits,
) -> Result<LocalBinding> {
    input.validate(limits)?;
    let mut binding = input.clone();
    if let Some(metadata) = &mut binding.accessory_metadata {
        // The backend verifies native bytes, not encrypted bones or client extraction.
        metadata.validation_source = "CLIENT_EXTRACTED".into();
    }
    let encoded = serde_json::to_string(&binding).map_err(|_| ErrorCode::InvalidMessage)?;
    if encoded.len() > 32768 {
        return Err(ErrorCode::InvalidMessage);
    }
    let ResourceRef::Local {
        local_id,
        content_hash,
        format,
    } = &binding.reference
    else {
        return Err(ErrorCode::InvalidMessage);
    };

    binding.validate_native(transaction, owner)?;
    let prior: Option<String> = transaction
        .query_row(
            "SELECT binding_json FROM outfit_local_resources
                     WHERE owner_uuid = ?1 AND local_id = ?2 AND source_hash = ?3 AND model_type = ?4",
            params![owner.to_string(), local_id.to_string(), content_hash, format],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite::sql_error)?;
    if let Some(prior) = prior {
        if prior != encoded {
            return Err(ErrorCode::Conflict);
        }
    } else {
        transaction
            .execute(
                "INSERT INTO outfit_local_resources VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    owner.to_string(),
                    local_id.to_string(),
                    content_hash,
                    format,
                    encoded
                ],
            )
            .map_err(sqlite::sql_error)?;
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO outfit_private_sources VALUES (?1, ?2, ?3)",
            params![
                owner.to_string(),
                binding.asset_id,
                library::sql_integer(binding.asset_revision)?
            ],
        )
        .map_err(sqlite::sql_error)?;
    Ok(binding.clone())
}

impl Storage {
    /// Owner inventory lookup, independent of whether this source is currently worn.
    /// Only an absent binding is None; revoked/deleted native resources remain errors.
    pub(crate) fn outfit_resolve_local(
        &self,
        owner: Uuid,
        reference: &ResourceRef,
        kind: ResourceKind,
        limits: &OutfitLimits,
    ) -> Result<Option<LocalBinding>> {
        reference.validate()?;
        let ResourceRef::Local {
            local_id,
            content_hash,
            format,
        } = reference
        else {
            return Err(ErrorCode::InvalidMessage);
        };
        if kind == ResourceKind::Outfit {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_read(|database| {
            let exists: bool = database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM outfit_local_resources
                 WHERE owner_uuid = ?1 AND local_id = ?2 AND source_hash = ?3 AND model_type = ?4)",
                    params![
                        owner.to_string(),
                        local_id.to_string(),
                        content_hash,
                        format
                    ],
                    |row| row.get(0),
                )
                .map_err(sqlite::sql_error)?;
            if !exists {
                return Ok(None);
            }
            resolve_local(database, owner, reference, kind, limits).map(Some)
        })
    }

    pub(crate) fn outfit_bind_local(
        &self,
        owner: Uuid,
        input: &LocalBinding,
        limits: &OutfitLimits,
    ) -> Result<LocalBinding> {
        input.validate(limits)?;
        let _publication = self.lock_blob_files()?;
        let avatar = self.avatar(owner, &input.asset_id, input.asset_revision)?;
        self.library_ensure_verified_blob(&avatar)?;
        self.library_transaction(|transaction| bind(transaction, owner, input, limits))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outfit_private_marker_version_nine_upgrade_keeps_existing_bindings_and_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let binding = LocalBinding {
            reference: ResourceRef::Local {
                local_id: Uuid::from_u128(2),
                content_hash: "a".repeat(64),
                format: "FIGURA".into(),
            },
            asset_id: "legacy-avatar".into(),
            asset_revision: 1,
            raw_sha256: "b".repeat(64),
            resource_kind: ResourceKind::Avatar,
            accessory_metadata: None,
        };
        let mut configuration = crate::service::outfit::OutfitConfig::default();
        configuration.revision = 5;
        configuration.base = Some(binding.reference.clone());
        let binding_json = serde_json::to_string(&binding).unwrap();
        let configuration_json = serde_json::to_string(&configuration).unwrap();
        storage
            .library_transaction(|database| {
                // Historical metadata fixture; native geometry is irrelevant to schema migration.
                database
                    .execute(
                        "INSERT INTO outfit_local_resources VALUES (?1, ?2, ?3, 'FIGURA', ?4)",
                        params![
                            owner.to_string(),
                            Uuid::from_u128(2).to_string(),
                            "a".repeat(64),
                            binding_json
                        ],
                    )
                    .unwrap();
                database
                    .execute(
                        "INSERT INTO outfit_configurations VALUES (?1, 5, ?2)",
                        params![owner.to_string(), configuration_json],
                    )
                    .unwrap();
                database
                    .execute_batch("DROP TABLE outfit_private_uploads; DROP TABLE outfit_private_sources; PRAGMA user_version = 9;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        drop(storage);
        let upgraded = Storage::open(directory.path()).unwrap();
        assert_eq!(upgraded.outfit_current(owner).unwrap(), configuration);
        upgraded
            .library_read(|database| {
                let saved: String = database
                    .query_row(
                        "SELECT binding_json FROM outfit_local_resources",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(saved, binding_json);
                let count: i64 = database
                    .query_row("SELECT count(*) FROM outfit_private_sources", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    count, 0,
                    "legacy normalization does not imply explicit owner privacy intent"
                );
                let version: u32 = database
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .unwrap();
                assert_eq!(version, library::SCHEMA_VERSION);
                Ok(())
            })
            .unwrap();
        let backup = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.to_string_lossy().contains("pre-library-"))
            .unwrap();
        let backup = Connection::open(backup).unwrap();
        assert_eq!(
            backup
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            9
        );
        drop(upgraded);
        assert_eq!(
            Storage::open(directory.path())
                .unwrap()
                .outfit_current(owner)
                .unwrap(),
            configuration
        );
    }

    #[test]
    fn version_eight_private_binding_upgrade_preserves_configuration_and_backup() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let mut configuration = crate::service::outfit::OutfitConfig::default();
        configuration.revision = 7;
        let json = serde_json::to_string(&configuration).unwrap();
        storage
            .library_transaction(|transaction| {
                transaction
                    .execute(
                        "INSERT INTO outfit_configurations VALUES (?1, 7, ?2)",
                        params![owner.to_string(), json],
                    )
                    .unwrap();
                transaction
                    .execute_batch("DROP TABLE outfit_private_uploads; DROP TABLE outfit_private_sources; DROP TABLE outfit_local_resources; PRAGMA user_version = 8;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        drop(storage);

        let reopened = Storage::open(directory.path()).unwrap();
        assert_eq!(reopened.outfit_current(owner).unwrap(), configuration);
        reopened
            .library_read(|database| {
                let version: u32 = database
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .unwrap();
                assert_eq!(version, library::SCHEMA_VERSION);
                let count: i64 = database
                    .query_row("SELECT count(*) FROM outfit_local_resources", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
        let backup = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.to_string_lossy().contains("pre-library-"))
            .unwrap();
        let backup = Connection::open(backup).unwrap();
        assert_eq!(
            backup
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            8
        );
        assert_eq!(
            backup
                .query_row(
                    "SELECT configuration_json FROM outfit_configurations",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            json
        );
        drop(reopened);
        assert_eq!(
            Storage::open(directory.path())
                .unwrap()
                .outfit_current(owner)
                .unwrap(),
            configuration
        );
    }
}
