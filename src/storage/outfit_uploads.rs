//! Private source preparation over the existing native upload/commit transaction.
use super::*;
use crate::protocol::v1::UploadRequest;
use crate::service::{
    library::sql_error,
    outfit::{AccessoryMetadata, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

impl Storage {
    /// Catalog aliases and their publication commit together; original private sources remain intact.
    pub(crate) fn outfit_publish_library_assets<T>(
        &self,
        owner: Uuid,
        assets: &[AvatarDescriptor],
        publish: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut result = None;
        self.transaction_with_library(
            |manifest| {
                let account = manifest.owners.entry(owner).or_default();
                for asset in assets {
                    let encoded = asset.encode_to_vec();
                    if let Some(history) = account.assets.get(&asset.asset_id) {
                        let stored = history.get(&1).ok_or(ErrorCode::Conflict)?;
                        if stored.tombstone || stored.descriptor != encoded {
                            return Err(ErrorCode::Conflict);
                        }
                    } else {
                        if account.assets.len() >= 1024 {
                            return Err(ErrorCode::LimitExceeded);
                        }
                        account.assets.insert(
                            asset.asset_id.clone(),
                            BTreeMap::from([(
                                1,
                                StoredAvatar {
                                    descriptor: encoded,
                                    tombstone: false,
                                },
                            )]),
                        );
                    }
                }
                Ok(())
            },
            |transaction| {
                result = Some(publish(transaction)?);
                Ok(())
            },
        )?;
        result.ok_or(ErrorCode::Internal)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrivateUpload {
    pub operation_id: Uuid,
    pub reference: ResourceRef,
    pub resource_kind: ResourceKind,
    pub accessory_metadata: Option<AccessoryMetadata>,
    pub raw_sha256: String,
    pub byte_length: u64,
    pub content_sha256: Option<String>,
    pub schema_sha256: Option<String>,
}

impl PrivateUpload {
    pub(crate) fn request(&self, owner: Uuid) -> Result<UploadRequest> {
        let ResourceRef::Local { format, .. } = &self.reference else {
            return Err(ErrorCode::InvalidMessage);
        };
        let kind = match format.as_str() {
            "FIGURA" => 1,
            "YSM_PLAIN" => 2,
            "YSM_ENCRYPTED" => 3,
            _ => return Err(ErrorCode::InvalidMessage),
        };
        fn digest(value: &str) -> Result<Vec<u8>> {
            library::validate_hash("RAW_SHA256", value)?;
            let mut bytes = vec![0; 32];
            faster_hex::hex_decode(value.as_bytes(), &mut bytes)
                .map_err(|_| ErrorCode::InvalidMessage)?;
            Ok(bytes)
        }
        let content = self
            .content_sha256
            .as_deref()
            .map(digest)
            .transpose()?
            .unwrap_or_default();
        let schema = self
            .schema_sha256
            .as_deref()
            .map(digest)
            .transpose()?
            .unwrap_or_default();
        if self.byte_length == 0
            || self.byte_length > i64::MAX as u64
            || (kind != 1 && (content.len() != 32 || schema.len() != 32))
        {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(UploadRequest {
            operation_id: self.operation_id.to_string(),
            resource: Some(AvatarDescriptor {
                owner: owner.to_string(),
                asset_id: format!("outfit_{}", self.operation_id.simple()),
                model_type: kind,
                distribution_format: if kind == 1 { 1 } else { 2 },
                raw_sha256: digest(&self.raw_sha256)?,
                raw_length: self.byte_length,
                content_sha256: content,
                schema_sha256: schema,
                required_capabilities: vec![kind],
                ..Default::default()
            }),
        })
    }

    fn binding(&self, owner: Uuid) -> Result<outfit_resources::LocalBinding> {
        let descriptor = self.request(owner)?.resource.ok_or(ErrorCode::Internal)?;
        Ok(outfit_resources::LocalBinding {
            reference: self.reference.clone(),
            asset_id: descriptor.asset_id,
            asset_revision: 1,
            raw_sha256: self.raw_sha256.clone(),
            resource_kind: self.resource_kind,
            accessory_metadata: self.accessory_metadata.clone(),
        })
    }

    pub(crate) fn validate(&self, owner: Uuid, limits: &OutfitLimits) -> Result<()> {
        self.binding(owner)?.validate(limits)
    }
}

fn lookup(
    database: &Connection,
    owner: Uuid,
    operation: &str,
    asset: Option<&str>,
) -> Result<Option<PrivateUpload>> {
    let json: Option<String> = database
        .query_row(
            "SELECT input_json FROM outfit_private_uploads WHERE owner_uuid = ?1
         AND (operation_id = ?2 OR asset_id = ?3)",
            params![owner.to_string(), operation, asset],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(|_| ErrorCode::Internal))
        .transpose()
}

/// Check both operation and asset so a different native operation cannot occupy the reservation.
pub(crate) fn authorize(
    database: &Connection,
    owner: Uuid,
    request: &UploadRequest,
    limits: &OutfitLimits,
) -> Result<()> {
    let descriptor = request.resource.as_ref().ok_or(ErrorCode::InvalidMessage)?;
    if let Some(input) = lookup(
        database,
        owner,
        &request.operation_id,
        Some(&descriptor.asset_id),
    )? {
        input.validate(owner, limits)?;
        if input.request(owner)? != *request {
            return Err(ErrorCode::Conflict);
        }
    }
    Ok(())
}

/// Runs after native rows and verified identities are written, before their shared commit.
pub(crate) fn publish(
    database: &Transaction<'_>,
    owner: Uuid,
    operation: &str,
    descriptor: &AvatarDescriptor,
    limits: &OutfitLimits,
) -> Result<()> {
    let Some(input) = lookup(database, owner, operation, Some(&descriptor.asset_id))? else {
        return Ok(());
    };
    input.validate(owner, limits)?;
    let expected = input.request(owner)?.resource.ok_or(ErrorCode::Internal)?;
    if input.operation_id.to_string() != operation
        || descriptor.owner != expected.owner
        || descriptor.asset_id != expected.asset_id
        || descriptor.revision != 1
        || descriptor.model_type != expected.model_type
        || descriptor.distribution_format != expected.distribution_format
        || descriptor.raw_sha256 != expected.raw_sha256
        || descriptor.raw_length != expected.raw_length
        || (!expected.content_sha256.is_empty()
            && descriptor.content_sha256 != expected.content_sha256)
        || descriptor.schema_sha256 != expected.schema_sha256
    {
        return Err(ErrorCode::Conflict);
    }
    outfit_resources::bind(database, owner, &input.binding(owner)?, limits)?;
    Ok(())
}

impl Storage {
    pub(crate) fn outfit_reserve_upload(
        &self,
        owner: Uuid,
        input: &PrivateUpload,
        limits: &OutfitLimits,
        max_assets: u64,
    ) -> Result<PrivateUpload> {
        let mut input = input.clone();
        if let Some(metadata) = &mut input.accessory_metadata {
            metadata.validation_source = "CLIENT_EXTRACTED".into();
        }
        input.validate(owner, limits)?;
        let asset = input
            .request(owner)?
            .resource
            .ok_or(ErrorCode::Internal)?
            .asset_id;
        let json = serde_json::to_string(&input).map_err(|_| ErrorCode::InvalidMessage)?;
        if json.len() > 32768 {
            return Err(ErrorCode::InvalidMessage);
        }
        let fingerprint = sha256(
            &serde_json::to_vec(&("outfit-private-upload", &input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|database| {
            if let Some(previous) = lookup(database, owner, &input.operation_id.to_string(), None)? {
                return if previous == input {
                    Ok(previous)
                } else {
                    Err(ErrorCode::Conflict)
                };
            }
            let result = library::operation(
                database,
                owner,
                input.operation_id,
                &fingerprint,
                now_seconds(),
                |database| {
                    let occupied: bool = database
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM avatars WHERE owner_uuid = ?1 AND asset_id = ?2)
                     OR EXISTS(SELECT 1 FROM operations WHERE id = ?3)",
                            params![owner.to_string(), asset, format!("{owner}/{}", input.operation_id)],
                            |row| row.get(0),
                        )
                        .map_err(sql_error)?;
                    if occupied {
                        return Err(ErrorCode::Conflict);
                    }
                    let count: i64 = database
                        .query_row(
                            "SELECT (SELECT count(DISTINCT asset_id) FROM avatars WHERE owner_uuid = ?1)
                     + (SELECT count(*) FROM outfit_private_uploads u WHERE u.owner_uuid = ?1
                        AND NOT EXISTS(SELECT 1 FROM avatars a WHERE a.owner_uuid = u.owner_uuid AND a.asset_id = u.asset_id))",
                            [owner.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(sql_error)?;
                    if u64::try_from(count).map_err(|_| ErrorCode::Internal)? >= max_assets {
                        return Err(ErrorCode::LimitExceeded);
                    }
                    database
                        .execute(
                            "INSERT INTO outfit_private_uploads VALUES (?1, ?2, ?3, ?4)",
                            params![owner.to_string(), input.operation_id.to_string(), asset, json],
                        )
                        .map_err(sql_error)?;
                    Ok(json.as_bytes().to_vec())
                },
            )?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Conflict)
        })
    }

    pub(crate) fn outfit_upload_reservation(
        &self,
        owner: Uuid,
        operation: Uuid,
    ) -> Result<PrivateUpload> {
        self.library_read(|database| {
            lookup(database, owner, &operation.to_string(), None)?.ok_or(ErrorCode::NotFound)
        })
    }

    pub(crate) fn outfit_authorize_upload(
        &self,
        owner: Uuid,
        request: &UploadRequest,
        limits: &OutfitLimits,
    ) -> Result<()> {
        self.library_read(|database| authorize(database, owner, request, limits))
    }

    /// A committed immutable private source outlives the ordinary native upload receipt TTL.
    pub(crate) fn outfit_completed_upload(
        &self,
        owner: Uuid,
        operation: Uuid,
        limits: &OutfitLimits,
    ) -> Result<Option<AvatarDescriptor>> {
        self.library_transaction(|database| {
            let Some(input) = lookup(database, owner, &operation.to_string(), None)? else {
                return Ok(None);
            };
            input.validate(owner, limits)?;
            let binding = input.binding(owner)?;
            let bytes: Option<Vec<u8>> = database
                .query_row(
                    "SELECT a.descriptor FROM avatars a JOIN outfit_private_sources p
                 ON p.owner_uuid = a.owner_uuid AND p.asset_id = a.asset_id AND p.asset_revision = a.revision
                 WHERE a.owner_uuid = ?1 AND a.asset_id = ?2 AND a.revision = 1 AND a.tombstone = 0",
                    params![owner.to_string(), binding.asset_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            let descriptor = AvatarDescriptor::decode(bytes.as_slice()).map_err(|_| ErrorCode::Internal)?;
            let actual = outfit_resources::resolve_local(
                database, owner, &input.reference, input.resource_kind, limits,
            )?;
            let expected = input.request(owner)?.resource.ok_or(ErrorCode::Internal)?;
            if actual != binding || descriptor.raw_sha256 != expected.raw_sha256 {
                return Err(ErrorCode::Conflict);
            }
            Ok(Some(descriptor))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outfit_private_upload_version_ten_upgrade_preserves_prior_privacy_and_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Uuid::from_u128(1);
        let storage = Storage::open(directory.path()).unwrap();
        let mut configuration = crate::service::outfit::OutfitConfig::default();
        configuration.revision = 3;
        storage
            .library_transaction(|database| {
                database
                    .execute(
                        "INSERT INTO outfit_configurations VALUES (?1, 3, ?2)",
                        params![
                            owner.to_string(),
                            serde_json::to_string(&configuration).unwrap()
                        ],
                    )
                    .unwrap();
                database
                    .execute(
                        "INSERT INTO outfit_private_sources VALUES (?1, 'historical-private', 1)",
                        [owner.to_string()],
                    )
                    .unwrap();
                database
                    .execute_batch("DROP TABLE outfit_private_uploads; PRAGMA user_version = 10;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        drop(storage);
        let upgraded = Storage::open(directory.path()).unwrap();
        assert_eq!(upgraded.outfit_current(owner).unwrap(), configuration);
        upgraded
            .library_read(|database| {
                let private: i64 = database
                    .query_row("SELECT count(*) FROM outfit_private_sources", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                let uploads: i64 = database
                    .query_row("SELECT count(*) FROM outfit_private_uploads", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!((private, uploads), (1, 0));
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
            10
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
}
