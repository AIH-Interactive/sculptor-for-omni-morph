//! Small JSON publications share the existing catalog, policy, images and membership.
use super::*;
use crate::service::outfit::{OutfitConfig, ResourceKind, ResourceRef};
use crate::service::outfit_limits::OutfitLimits;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutfitMetadata {
    pub resource_kind: ResourceKind,
    pub model_type: Option<()>,
    pub display_name: String,
    pub internal_name: String,
    pub authors: Vec<String>,
    pub category_id: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub visibility: Visibility,
    pub requires_authorization: bool,
}

impl OutfitMetadata {
    fn validate(&self, categories: &[String]) -> Result<()> {
        if self.resource_kind != ResourceKind::Outfit
            || self.model_type.is_some()
            || !categories.contains(&self.category_id)
            || self.tags.len() > 8
        {
            return Err(ErrorCode::InvalidMessage);
        }
        text(&self.display_name, 96, false)?;
        text(&self.internal_name, 96, false)?;
        text(&self.description, 4096, true)?;
        let mut authors = std::collections::HashSet::new();
        for author in &self.authors {
            text(author, usize::MAX, false)?;
            if author.trim() != author || !authors.insert(author) {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        let mut tags = std::collections::HashSet::new();
        for tag in &self.tags {
            text(tag, 32, false)?;
            if tag.trim() != tag || !tags.insert(tag.to_lowercase()) {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutfitUploadInput {
    pub operation_id: Uuid,
    pub metadata: OutfitMetadata,
    pub configuration: OutfitConfig,
    #[serde(default, skip_serializing_if = "local_conversion_disabled")]
    pub convert_local_dependencies: bool,
}

fn local_conversion_disabled(requested: &bool) -> bool {
    !requested
}

fn dependency(
    database: &Connection,
    actor: Actor,
    origin: &str,
    reference: &ResourceRef,
    kind: ResourceKind,
) -> Result<()> {
    let ResourceRef::Cloud {
        origin: resource_origin,
        resource_id,
        revision,
        content_hash,
        format,
    } = reference
    else {
        return Err(ErrorCode::InvalidMessage);
    };
    // This service can authorize only its own resources; publication never grants or uploads a dependency.
    if resource_origin != origin {
        return Err(ErrorCode::UnsupportedCapability);
    }
    let id = resource_id.to_string();
    let item = model(database, &id, actor, false)?;
    require_action(&item, "preview")?;
    if item.summary.resource_kind != kind
        || item.summary.revision != library::sql_integer(*revision)?
        || item.summary.model_type.as_deref() != Some(format)
    {
        return Err(ErrorCode::Conflict);
    }
    let actual: String = database
        .query_row(
            "SELECT raw_sha256 FROM library_revisions WHERE model_id = ?1 AND revision = ?2",
            params![id, library::sql_integer(*revision)?],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if actual != *content_hash {
        return Err(ErrorCode::Conflict);
    }
    Ok(())
}

impl Storage {
    pub fn library_publish_outfit(
        &self,
        actor: Actor,
        name: &str,
        origin: &str,
        input: &OutfitUploadInput,
        categories: &[String],
        limits: &OutfitLimits,
    ) -> Result<Value> {
        input.metadata.validate(categories)?;
        text(name, 64, false)?;
        limits.validate()?;
        input.configuration.validate_with_limits(limits)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&("publish-outfit", origin, input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        // A lost publication reply must replay even if the original local source has since disappeared.
        let replay = self.library_transaction(|transaction| {
            library::operation(
                transaction,
                actor.uuid,
                input.operation_id,
                &fingerprint,
                now_seconds(),
                |_| Err(ErrorCode::NotFound),
            )
        });
        match replay {
            Ok(bytes) => return serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal),
            Err(ErrorCode::NotFound) => {}
            Err(error) => return Err(error),
        }
        let local_sources =
            super::outfit_local_publication::prepare(self, actor, input, categories, limits)?;
        let assets: Vec<_> = local_sources
            .iter()
            .map(|source| source.descriptor.clone())
            .collect();
        let publish = |transaction: &Transaction<'_>| {
            let bytes = library::operation(
                transaction,
                actor.uuid,
                input.operation_id,
                &fingerprint,
                now_seconds(),
                |transaction| {
                    let configuration = super::outfit_local_publication::promote(
                        transaction,
                        actor,
                        name,
                        origin,
                        &input.configuration,
                        &local_sources,
                        limits,
                    )?;
                    if let Some(base) = &configuration.base {
                        dependency(transaction, actor, origin, base, ResourceKind::Avatar)?;
                    }
                    for instance in &configuration.instances {
                        dependency(
                            transaction,
                            actor,
                            origin,
                            &instance.resource,
                            ResourceKind::Accessory,
                        )?;
                    }
                    let count: i64 = transaction.query_row(
                        "SELECT count(*) FROM library_models WHERE uploader_uuid = ?1 AND moderation_status != 'DELETED'",
                        [actor.uuid.to_string()], |row| row.get(0),
                    ).map_err(sql_error)?;
                    if count >= 1000 {
                        return Err(ErrorCode::LimitExceeded);
                    }
                    let id = random_id();
                    let now = library::sql_integer(now_seconds())?;
                    let metadata = &input.metadata;
                    let visibility = match metadata.visibility {
                        Visibility::Public => "PUBLIC",
                        Visibility::Private => "PRIVATE",
                    };
                    transaction.execute(
                        "INSERT INTO library_models VALUES
                        (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, 'ACTIVE', 1, 1, ?11, ?11, 1)",
                        params![id, actor.uuid.to_string(), name, metadata.display_name, metadata.internal_name,
                            serde_json::to_string(&metadata.authors).map_err(|_| ErrorCode::Internal)?,
                            metadata.category_id, metadata.description, visibility, metadata.requires_authorization, now],
                    ).map_err(sql_error)?;
                    transaction
                        .execute(
                            "INSERT INTO library_resource_kinds VALUES (?1, 'OUTFIT')",
                            [&id],
                        )
                        .map_err(sql_error)?;
                    transaction
                        .execute(
                            "INSERT INTO library_outfit_contents VALUES (?1, 1, ?2)",
                            params![
                                id,
                                serde_json::to_string(&configuration)
                                    .map_err(|_| ErrorCode::Internal)?
                            ],
                        )
                        .map_err(sql_error)?;
                    for tag in &metadata.tags {
                        transaction
                            .execute(
                                "INSERT INTO library_tags VALUES (?1, ?2, ?3)",
                                params![id, tag.to_lowercase(), tag],
                            )
                            .map_err(sql_error)?;
                    }
                    ensure_uploader_grant(transaction, &id, actor.uuid)?;
                    transaction.execute(
                        "INSERT INTO library_collection (player_uuid, model_id, joined_at, join_source) VALUES (?1, ?2, ?3, 'UPLOAD')",
                        params![actor.uuid.to_string(), id, now],
                    ).map_err(sql_error)?;
                    advance(transaction, &id)?;
                    serde_json::to_vec(&json!({"model_id": id, "published": true, "revision": 1, "policy_revision": 2}))
                        .map_err(|_| ErrorCode::Internal)
                },
            )?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        };
        if assets.is_empty() {
            self.library_transaction(publish)
        } else {
            self.outfit_publish_library_assets(actor.uuid, &assets, publish)
        }
    }

    pub fn library_read_outfit(&self, actor: Actor, id: &str, revision: u64) -> Result<Value> {
        self.library_read(|database| {
            let item = model(database, id, actor, false)?;
            require_action(&item, "preview")?;
            if item.summary.resource_kind != ResourceKind::Outfit
                || item.summary.revision != library::sql_integer(revision)?
            {
                return Err(ErrorCode::Conflict);
            }
            let configuration: String = database.query_row(
                "SELECT configuration_json FROM library_outfit_contents WHERE model_id = ?1 AND revision = ?2",
                params![id, library::sql_integer(revision)?], |row| row.get(0),
            ).map_err(sql_error)?;
            let configuration: Value = serde_json::from_str(&configuration).map_err(|_| ErrorCode::Internal)?;
            Ok(json!({"model_id": id, "revision": revision, "configuration": configuration}))
        })
    }
}
