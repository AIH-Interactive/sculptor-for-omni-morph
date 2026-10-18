//! Source-bound rendering is distinct from catalog download permission.
use super::*;
use crate::protocol::v1::{AvatarDescriptor, EquippedState};
use crate::service::event::CloudEvent;

pub(crate) async fn can_observe_model(
    state: &crate::AppState,
    owner: Uuid,
    viewer: Uuid,
) -> Result<bool> {
    let storage = state.storage.clone();
    super::work(move || {
        let equipped = storage.library_project_equipped(owner, viewer)?;
        // Preserve ordinary recovery errors when no model is equipped.
        // An equipped but hidden model must not expose recovery data.
        Ok(equipped.primary.is_some() || storage.equipped(owner)?.primary.is_none())
    })
    .await
}

/// Recheck at the connection's send boundary as well as before recovery/queueing.
pub(crate) async fn event_visible(
    state: &crate::AppState,
    viewer: Uuid,
    event: &CloudEvent,
) -> Result<bool> {
    use crate::protocol::v1::envelope::Body;
    let (owner, body) = match event {
        CloudEvent::AvatarChanged { .. } | CloudEvent::LegacyRaw(_) => return Ok(true),
        CloudEvent::FiguraPing { owner, .. } => (*owner, None),
        CloudEvent::Ysm {
            owner,
            body: Body::SnapshotRequest(_),
            ..
        } => return Ok(*owner == viewer),
        CloudEvent::Ysm { owner, body, .. } => (*owner, Some(body.clone())),
    };
    let storage = state.storage.clone();
    super::work(move || {
        let equipped = storage.library_project_equipped(owner, viewer)?;
        let Some(body) = body else {
            // Unmodeled legacy pings keep their original behavior; a hidden
            // equipped model must not continue sending its script arguments.
            return Ok(equipped.primary.is_some() || storage.equipped(owner)?.primary.is_none());
        };
        if matches!(body, Body::Error(_)) {
            return Ok(equipped.primary.is_some() || storage.equipped(owner)?.primary.is_none());
        }
        let Some(avatar) = equipped.primary else {
            return Ok(false);
        };
        if !matches!(avatar.model_type, 2 | 3) {
            return Ok(false);
        }
        let id = crate::service::ysm::identity(&body)?;
        if id.owner != owner.to_string()
            || id.asset_revision != avatar.revision
            || id.content_sha256 != avatar.content_sha256
            || (id.schema_sha256 != avatar.schema_sha256
                && !(avatar.asset_id == "avatar" && avatar.schema_sha256.is_empty()))
        {
            return Ok(false);
        }
        let snapshot = match storage.ysm_snapshot(owner, now_seconds()) {
            Ok(snapshot) => snapshot,
            Err(ErrorCode::SnapshotRequired | ErrorCode::NotFound | ErrorCode::Conflict) => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let current = snapshot.identity.ok_or(ErrorCode::Internal)?;
        Ok(current.publisher_session == id.publisher_session
            && current.generation == id.generation
            && current.sequence >= id.sequence)
    })
    .await
}

pub(crate) fn record_identity(
    transaction: &Transaction<'_>,
    avatar: &AvatarDescriptor,
    inspected: &crate::service::package::PackageIdentity,
) -> Result<()> {
    let raw_hash = faster_hex::hex_string(&avatar.raw_sha256);
    let mut hashes = vec![
        ("RAW_SHA256", raw_hash.clone()),
        ("NBT_SHA256", faster_hex::hex_string(&inspected.nbt_sha256)),
    ];
    if let Some(ciphertext) = &inspected.ciphertext_sha256 {
        hashes.push(("YSM_CIPHERTEXT_SHA256", faster_hex::hex_string(ciphertext)));
    }
    if !avatar.legacy_figura_hash.is_empty() {
        hashes.push((
            "LEGACY_FIGURA_BASE64_SHA256",
            avatar.legacy_figura_hash.clone(),
        ));
    }
    for (kind, hash) in hashes {
        library::validate_hash(kind, &hash)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO library_verified_blobs VALUES (?1, ?2, ?3)",
                params![raw_hash, kind, hash],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

impl Storage {
    /// Called on a bounded worker before read policy evaluation. Old blobs are
    /// scanned once; neither client-declared content hashes nor names prove identity.
    pub fn library_ensure_verified_blob(&self, avatar: &AvatarDescriptor) -> Result<()> {
        let raw_hash = faster_hex::hex_string(&avatar.raw_sha256);
        library::validate_hash("RAW_SHA256", &raw_hash)?;
        let verified: bool = self.library_read(|database| {
            database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_verified_blobs WHERE raw_sha256 = ?1)",
                    [&raw_hash],
                    |row| row.get(0),
                )
                .map_err(sql_error)
        })?;
        if verified {
            return Ok(());
        }
        let path = self.blob_path(&avatar.raw_sha256)?;
        let mut file = std::fs::File::open(&path).map_err(|_| ErrorCode::NotFound)?;
        if file.metadata().map_err(|_| ErrorCode::Internal)?.len() != avatar.raw_length {
            return Err(ErrorCode::InvalidMessage);
        }
        use std::io::Read;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 64 * 1024];
        loop {
            let length = file.read(&mut buffer).map_err(|_| ErrorCode::Internal)?;
            if length == 0 {
                break;
            }
            hash.update(&buffer[..length]);
        }
        if hash.finish().as_ref() != avatar.raw_sha256 {
            return Err(ErrorCode::InvalidMessage);
        }
        let inspected = crate::service::package::inspect(&path)?;
        if inspected.model_type as i32 != avatar.model_type {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_transaction(|transaction| record_identity(transaction, avatar, &inspected))
    }

    pub fn library_has_equipped_source(&self, owner: Uuid) -> Result<bool> {
        self.library_read(|database| {
            database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM equipment e JOIN library_resource_bindings b
                     ON b.resource_owner = e.owner_uuid AND b.asset_id = e.asset_id
                        AND b.revision = e.revision
                 WHERE e.owner_uuid = ?1 AND e.slot = 'primary')",
                    [owner.to_string()],
                    |row| row.get(0),
                )
                .map_err(sql_error)
        })
    }

    pub fn library_project_equipped(&self, owner: Uuid, observer: Uuid) -> Result<EquippedState> {
        let mut equipped = self.equipped(owner)?;
        // Revocation stops distribution, without clearing the wearer's local selection.
        if owner == observer {
            return Ok(equipped);
        }
        for avatar in [&equipped.primary, &equipped.figura_compatibility]
            .into_iter()
            .flatten()
        {
            self.library_ensure_verified_blob(avatar)?;
        }
        let actor = Actor {
            uuid: observer,
            admin_context: false,
            granted: false,
        };
        self.library_read(|database| {
            for selected in [&mut equipped.primary, &mut equipped.figura_compatibility] {
                let Some(avatar) = selected.as_ref() else {
                    continue;
                };
                match resource_access(database, owner, &avatar.asset_id, avatar.revision, actor) {
                    Ok(()) => {}
                    Err(ErrorCode::Forbidden | ErrorCode::NotFound) => *selected = None,
                    Err(error) => return Err(error),
                }
            }
            Ok(equipped)
        })
    }
}

pub(super) fn resource_access(
    database: &Connection,
    owner: Uuid,
    asset: &str,
    revision: u64,
    observer: Actor,
) -> Result<()> {
    if observer.uuid != owner {
        let banned: bool = database
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM avatars a JOIN library_verified_blobs v
                 ON v.raw_sha256 = lower(hex(a.raw_sha256))
             JOIN library_active_blocked_hashes b ON b.hash_kind = v.hash_kind
                 AND b.verified_hash = v.verified_hash AND b.active = 1
             WHERE a.owner_uuid = ?1 AND a.asset_id = ?2 AND a.revision = ?3)",
                params![owner.to_string(), asset, library::sql_integer(revision)?],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if banned {
            return Err(ErrorCode::NotFound);
        }
    }
    let source: Option<(String, bool)> = database
        .query_row(
            "SELECT b.source_model_id,
                b.resource_owner = r.resource_owner AND b.asset_id = r.asset_id
                    AND b.revision = r.resource_revision
         FROM library_resource_bindings b JOIN library_revisions r
           ON r.model_id = b.source_model_id AND r.revision = b.source_revision
         WHERE b.resource_owner = ?1 AND b.asset_id = ?2 AND b.revision = ?3",
            params![owner.to_string(), asset, library::sql_integer(revision)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((source, canonical)) = source else {
        if observer.uuid != owner {
            let hidden: bool = database.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_wardrobe_settings WHERE owner_uuid = ?1
                 AND asset_id = ?2 AND asset_revision = ?3 AND show_equipment = 0)",
                params![owner.to_string(), asset, library::sql_integer(revision)?], |row| row.get(0),
            ).map_err(sql_error)?;
            if hidden {
                return Err(ErrorCode::NotFound);
            }
        }
        return Ok(());
    };
    if canonical {
        return require_action(&model(database, &source, observer, false)?, "download");
    }

    // A grant held by an observer cannot keep a revoked wearer's alias distributable.
    let wearer = Actor {
        uuid: owner,
        admin_context: false,
        granted: false,
    };
    let item = model(database, &source, wearer, false)?;
    require_action(&item, "equip")?;
    if observer.uuid == owner {
        return Ok(());
    }
    let show_private: bool = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM equipment e JOIN library_equipment_settings s
             ON s.resource_owner = e.owner_uuid AND s.asset_id = e.asset_id
                AND s.revision = e.revision
         WHERE e.owner_uuid = ?1 AND e.slot = 'primary' AND e.asset_id = ?2
           AND e.revision = ?3 AND s.show_private_equipment = 1)",
            params![owner.to_string(), asset, library::sql_integer(revision)?],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let granted: bool = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM library_grants WHERE model_id = ?1 AND player_uuid = ?2)",
            params![source, observer.uuid.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if item
        .policy
        .can_render_equipment(owner, observer.uuid, granted, show_private)
    {
        Ok(())
    } else if item.policy.visibility == Visibility::Private {
        Err(ErrorCode::NotFound)
    } else {
        Err(ErrorCode::Forbidden)
    }
}
