//! Bounded cover decoding; images share immutable blob storage, not model caches.
use super::*;
use image::{ImageFormat, ImageReader, Limits};
use std::{io::Cursor, path::PathBuf};

pub struct ImageFile {
    pub path: PathBuf,
    pub media_type: String,
    pub length: u64,
}

pub fn inspect(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return Err(ErrorCode::LimitExceeded);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ErrorCode::InvalidMessage)?;
    let media_type = match reader.format() {
        Some(ImageFormat::Png) => "image/png",
        Some(ImageFormat::Jpeg) => "image/jpeg",
        _ => return Err(ErrorCode::UnsupportedCapability),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| ErrorCode::InvalidMessage)?;
    if decoded.width() == 0 || decoded.height() == 0 {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok((media_type, decoded.width(), decoded.height()))
}

impl Storage {
    pub fn library_image_put(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
        bytes: &[u8],
    ) -> Result<Value> {
        let (media_type, width, height) = inspect(bytes)?;
        let hash = crate::storage::sha256(bytes);
        let hash_text = faster_hex::hex_string(&hash);
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["image", id, input, hash_text]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        // Reject unauthorized writes before file publication. The transaction checks again.
        self.library_read(|database| require_action(&model(database, id, actor, true)?, "image"))?;
        let temporary = self.upload_path(
            actor.uuid,
            Uuid::parse_str(&random_id()).map_err(|_| ErrorCode::Internal)?,
        );
        let written = (|| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| ErrorCode::Internal)?;
            file.write_all(bytes).map_err(|_| ErrorCode::Internal)?;
            file.sync_all().map_err(|_| ErrorCode::Internal)?;
            drop(file);
            self.publish_staged(&temporary, &hash)
        })();
        let _ = std::fs::remove_file(&temporary);
        written?;
        self.library_transaction(|transaction| {
            let item = model(transaction, id, actor, true)?;
            require_action(&item, "image")?;
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                if item.summary.policy_revision != library::sql_integer(input.expected_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                let image_revision = item.summary.policy_revision + 1;
                transaction.execute("INSERT INTO library_images VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ON CONFLICT(model_id) DO UPDATE SET image_revision = excluded.image_revision, hash = excluded.hash,
                        media_type = excluded.media_type, width = excluded.width, height = excluded.height, byte_length = excluded.byte_length",
                    params![id, image_revision, hash_text, media_type, width, height, bytes.len() as i64]).map_err(sql_error)?;
                advance(transaction, id)?;
                serde_json::to_vec(&json!({"model_id": id, "image_revision": image_revision, "policy_revision": image_revision})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_image_delete(&self, actor: Actor, id: &str, input: &Mutation) -> Result<Value> {
        mutate(
            self,
            actor,
            id,
            input,
            "clear-image",
            |transaction, item| {
                require_action(item, "image")?;
                transaction
                    .execute("DELETE FROM library_images WHERE model_id = ?1", [id])
                    .map_err(sql_error)?;
                advance(transaction, id)?;
                Ok(
                    json!({"model_id": id, "image_available": false, "policy_revision": item.summary.policy_revision + 1}),
                )
            },
        )
    }

    pub fn library_image(&self, actor: Actor, id: &str) -> Result<ImageFile> {
        self.library_read(|database| {
            model(database, id, actor, true)?;
            let (hash, media_type, length): (String, String, i64) = database
                .query_row(
                    "SELECT hash, media_type, byte_length FROM library_images WHERE model_id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(sql_error)?
                .ok_or(ErrorCode::NotFound)?;
            let mut digest = [0; 32];
            faster_hex::hex_decode(hash.as_bytes(), &mut digest)
                .map_err(|_| ErrorCode::Internal)?;
            Ok(ImageFile {
                path: self.blob_path(&digest)?,
                media_type,
                length: u64::try_from(length).map_err(|_| ErrorCode::Internal)?,
            })
        })
    }
}
