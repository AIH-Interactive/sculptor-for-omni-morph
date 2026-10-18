//! Administrator-only recovery and irreversible deletion of archived library payloads.
use super::*;
use crate::storage::recycle::copy_durable;

impl Storage {
    pub fn library_recover(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
        purge: bool,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        super::super::resource::operation_id(id)?;
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!([id, if purge { "purge" } else { "restore" }, input]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        let folder = self.library_recycle_path(id)?;
        let _publication = self.lock_blob_files()?;
        let result = self.library_transaction(|transaction| {
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let (status, revision, original, purged): (String, i64, String, bool) = transaction.query_row(
                    "SELECT m.moderation_status, m.policy_revision, r.original_status, r.purged
                     FROM library_models m JOIN library_recycle r USING(model_id) WHERE model_id = ?1",
                    [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                ).optional().map_err(sql_error)?.ok_or(ErrorCode::NotFound)?;
                if purged || status != "DELETED" {
                    return Err(ErrorCode::NotFound);
                }
                if revision != library::sql_integer(input.expected_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                if purge {
                    transaction.execute("UPDATE library_recycle SET purged = 1 WHERE model_id = ?1", [id]).map_err(sql_error)?;
                } else {
                    for hash in Self::library_files(transaction, id)? {
                        let mut digest = [0; 32];
                        faster_hex::hex_decode(hash.as_bytes(), &mut digest).map_err(|_| ErrorCode::Internal)?;
                        copy_durable(&folder.join(&hash), &self.library_blob_path(&digest)?)?;
                    }
                    transaction.execute("UPDATE library_models SET moderation_status = ?2 WHERE model_id = ?1",
                        params![id, original]).map_err(sql_error)?;
                    transaction.execute("DELETE FROM library_recycle WHERE model_id = ?1", [id]).map_err(sql_error)?;
                }
                advance(transaction, id)?;
                serde_json::to_vec(&json!({"model_id": id, "policy_revision": revision + 1,
                    "restored": !purge, "purged": purge})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice::<Value>(&bytes).map_err(|_| ErrorCode::Internal)
        })?;
        // The durable operation result survives a lost response or interrupted cleanup.
        if purge {
            self.library_read(|database| self.clean_library_files(database, id))?;
        }
        let cleanup = self.library_read(|database| {
            database.query_row(
                "SELECT CASE WHEN ?2 THEN EXISTS(SELECT 1 FROM library_recycle r
                 WHERE r.model_id = m.model_id AND r.purged = 1)
                 ELSE m.moderation_status != 'DELETED' END FROM library_models m WHERE model_id = ?1",
                params![id, purge], |row| row.get::<_, bool>(0),
            ).map_err(sql_error)
        })?;
        if cleanup && folder.is_dir() {
            std::fs::remove_dir_all(folder).map_err(|_| ErrorCode::Internal)?;
        }
        Ok(result)
    }
}
