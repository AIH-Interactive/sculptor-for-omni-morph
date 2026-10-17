//! Library metadata operations. All methods run on the bounded blocking path.
pub mod images;
pub mod transfers;
pub mod uploads;
use super::library_policy::{Actor, Moderation, Policy, Visibility};
use crate::{
    protocol::v1::ErrorCode,
    storage::{Storage, library, now_seconds},
};
use rusqlite::{Connection, OptionalExtension, Transaction, named_params, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, ErrorCode>;

static WORKERS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(8)));

pub async fn work<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let permit = WORKERS
        .clone()
        .try_acquire_owned()
        .map_err(|_| ErrorCode::Offline)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| ErrorCode::Internal)?
}

pub(crate) fn sql_error(error: rusqlite::Error) -> ErrorCode {
    // Do not log parameter values or private metadata on a failed statement.
    tracing::error!(code = ?error.sqlite_error_code(), "Library database operation failed");
    ErrorCode::Internal
}

pub fn random_id() -> String {
    // Use a canonical UUID with random bytes; no client-controlled path segments.
    let mut bytes: [u8; 16] = crate::utils::rand()[..16]
        .try_into()
        .expect("sixteen bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub display_name: String,
    pub internal_name: String,
    pub authors: Vec<String>,
    pub model_type: String,
    pub category_id: String,
    pub description: String,
    pub tags: Vec<String>,
    pub visibility: Visibility,
    pub requires_authorization: bool,
}

pub fn text(value: &str, maximum: usize, empty: bool) -> Result<()> {
    if value.len() > maximum
        || (!empty && value.trim().is_empty())
        || value.chars().any(|character| {
            character == '\0'
                || character == '§'
                || (character.is_control() && character != '\n' && character != '\t')
        })
    {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok(())
}

impl Metadata {
    pub fn validate(&self, categories: &[String]) -> Result<()> {
        text(&self.display_name, 96, false)?;
        text(&self.internal_name, 96, false)?;
        text(&self.description, 4096, true)?;
        if !matches!(
            self.model_type.as_str(),
            "FIGURA" | "YSM_PLAIN" | "YSM_ENCRYPTED"
        ) || !categories.contains(&self.category_id)
            || self.authors.len() > 4
            || self.tags.len() > 8
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let mut authors = std::collections::HashSet::new();
        for author in &self.authors {
            text(author, 48, false)?;
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

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub model_id: String,
    pub revision: i64,
    pub policy_revision: i64,
    pub display_name: String,
    pub internal_name: String,
    pub authors: Vec<String>,
    pub uploader: Uploader,
    pub model_type: String,
    pub image_revision: i64,
    pub image_available: bool,
    pub downloads: i64,
    pub favorites: i64,
    pub favorited: bool,
    pub visibility: Visibility,
    pub requires_authorization: bool,
    pub allowed_actions: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Uploader {
    pub uuid: String,
    pub name: String,
}

pub(crate) struct Model {
    pub summary: Summary,
    pub policy: Policy,
    pub description: String,
    pub category_id: String,
    pub moderation: String,
    pub published: bool,
    pub updated_at: i64,
}

const MODEL_SELECT: &str = "SELECT m.model_id, m.uploader_uuid, m.uploader_name_snapshot,
    m.display_name, m.internal_name, m.authors_json, m.model_type, m.category_id,
    m.description, m.visibility, m.requires_authorization, m.moderation_status,
    m.current_revision, m.policy_revision, m.updated_at, m.published,
    COALESCE((SELECT image_revision FROM library_images i WHERE i.model_id = m.model_id), 0),
    COALESCE((SELECT downloads FROM library_stats s WHERE s.model_id = m.model_id), 0),
    (SELECT count(*) FROM library_favorites f WHERE f.model_id = m.model_id),
    EXISTS(SELECT 1 FROM library_favorites f WHERE f.model_id = m.model_id AND f.player_uuid = :actor)
    FROM library_models m";

fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Model> {
    let uploader: String = row.get(1)?;
    let visibility: String = row.get(9)?;
    let moderation: String = row.get(11)?;
    // SQL CHECK constraints guarantee enums; parse failures are database corruption.
    let bad_data = || rusqlite::Error::InvalidQuery;
    let policy = Policy {
        uploader: Uuid::parse_str(&uploader).map_err(|_| bad_data())?,
        visibility: match visibility.as_str() {
            "PUBLIC" => Visibility::Public,
            "PRIVATE" => Visibility::Private,
            _ => return Err(bad_data()),
        },
        requires_authorization: row.get(10)?,
        moderation: match moderation.as_str() {
            "ACTIVE" => Moderation::Active,
            "BLOCKED" => Moderation::Blocked,
            "DELETED" => Moderation::Deleted,
            _ => return Err(bad_data()),
        },
    };
    let image_revision = row.get(16)?;
    Ok(Model {
        summary: Summary {
            model_id: row.get(0)?,
            revision: row.get(12)?,
            policy_revision: row.get(13)?,
            display_name: row.get(3)?,
            internal_name: row.get(4)?,
            authors: serde_json::from_str(&row.get::<_, String>(5)?).map_err(|_| bad_data())?,
            uploader: Uploader {
                uuid: uploader,
                name: row.get(2)?,
            },
            model_type: row.get(6)?,
            image_revision,
            image_available: image_revision > 0,
            downloads: row.get(17)?,
            favorites: row.get(18)?,
            favorited: row.get(19)?,
            visibility: policy.visibility,
            requires_authorization: policy.requires_authorization,
            allowed_actions: Vec::new(),
        },
        policy,
        description: row.get(8)?,
        category_id: row.get(7)?,
        moderation,
        updated_at: row.get(14)?,
        published: row.get(15)?,
    })
}

pub(crate) fn model(
    database: &Connection,
    id: &str,
    actor: Actor,
    draft_owner: bool,
) -> Result<Model> {
    crate::service::resource::operation_id(id)?;
    let mut model = database
        .query_row(
            &format!("{MODEL_SELECT} WHERE m.model_id = :id"),
            named_params! {":id": id, ":actor": actor.uuid.to_string()},
            model_row,
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(ErrorCode::NotFound)?;
    let draft = draft_owner
        && actor.uuid == model.policy.uploader
        && model.policy.moderation == Moderation::Active;
    let permissions = model.policy.permissions(actor);
    if (!model.published && !draft) || !permissions.visible {
        return Err(ErrorCode::NotFound);
    }
    actions(database, &mut model, actor)?;
    Ok(model)
}

fn actions(database: &Connection, model: &mut Model, mut actor: Actor) -> Result<()> {
    actor.granted = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM library_grants WHERE model_id = ?1 AND player_uuid = ?2)",
            params![model.summary.model_id, actor.uuid.to_string()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let permissions = model.policy.permissions(actor);
    let mut allowed = Vec::new();
    if permissions.visible && model.policy.moderation == Moderation::Active {
        allowed.push("favorite");
    }
    if permissions.use_model && model.published {
        allowed.extend(["preview", "download", "equip"]);
    }
    if permissions.manage && model.policy.moderation != Moderation::Deleted {
        allowed.extend(["patch", "delete", "image", "grants"]);
    }
    if actor.admin_context {
        allowed.push("moderate");
    }
    model.summary.allowed_actions = allowed;
    Ok(())
}

pub(crate) fn require_action(model: &Model, action: &str) -> Result<()> {
    if model.summary.allowed_actions.contains(&action) {
        Ok(())
    } else {
        Err(ErrorCode::Forbidden)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListQuery {
    pub scope: String,
    pub q: String,
    pub category: String,
    pub tags: String,
    #[serde(rename = "type")]
    pub model_type: String,
    pub authorization: String,
    pub status: String,
    pub sort: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

impl Storage {
    pub fn library_list(&self, query: &ListQuery, mut actor: Actor) -> Result<Value> {
        text(&query.q, 128, true)?;
        let scope = if query.scope.is_empty() {
            "public"
        } else {
            &query.scope
        };
        if !matches!(scope, "public" | "mine" | "favorites" | "admin")
            || (scope == "admin" && !actor.admin_context)
        {
            return Err(ErrorCode::Forbidden);
        }
        actor.admin_context = scope == "admin";
        if (!query.model_type.is_empty()
            && !matches!(
                query.model_type.as_str(),
                "FIGURA" | "YSM_PLAIN" | "YSM_ENCRYPTED"
            ))
            || !matches!(query.authorization.as_str(), "" | "required" | "free")
            || (!query.status.is_empty()
                && (!actor.admin_context
                    || !matches!(query.status.as_str(), "ACTIVE" | "BLOCKED" | "DELETED")))
            || query.category.len() > 32
            || query.tags.len() > 264
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let page = query.page.unwrap_or(1);
        let size = query.page_size.unwrap_or(8);
        if page == 0 || page > 1_000_000 || !(1..=8).contains(&size) {
            return Err(ErrorCode::InvalidMessage);
        }
        let sort = match query.sort.as_str() {
            "" | "updated" => "m.updated_at DESC, m.model_id",
            "newest" => "m.created_at DESC, m.model_id",
            "downloads" => {
                "COALESCE((SELECT downloads FROM library_stats s WHERE s.model_id = m.model_id), 0) DESC, m.model_id"
            }
            "favorites" => {
                "(SELECT count(*) FROM library_favorites f WHERE f.model_id = m.model_id) DESC, m.model_id"
            }
            _ => return Err(ErrorCode::InvalidMessage),
        };
        let tags: Vec<_> = query
            .tags
            .split(',')
            .filter(|tag| !tag.is_empty())
            .map(str::to_lowercase)
            .collect();
        if tags.len() > 8 || tags.iter().any(|tag| tag.len() > 32) {
            return Err(ErrorCode::InvalidMessage);
        }
        let pattern = format!(
            "%{}%",
            query
                .q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let tags = serde_json::to_string(&tags).map_err(|_| ErrorCode::Internal)?;
        let predicate = r"m.published = 1
            AND (:admin OR (m.moderation_status = 'ACTIVE' AND (m.visibility = 'PUBLIC' OR m.uploader_uuid = :actor)))
            AND (:scope != 'public' OR m.visibility = 'PUBLIC')
            AND (:scope != 'mine' OR m.uploader_uuid = :actor)
            AND (:scope != 'favorites' OR EXISTS(SELECT 1 FROM library_favorites f WHERE f.model_id = m.model_id AND f.player_uuid = :actor))
            AND (:category = '' OR m.category_id = :category)
            AND (:type = '' OR m.model_type = :type)
            AND (:status = '' OR m.moderation_status = :status)
            AND (:authorization = '' OR m.requires_authorization = (:authorization = 'required'))
            AND (m.display_name LIKE :q ESCAPE '\' OR m.internal_name LIKE :q ESCAPE '\'
                OR EXISTS(SELECT 1 FROM json_each(m.authors_json) a WHERE a.value LIKE :q ESCAPE '\'))
            AND NOT EXISTS(SELECT 1 FROM json_each(:tags) wanted WHERE NOT EXISTS
                (SELECT 1 FROM library_tags t WHERE t.model_id = m.model_id AND t.normalized_tag = wanted.value))";
        self.library_read(|database| {
            let parameters = named_params! {
                ":actor": actor.uuid.to_string(), ":admin": actor.admin_context, ":scope": scope,
                ":category": query.category, ":type": query.model_type, ":status": query.status,
                ":authorization": query.authorization, ":q": pattern, ":tags": tags,
            };
            let total: i64 = database
                .query_row(
                    &format!("SELECT count(*) FROM library_models m WHERE {predicate}"),
                    parameters,
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            // Limit/offset are validated integers; sort is selected from literal SQL only.
            let mut statement = database
                .prepare(&format!(
                    "{MODEL_SELECT} WHERE {predicate} ORDER BY {sort} LIMIT {size} OFFSET {}",
                    (u64::from(page) - 1) * u64::from(size)
                ))
                .map_err(sql_error)?;
            let rows = statement
                .query_map(parameters, model_row)
                .map_err(sql_error)?;
            let mut items = Vec::new();
            for row in rows {
                let mut item = row.map_err(sql_error)?;
                actions(database, &mut item, actor)?;
                items.push(item.summary);
            }
            Ok(
                json!({"items": items, "total": total, "page": page, "page_size": size,
                "has_next": u64::from(page) * u64::from(size) < total as u64}),
            )
        })
    }

    pub fn library_detail(&self, id: &str, actor: Actor) -> Result<Value> {
        self.library_read(|database| {
            let item = model(database, id, actor, false)?;
            let mut detail = serde_json::to_value(&item.summary).map_err(|_| ErrorCode::Internal)?;
            let revision: Option<(i64, i64, String, String)> = database.query_row(
                "SELECT byte_length, resource_revision, metadata_source, metadata_validation FROM library_revisions WHERE model_id = ?1 AND revision = ?2",
                params![id, item.summary.revision], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(sql_error)?;
            let mut statement = database.prepare("SELECT display_tag FROM library_tags WHERE model_id = ?1 ORDER BY normalized_tag").map_err(sql_error)?;
            let tags: Vec<String> = statement.query_map([id], |row| row.get(0)).map_err(sql_error)?
                .collect::<rusqlite::Result<_>>().map_err(sql_error)?;
            detail["description"] = json!(item.description);
            detail["category_id"] = json!(item.category_id);
            detail["tags"] = json!(tags);
            detail["updated_at"] = json!(item.updated_at);
            detail["moderation_status"] = json!(item.moderation);
            if let Some((length, resource_revision, source, validation)) = revision {
                detail["byte_length"] = json!(length);
                detail["resource_revision"] = json!(resource_revision);
                detail["metadata_source"] = json!(source);
                detail["metadata_validation"] = json!(validation);
            }
            Ok(detail)
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub operation_id: String,
    pub expected_revision: u64,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub requires_authorization: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub(crate) fn mutate(
    storage: &Storage,
    actor: Actor,
    id: &str,
    input: &Mutation,
    action: &str,
    change: impl FnOnce(&Transaction<'_>, &Model) -> Result<Value>,
) -> Result<Value> {
    let operation = super::resource::operation_id(&input.operation_id)?;
    let request =
        serde_json::to_vec(&json!([id, action, input])).map_err(|_| ErrorCode::Internal)?;
    storage.library_transaction(|transaction| {
        // Recheck access even on replay; old cached mutation responses must not leak revoked metadata.
        let item = model(transaction, id, actor, true)?;
        let result = library::operation(
            transaction,
            actor.uuid,
            operation,
            &crate::storage::sha256(&request),
            now_seconds(),
            |transaction| {
                if item.summary.policy_revision != library::sql_integer(input.expected_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                let result = change(transaction, &item)?;
                serde_json::to_vec(&result).map_err(|_| ErrorCode::Internal)
            },
        )?;
        serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
    })
}

pub(crate) fn advance(transaction: &Transaction<'_>, id: &str) -> Result<()> {
    transaction.execute("UPDATE library_models SET policy_revision = policy_revision + 1, updated_at = ?2 WHERE model_id = ?1",
        params![id, library::sql_integer(now_seconds())?]).map_err(sql_error)?;
    Ok(())
}

pub(crate) fn ensure_uploader_grant(
    transaction: &Transaction<'_>,
    id: &str,
    actor: Uuid,
) -> Result<()> {
    transaction.execute("INSERT OR IGNORE INTO library_grants(model_id, player_uuid, name_snapshot, granted_by, created_at)
        SELECT model_id, uploader_uuid, uploader_name_snapshot, ?2, ?3 FROM library_models
        WHERE model_id = ?1 AND requires_authorization = 1",
        params![id, actor.to_string(), library::sql_integer(now_seconds())?]).map_err(sql_error)?;
    Ok(())
}

impl Storage {
    pub fn library_patch(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
        categories: &[String],
    ) -> Result<Value> {
        mutate(self, actor, id, input, "patch", |transaction, item| {
            require_action(item, "patch")?;
            if input.reason.is_some() {
                return Err(ErrorCode::InvalidMessage);
            }
            if let Some(name) = &input.display_name {
                text(name, 96, false)?;
            }
            if let Some(description) = &input.description {
                text(description, 4096, true)?;
            }
            if input
                .category_id
                .as_ref()
                .is_some_and(|category| !categories.contains(category))
            {
                return Err(ErrorCode::InvalidMessage);
            }
            let visibility = match input.visibility.unwrap_or(item.policy.visibility) {
                Visibility::Public if item.policy.moderation == Moderation::Blocked => {
                    return Err(ErrorCode::Forbidden);
                }
                Visibility::Public => "PUBLIC",
                Visibility::Private => "PRIVATE",
            };
            if let Some(tags) = &input.tags {
                if tags.len() > 8 {
                    return Err(ErrorCode::InvalidMessage);
                }
                transaction
                    .execute("DELETE FROM library_tags WHERE model_id = ?1", [id])
                    .map_err(sql_error)?;
                let mut unique = std::collections::HashSet::new();
                for tag in tags {
                    text(tag, 32, false)?;
                    if tag.trim() != tag || !unique.insert(tag.to_lowercase()) {
                        return Err(ErrorCode::InvalidMessage);
                    }
                    transaction
                        .execute(
                            "INSERT INTO library_tags VALUES (?1, ?2, ?3)",
                            params![id, tag.to_lowercase(), tag],
                        )
                        .map_err(sql_error)?;
                }
            }
            transaction.execute("UPDATE library_models SET display_name = ?2, description = ?3, category_id = ?4,
                visibility = ?5, requires_authorization = ?6 WHERE model_id = ?1",
                params![id, input.display_name.as_ref().unwrap_or(&item.summary.display_name),
                    input.description.as_ref().unwrap_or(&item.description), input.category_id.as_ref().unwrap_or(&item.category_id),
                    visibility, input.requires_authorization.unwrap_or(item.policy.requires_authorization)],
            ).map_err(sql_error)?;
            ensure_uploader_grant(transaction, id, actor.uuid)?;
            advance(transaction, id)?;
            Ok(json!({"model_id": id, "policy_revision": item.summary.policy_revision + 1}))
        })
    }

    pub fn library_delete(&self, actor: Actor, id: &str, input: &Mutation) -> Result<Value> {
        let operation = super::resource::operation_id(&input.operation_id)?;
        super::resource::operation_id(id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!([id, "delete", input])).map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let ownership: Option<(String, String)> = transaction.query_row(
                "SELECT uploader_uuid, moderation_status FROM library_models WHERE model_id = ?1", [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(sql_error)?;
            let (owner, status) = ownership.ok_or(ErrorCode::NotFound)?;
            let tombstone_replay = status == "DELETED" && (actor.admin_context || owner == actor.uuid.to_string());
            if !tombstone_replay {
                require_action(&model(transaction, id, actor, true)?, "delete")?;
            }
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                // Only the original operation can succeed after the model becomes invisible.
                if tombstone_replay {
                    return Err(ErrorCode::NotFound);
                }
                let item = model(transaction, id, actor, true)?;
                if item.summary.policy_revision != library::sql_integer(input.expected_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                transaction.execute("UPDATE library_models SET moderation_status = 'DELETED' WHERE model_id = ?1", [id]).map_err(sql_error)?;
                advance(transaction, id)?;
                // Bans, immutable revisions and bindings survive logical deletion.
                serde_json::to_vec(&json!({"model_id": id, "policy_revision": item.summary.policy_revision + 1})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_block(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
        blocked: bool,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        mutate(
            self,
            actor,
            id,
            input,
            if blocked { "block" } else { "unblock" },
            |transaction, item| {
                library::set_blocked(
                    transaction,
                    id,
                    actor.uuid,
                    input.expected_revision,
                    blocked,
                    input.reason.as_deref().unwrap_or(""),
                    now_seconds(),
                )?;
                Ok(json!({"model_id": id, "policy_revision": item.summary.policy_revision + 1}))
            },
        )
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationInput {
    pub operation_id: String,
}

impl Storage {
    pub fn library_favorite(
        &self,
        actor: Actor,
        id: &str,
        input: &OperationInput,
        favorite: bool,
    ) -> Result<Value> {
        let operation = super::resource::operation_id(&input.operation_id)?;
        super::resource::operation_id(id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["favorite", id, favorite]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            if favorite {
                require_action(&model(transaction, id, actor, false)?, "favorite")?;
            }
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                if favorite {
                    transaction.execute("INSERT OR IGNORE INTO library_favorites VALUES (?1, ?2, ?3)",
                        params![actor.uuid.to_string(), id, library::sql_integer(now_seconds())?]).map_err(sql_error)?;
                } else {
                    transaction.execute("DELETE FROM library_favorites WHERE player_uuid = ?1 AND model_id = ?2",
                        params![actor.uuid.to_string(), id]).map_err(sql_error)?;
                }
                // Never expose a private model's name/cover when removing a hidden reference.
                serde_json::to_vec(&json!({"favorited": favorite})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_grants(&self, actor: Actor, id: &str, page: u32) -> Result<Value> {
        if page == 0 || page > 1_000_000 {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_read(|database| {
            require_action(&model(database, id, actor, false)?, "grants")?;
            let total: i64 = database.query_row("SELECT count(*) FROM library_grants WHERE model_id = ?1", [id], |row| row.get(0)).map_err(sql_error)?;
            let mut statement = database.prepare("SELECT player_uuid, name_snapshot, created_at FROM library_grants WHERE model_id = ?1 ORDER BY player_uuid LIMIT 8 OFFSET ?2").map_err(sql_error)?;
            let rows = statement.query_map(params![id, i64::from(page - 1) * 8], |row| {
                Ok(json!({"uuid": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?, "created_at": row.get::<_, i64>(2)?}))
            }).map_err(sql_error)?;
            let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>().map_err(sql_error)?;
            Ok(json!({"items": items, "total": total, "page": page, "page_size": 8, "has_next": i64::from(page) * 8 < total}))
        })
    }

    pub fn library_grant(
        &self,
        actor: Actor,
        id: &str,
        player: Uuid,
        input: &Mutation,
        grant: bool,
    ) -> Result<Value> {
        mutate(
            self,
            actor,
            id,
            input,
            &format!("grant:{player}:{grant}"),
            |transaction, item| {
                require_action(item, "grants")?;
                if !grant && player == item.policy.uploader {
                    return Err(ErrorCode::Forbidden);
                }
                if grant {
                    let profile: String = transaction
                        .query_row(
                            "SELECT profile FROM users WHERE uuid = ?1",
                            [player.to_string()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sql_error)?
                        .ok_or(ErrorCode::NotFound)?;
                    let user: crate::auth::Userinfo =
                        serde_json::from_str(&profile).map_err(|_| ErrorCode::Internal)?;
                    if user.nickname.is_empty() || user.banned {
                        return Err(ErrorCode::NotFound);
                    }
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO library_grants VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                id,
                                player.to_string(),
                                user.nickname,
                                actor.uuid.to_string(),
                                library::sql_integer(now_seconds())?
                            ],
                        )
                        .map_err(sql_error)?;
                } else {
                    transaction
                        .execute(
                            "DELETE FROM library_grants WHERE model_id = ?1 AND player_uuid = ?2",
                            params![id, player.to_string()],
                        )
                        .map_err(sql_error)?;
                }
                advance(transaction, id)?;
                Ok(json!({"model_id": id, "policy_revision": item.summary.policy_revision + 1}))
            },
        )
    }

    pub fn library_resolve(&self, actor: Actor, model_id: &str, name: &str) -> Result<Value> {
        text(name, 64, false)?;
        self.library_read(|database| {
            require_action(&model(database, model_id, actor, false)?, "grants")?;
            let mut statement = database.prepare("SELECT profile FROM users WHERE lower(json_extract(profile, '$.nickname')) = lower(?1) LIMIT 2").map_err(sql_error)?;
            let rows = statement.query_map([name], |row| row.get::<_, String>(0)).map_err(sql_error)?;
            let mut items = Vec::new();
            for row in rows {
                let user: crate::auth::Userinfo = serde_json::from_str(&row.map_err(sql_error)?).map_err(|_| ErrorCode::Internal)?;
                if !user.banned {
                    items.push(json!({"uuid": user.uuid.to_string(), "name": user.nickname}));
                }
            }
            if items.len() > 1 {
                return Err(ErrorCode::Conflict);
            }
            Ok(json!({"items": items}))
        })
    }
}
