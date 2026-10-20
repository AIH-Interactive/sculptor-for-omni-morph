//! Reserve pending metadata before issuing an existing resource upload grant.
use super::*;
use crate::protocol::v1::{AvatarDescriptor, DistributionFormat, ModelType, UploadRequest};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UploadInput {
    pub operation_id: String,
    pub metadata: Metadata,
    pub raw_sha256: String,
    pub byte_length: u64,
    pub content_sha256: Option<String>,
    pub schema_sha256: Option<String>,
    pub source_filename: String,
    pub metadata_source: String,
}

#[derive(Deserialize, Serialize)]
pub struct Reservation {
    pub kind: String,
    pub model_id: String,
    pub request: UploadInput,
}

impl Reservation {
    pub fn descriptor(&self, owner: Uuid) -> Result<UploadRequest> {
        let kind = match self.request.metadata.model_type.as_str() {
            "FIGURA" => ModelType::ModelFigura,
            "YSM_PLAIN" => ModelType::ModelYsmPlain,
            "YSM_ENCRYPTED" => ModelType::ModelYsmEncrypted,
            _ => return Err(ErrorCode::UnsupportedCapability),
        };
        fn digest(text: &str) -> Result<Vec<u8>> {
            library::validate_hash("RAW_SHA256", text)?;
            let mut digest = vec![0; 32];
            faster_hex::hex_decode(text.as_bytes(), &mut digest)
                .map_err(|_| ErrorCode::InvalidMessage)?;
            Ok(digest)
        }
        let raw_sha256 = digest(&self.request.raw_sha256)?;
        let content_sha256 = self
            .request
            .content_sha256
            .as_deref()
            .map(digest)
            .transpose()?
            .unwrap_or_default();
        let schema_sha256 = self
            .request
            .schema_sha256
            .as_deref()
            .map(digest)
            .transpose()?
            .unwrap_or_default();
        if kind != ModelType::ModelFigura
            && (content_sha256.len() != 32 || schema_sha256.len() != 32)
        {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(UploadRequest {
            operation_id: self.request.operation_id.clone(),
            resource: Some(AvatarDescriptor {
                owner: owner.to_string(),
                asset_id: format!("library_{}", self.model_id.replace('-', "")),
                model_type: kind as i32,
                distribution_format: if kind == ModelType::ModelFigura {
                    DistributionFormat::FiguraCompressedNbt as i32
                } else {
                    DistributionFormat::YsmDistributionPackage as i32
                },
                raw_sha256,
                raw_length: self.request.byte_length,
                content_sha256,
                schema_sha256,
                required_capabilities: vec![kind as i32],
                ..Default::default()
            }),
        })
    }
}

impl Storage {
    pub fn library_reserve(
        &self,
        actor: Actor,
        name: &str,
        input: &UploadInput,
        categories: &[String],
    ) -> Result<Reservation> {
        input.metadata.validate(categories)?;
        text(name, 64, false)?;
        text(&input.source_filename, 128, false)?;
        let file = input.source_filename.to_ascii_lowercase();
        if input.source_filename.contains(['/', '\\', ':'])
            || !(file.ends_with(".zip") || file.ends_with(".ysm"))
            || input.byte_length == 0
            || !matches!(
                input.metadata_source.as_str(),
                "avatar.json" | "ysm.json" | "main.json" | "encrypted_manifest"
            )
            || (input.metadata.model_type == "FIGURA" && input.metadata_source != "avatar.json")
            || (input.metadata.model_type == "YSM_ENCRYPTED"
                && input.metadata_source != "encrypted_manifest")
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let input_bytes = serde_json::to_vec(input).map_err(|_| ErrorCode::Internal)?;
        let fingerprint = crate::storage::sha256(&[b"reserve".as_slice(), &input_bytes].concat());
        self.library_transaction(|transaction| {
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let reservation = Reservation {
                    kind: "upload".into(),
                    model_id: random_id(),
                    request: input.clone(),
                };
                let request = reservation.descriptor(actor.uuid)?;
                let descriptor = request.resource.ok_or(ErrorCode::Internal)?;
                let now = library::sql_integer(now_seconds())?;
                let visibility = match input.metadata.visibility {
                    Visibility::Public => "PUBLIC",
                    Visibility::Private => "PRIVATE",
                };
                let count: i64 = transaction.query_row("SELECT count(*) FROM library_models WHERE uploader_uuid = ?1 AND moderation_status != 'DELETED'",
                    [actor.uuid.to_string()], |row| row.get(0)).map_err(sql_error)?;
                if count >= 1000 {
                    return Err(ErrorCode::LimitExceeded);
                }
                transaction.execute(
                    "INSERT INTO library_models VALUES
                    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'ACTIVE', 1, 1, ?12, ?12, 0)",
                    params![
                        reservation.model_id,
                        actor.uuid.to_string(),
                        name,
                        input.metadata.display_name,
                        input.metadata.internal_name,
                        serde_json::to_string(&input.metadata.authors).map_err(|_| ErrorCode::Internal)?,
                        input.metadata.model_type,
                        input.metadata.category_id,
                        input.metadata.description,
                        visibility,
                        input.metadata.requires_authorization,
                        now,
                    ],
                ).map_err(sql_error)?;
                let metadata_validation = if input.metadata.model_type == "YSM_ENCRYPTED" {
                    "CLIENT_EXTRACTED"
                } else {
                    "SERVER_VERIFIED"
                };
                transaction.execute(
                    "INSERT INTO library_revisions VALUES (?1, 1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        reservation.model_id,
                        actor.uuid.to_string(),
                        descriptor.asset_id,
                        input.raw_sha256,
                        library::sql_integer(input.byte_length)?,
                        descriptor.distribution_format.to_string(),
                        input.metadata_source,
                        metadata_validation,
                        input.source_filename,
                    ],
                ).map_err(sql_error)?;
                // The resource does not exist yet, but its reserved name/revision is already protected.
                transaction.execute("INSERT INTO library_resource_bindings VALUES (?1, ?2, 1, ?3, 1)",
                    params![actor.uuid.to_string(), descriptor.asset_id, reservation.model_id]).map_err(sql_error)?;
                for tag in &input.metadata.tags {
                    transaction.execute("INSERT INTO library_tags VALUES (?1, ?2, ?3)",
                        params![reservation.model_id, tag.to_lowercase(), tag]).map_err(sql_error)?;
                }
                serde_json::to_vec(&reservation).map_err(|_| ErrorCode::Internal)
            })?;
            let reservation: Reservation = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Conflict)?;
            if reservation.kind != "upload" {
                return Err(ErrorCode::Conflict);
            }
            Ok(reservation)
        })
    }

    pub fn library_reservation(&self, owner: Uuid, operation: &str) -> Result<Reservation> {
        super::super::resource::operation_id(operation)?;
        self.library_read(|database| {
            let bytes: Vec<u8> = database.query_row("SELECT result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
                params![owner.to_string(), operation], |row| row.get(0)).optional().map_err(sql_error)?.ok_or(ErrorCode::NotFound)?;
            let reservation: Reservation = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::NotFound)?;
            if reservation.kind != "upload" {
                return Err(ErrorCode::NotFound);
            }
            Ok(reservation)
        })
    }

    pub fn library_asset_bound(&self, owner: Uuid, asset: &str) -> Result<bool> {
        self.library_read(|database| database.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1 AND asset_id = ?2)",
            params![owner.to_string(), asset], |row| row.get(0)).map_err(sql_error))
    }

    pub fn library_upload_pending(&self, actor: Actor, operation: &str) -> Result<Reservation> {
        let reservation = self.library_reservation(actor.uuid, operation)?;
        self.library_read(|database| {
            let item = model(database, &reservation.model_id, actor, true)?;
            if item.published || item.policy.moderation != Moderation::Active {
                return Err(ErrorCode::Conflict);
            }
            Ok(())
        })?;
        Ok(reservation)
    }

    pub fn library_cancel(
        &self,
        actor: Actor,
        operation: &str,
        input: &OperationInput,
    ) -> Result<Value> {
        let reservation = self.library_reservation(actor.uuid, operation)?;
        let operation_id = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(format!("cancel:{operation}").as_bytes());
        self.library_transaction(|transaction| {
            let bytes = library::operation(transaction, actor.uuid, operation_id, &fingerprint, now_seconds(), |transaction| {
                let published: bool = transaction.query_row("SELECT published FROM library_models WHERE model_id = ?1", [&reservation.model_id], |row| row.get(0)).map_err(sql_error)?;
                if published {
                    return Err(ErrorCode::Conflict);
                }
                transaction.execute("UPDATE library_models SET moderation_status = 'DELETED', policy_revision = policy_revision + 1 WHERE model_id = ?1", [&reservation.model_id]).map_err(sql_error)?;
                serde_json::to_vec(&json!({"cancelled": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        })
    }
}

pub struct CommitContext {
    pub actor: Actor,
    pub reservation: Reservation,
    pub input: Mutation,
}

impl CommitContext {
    pub fn check_replay(&self, storage: &Storage) -> Result<()> {
        let operation_id = super::super::resource::operation_id(&self.input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["commit", self.reservation.model_id, self.input]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        storage.library_transaction(|transaction| {
            library::operation(
                transaction,
                self.actor.uuid,
                operation_id,
                &fingerprint,
                now_seconds(),
                |_| Err(ErrorCode::Conflict),
            )?;
            Ok(())
        })
    }
    pub fn publish(
        &self,
        transaction: &Transaction<'_>,
        descriptor: &AvatarDescriptor,
        inspected: &super::super::package::PackageIdentity,
    ) -> Result<()> {
        let id = &self.reservation.model_id;
        let item = model(transaction, id, self.actor, true)?;
        let expected = &self.reservation.request.metadata;
        if inspected.model_type != ModelType::ModelYsmEncrypted
            && (inspected.internal_name.as_ref() != Some(&expected.internal_name)
                || inspected.authors_text != expected.authors.join("\n"))
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let operation_id = super::super::resource::operation_id(&self.input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["commit", id, self.input]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        library::operation(
            transaction,
            self.actor.uuid,
            operation_id,
            &fingerprint,
            now_seconds(),
            |transaction| {
                if item.published
                    || item.policy.moderation != Moderation::Active
                    || item.summary.policy_revision
                        != library::sql_integer(self.input.expected_revision)?
                {
                    return Err(ErrorCode::Conflict);
                }
                transaction.execute("UPDATE library_revisions SET resource_revision = ?2 WHERE model_id = ?1 AND revision = 1", params![id, library::sql_integer(descriptor.revision)?]).map_err(sql_error)?;
                transaction.execute("UPDATE library_resource_bindings SET revision = ?2 WHERE source_model_id = ?1", params![id, library::sql_integer(descriptor.revision)?]).map_err(sql_error)?;
                let mut identities = vec![
                    ("RAW_SHA256", descriptor.raw_sha256.clone()),
                    ("NBT_SHA256", inspected.nbt_sha256.clone()),
                ];
                if let Some(ciphertext) = &inspected.ciphertext_sha256 {
                    identities.push(("YSM_CIPHERTEXT_SHA256", ciphertext.clone()));
                }
                transaction.execute(
                    "INSERT OR IGNORE INTO library_revision_hashes VALUES (?1, 1, 'LEGACY_FIGURA_BASE64_SHA256', ?2)",
                    params![id, descriptor.legacy_figura_hash],
                ).map_err(sql_error)?;
                for (kind, hash) in &identities {
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO library_revision_hashes VALUES (?1, 1, ?2, ?3)",
                            params![id, kind, faster_hex::hex_string(hash)],
                        )
                        .map_err(sql_error)?;
                }
                // A known banned package cannot be republished under a fresh library ID.
                let banned: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM library_revision_hashes r JOIN library_active_blocked_hashes b
                ON b.hash_kind = r.hash_kind AND b.verified_hash = r.verified_hash WHERE r.model_id = ?1 AND b.active = 1)", [id], |row| row.get(0)).map_err(sql_error)?;
                transaction
                    .execute(
                        "UPDATE library_models SET published = 1,
                moderation_status = CASE WHEN ?2 THEN 'BLOCKED' ELSE 'ACTIVE' END,
                visibility = CASE WHEN ?2 THEN 'PRIVATE' ELSE visibility END WHERE model_id = ?1",
                        params![id, banned],
                    )
                    .map_err(sql_error)?;
                ensure_uploader_grant(transaction, id, self.actor.uuid)?;
                if !banned {
                    transaction.execute(
                        "INSERT OR IGNORE INTO library_collection
                         (player_uuid, model_id, joined_at, join_source)
                         VALUES (?1, ?2, ?3, 'UPLOAD')",
                        params![self.actor.uuid.to_string(), id, library::sql_integer(now_seconds())?],
                    ).map_err(sql_error)?;
                }
                if banned {
                    library::set_blocked(
                        transaction,
                        id,
                        self.actor.uuid,
                        self.input.expected_revision,
                        true,
                        "verified identity already blocked",
                        now_seconds(),
                    )?;
                } else {
                    advance(transaction, id)?;
                }
                serde_json::to_vec(&json!({"model_id": id, "published": true}))
                    .map_err(|_| ErrorCode::Internal)
            },
        )?;
        Ok(())
    }
}

impl Storage {
    pub fn library_resource_access(
        &self,
        owner: Uuid,
        asset: &str,
        revision: u64,
        actor: Actor,
    ) -> Result<()> {
        match self.avatar(owner, asset, revision) {
            Ok(avatar) => self.library_ensure_verified_blob(&avatar)?,
            Err(ErrorCode::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.library_read(|database| {
            super::distribution::resource_access(database, owner, asset, revision, actor)
        })
    }
}
