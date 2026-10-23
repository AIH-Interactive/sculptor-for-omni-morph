//! Player-bound, revision-fixed transfers and idempotent download receipts.
use super::*;
use std::path::PathBuf;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferInput {
    pub operation_id: String,
    pub purpose: String,
    pub model_revision: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteInput {
    pub operation_id: String,
    pub raw_sha256: String,
}

pub struct TransferFile {
    pub body: TransferBody,
    pub length: u64,
    pub raw_sha256: String,
    pub media_type: &'static str,
}

pub enum TransferBody {
    Model(PathBuf),
    Configuration(Vec<u8>),
}

fn receipt(
    database: &Connection,
    id: &str,
    actor: Actor,
) -> Result<(String, i64, String, i64, bool, bool)> {
    super::super::resource::operation_id(id)?;
    let (model_id, revision, purpose, policy_revision, expiry, sent, completed): (
        String,
        i64,
        String,
        i64,
        i64,
        bool,
        bool,
    ) = database
        .query_row(
            "SELECT model_id, revision, purpose, policy_revision, expires_at, bytes_sent, completed
         FROM library_download_receipts WHERE transfer_id = ?1 AND player_uuid = ?2",
            params![id, actor.uuid.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(ErrorCode::NotFound)?;
    if expiry <= library::sql_integer(now_seconds())? {
        return Err(ErrorCode::Unauthenticated);
    }
    let item = model(database, &model_id, actor, false)?;
    require_action(
        &item,
        if purpose == "preview" {
            "preview"
        } else {
            "download"
        },
    )?;
    if item.summary.policy_revision != policy_revision || item.summary.revision != revision {
        return Err(ErrorCode::Conflict);
    }
    Ok((model_id, revision, purpose, expiry, sent, completed))
}

impl Storage {
    pub fn library_transfer(&self, actor: Actor, id: &str, input: &TransferInput) -> Result<Value> {
        if !matches!(input.purpose.as_str(), "preview" | "download") {
            return Err(ErrorCode::InvalidMessage);
        }
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["transfer", id, input]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let item = model(transaction, id, actor, false)?;
            require_action(&item, &input.purpose)?;
            if item.summary.revision != library::sql_integer(input.model_revision)? {
                return Err(ErrorCode::Conflict);
            }
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let active: i64 = transaction.query_row(
                    "SELECT count(*) FROM library_download_receipts WHERE player_uuid = ?1 AND expires_at > ?2 AND bytes_sent = 0",
                    params![actor.uuid.to_string(), library::sql_integer(now_seconds())?], |row| row.get(0),
                ).map_err(sql_error)?;
                if active >= 2 {
                    return Err(ErrorCode::LimitExceeded);
                }
                let transfer = random_id();
                let expiry = library::sql_integer(now_seconds() + 120)?;
                transaction.execute("INSERT INTO library_download_receipts
                    (transfer_id, player_uuid, model_id, revision, purpose, policy_revision, expires_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![transfer, actor.uuid.to_string(), id,
                        item.summary.revision, input.purpose, item.summary.policy_revision, expiry]).map_err(sql_error)?;
                serde_json::to_vec(&json!({"transfer_id": transfer, "purpose": input.purpose,
                    "model_revision": item.summary.revision, "policy_revision": item.summary.policy_revision,
                    "relative_path": format!("/api/library/v1/transfers/{transfer}"), "expires_at": expiry})).map_err(|_| ErrorCode::Internal)
            })?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)?;
            receipt(transaction, value["transfer_id"].as_str().ok_or(ErrorCode::Internal)?, actor)?;
            Ok(value)
        })
    }

    pub fn library_transfer_file(&self, actor: Actor, transfer: &str) -> Result<TransferFile> {
        self.library_read(|database| {
            let (model, revision, _, _, _, _) = receipt(database, transfer, actor)?;
            self.library_transfer_payload(database, &model, revision)
        })
    }

    fn library_transfer_payload(
        &self,
        database: &Connection,
        model: &str,
        revision: i64,
    ) -> Result<TransferFile> {
        let configuration: Option<String> = database.query_row(
                "SELECT configuration_json FROM library_outfit_contents WHERE model_id = ?1 AND revision = ?2",
                params![model, revision], |row| row.get(0),
            ).optional().map_err(sql_error)?;
        if let Some(configuration) = configuration {
            let bytes = configuration.into_bytes();
            return Ok(TransferFile {
                length: bytes.len() as u64,
                raw_sha256: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
                body: TransferBody::Configuration(bytes),
                media_type: "application/json",
            });
        }
        let (hash, length): (String, i64) = database.query_row(
                "SELECT raw_sha256, byte_length FROM library_revisions WHERE model_id = ?1 AND revision = ?2",
                params![model, revision], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(sql_error)?;
        library::validate_hash("RAW_SHA256", &hash)?;
        let mut digest = [0; 32];
        faster_hex::hex_decode(hash.as_bytes(), &mut digest).map_err(|_| ErrorCode::Internal)?;
        Ok(TransferFile {
            body: TransferBody::Model(self.library_blob_path(&digest)?),
            raw_sha256: hash,
            media_type: "application/octet-stream",
            length: u64::try_from(length).map_err(|_| ErrorCode::Internal)?,
        })
    }

    pub fn library_transfer_sent(&self, actor: Actor, transfer: &str) -> Result<()> {
        self.library_transaction(|transaction| {
            receipt(transaction, transfer, actor)?;
            transaction
                .execute(
                    "UPDATE library_download_receipts SET bytes_sent = 1 WHERE transfer_id = ?1",
                    [transfer],
                )
                .map_err(sql_error)?;
            Ok(())
        })
    }

    pub fn library_transfer_complete(
        &self,
        actor: Actor,
        transfer: &str,
        input: &CompleteInput,
    ) -> Result<Value> {
        library::validate_hash("RAW_SHA256", &input.raw_sha256)?;
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["complete", transfer, input]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let (model, revision, purpose, _, sent, completed) = receipt(transaction, transfer, actor)?;
            let hash = self.library_transfer_payload(transaction, &model, revision)?.raw_sha256;
            if !sent || purpose != "download" || hash != input.raw_sha256 {
                return Err(ErrorCode::Conflict);
            }
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let day = library::sql_integer(now_seconds() / 86400)?;
                let already_counted: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_download_receipts WHERE player_uuid = ?1 AND model_id = ?2 AND revision = ?3 AND counted_day = ?4)",
                    params![actor.uuid.to_string(), model, revision, day], |row| row.get(0),
                ).map_err(sql_error)?;
                if !already_counted && !completed {
                    transaction.execute("UPDATE library_download_receipts SET completed = 1, counted_day = ?2 WHERE transfer_id = ?1", params![transfer, day]).map_err(sql_error)?;
                    transaction.execute("INSERT INTO library_stats VALUES (?1, 1) ON CONFLICT(model_id) DO UPDATE SET downloads = downloads + 1", [&model]).map_err(sql_error)?;
                } else {
                    transaction.execute("UPDATE library_download_receipts SET completed = 1 WHERE transfer_id = ?1", [transfer]).map_err(sql_error)?;
                }
                serde_json::to_vec(&json!({"completed": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        })
    }
}
