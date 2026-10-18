//! Read-only administrator preview of a selected wardrobe source, never observer access.
use super::*;
use crate::service::library_policy::Actor;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewQuery {
    pub owner: Option<Uuid>,
    pub asset_id: Option<String>,
    pub asset_revision: Option<u64>,
    pub equipment_revision: Option<u64>,
}

impl Storage {
    pub(crate) fn library_admin_preview(
        &self,
        actor: Actor,
        kind: &str,
        id: Uuid,
        query: &PreviewQuery,
    ) -> Result<AvatarDescriptor> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if kind == "RECYCLE" {
            let owner = query.owner.ok_or(ErrorCode::InvalidMessage)?;
            return self.wardrobe_recycle_source(actor, owner, id).map(|(avatar, _)| avatar);
        }
        let (owner, asset, revision) = match kind {
            "CURRENT" => {
                let equipment = self.equipped(id)?;
                let avatar = equipment.primary.ok_or(ErrorCode::NotFound)?;
                if query.asset_id.as_deref() != Some(avatar.asset_id.as_str())
                    || query.asset_revision != Some(avatar.revision)
                    || query.equipment_revision != Some(equipment.revision)
                {
                    return Err(ErrorCode::Conflict);
                }
                if self.library_asset_bound(id, &avatar.asset_id)? {
                    return Err(ErrorCode::Forbidden);
                }
                (id, avatar.asset_id, avatar.revision)
            }
            "BLOCK" => self.library_read(|database| {
                let (owner, asset, revision): (String, String, i64) = database
                    .query_row(
                        "SELECT owner_uuid, asset_id, asset_revision FROM library_wardrobe_blocks
                         WHERE operation_id = ?1 LIMIT 1",
                        [id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::QueryReturnedNoRows => ErrorCode::NotFound,
                        other => sqlite::sql_error(other),
                    })?;
                Ok((
                    Uuid::parse_str(&owner).map_err(|_| ErrorCode::Internal)?,
                    asset,
                    u64::try_from(revision).map_err(|_| ErrorCode::Internal)?,
                ))
            })?,
            _ => return Err(ErrorCode::InvalidMessage),
        };
        self.avatar(owner, &asset, revision)
    }
}
