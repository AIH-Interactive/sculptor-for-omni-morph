use crate::protocol::v1::{
    AvatarDescriptor, CommitUpload, DistributionFormat, ErrorCode, ModelType, TransferGrant,
    UploadRequest,
};
use crate::{api::errors::internal_and_log, ApiError, ApiResult, AppState, AVATARS_VAR};
use prost::Message;
use std::path::PathBuf;
use std::{collections::HashMap, sync::Mutex};
use tokio::fs;
use uuid::Uuid;

struct PendingUpload {
    request: UploadRequest,
    fingerprint: Vec<u8>,
    path: PathBuf,
    expires: u64,
    busy: bool,
    uploaded: bool,
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub struct Transfers {
    pending: Mutex<HashMap<(Uuid, Uuid), PendingUpload>>,
}

impl std::fmt::Debug for Transfers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Transfers").finish_non_exhaustive()
    }
}

pub fn operation_id(text: &str) -> Result<Uuid, ErrorCode> {
    let id = Uuid::parse_str(text).map_err(|_| ErrorCode::InvalidMessage)?;
    if id.to_string() != text {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok(id)
}

pub async fn grant_upload(
    state: &AppState,
    owner: Uuid,
    request: &UploadRequest,
) -> Result<TransferGrant, ErrorCode> {
    let operation = operation_id(&request.operation_id)?;
    let mut request = request.clone();
    let descriptor = request.resource.as_mut().ok_or(ErrorCode::InvalidMessage)?;
    if !descriptor.owner.is_empty() && descriptor.owner != owner.to_string() {
        return Err(ErrorCode::Forbidden);
    }
    descriptor.owner = owner.to_string();
    if descriptor.model_type != ModelType::ModelFigura as i32
        || descriptor.distribution_format != DistributionFormat::FiguraCompressedNbt as i32
    {
        return Err(ErrorCode::UnsupportedCapability);
    }
    if descriptor.asset_id.is_empty()
        || descriptor.asset_id.len() > 64
        || !descriptor
            .asset_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-'))
        || descriptor.raw_sha256.len() != 32
        || descriptor.raw_length == 0
        || descriptor.revision != 0
        || (!descriptor.content_sha256.is_empty() && descriptor.content_sha256.len() != 32)
        || (!descriptor.schema_sha256.is_empty() && descriptor.schema_sha256.len() != 32)
        || descriptor
            .required_capabilities
            .iter()
            .any(|capability| *capability != 1)
    {
        return Err(ErrorCode::InvalidMessage);
    }
    let config = state.config.read().await;
    if descriptor.raw_length > config.limitations.max_avatar_size.saturating_mul(1000) {
        return Err(ErrorCode::LimitExceeded);
    }
    if !state
        .user_manager
        .upload_state(owner, config.limitations.can_upload)
    {
        return Err(ErrorCode::Forbidden);
    }
    let raw_length = descriptor.raw_length;
    drop(config);
    let fingerprint =
        crate::storage::sha256(&[b"upload".as_slice(), &request.encode_to_vec()].concat());
    state
        .storage
        .replay_avatar(owner, &request.operation_id, &fingerprint)?;
    let mut pending = state
        .transfers
        .pending
        .lock()
        .map_err(|_| ErrorCode::Internal)?;
    pending.retain(|_, upload| upload.expires > crate::storage::now_seconds() || upload.busy);
    let key = (owner, operation);
    if let Some(existing) = pending.get(&key) {
        if existing.fingerprint != fingerprint {
            return Err(ErrorCode::Conflict);
        }
    } else {
        if pending
            .keys()
            .filter(|(account, _)| *account == owner)
            .count()
            >= 2
        {
            return Err(ErrorCode::LimitExceeded);
        }
        pending.insert(
            key,
            PendingUpload {
                request: request.clone(),
                fingerprint,
                path: state.storage.upload_path(owner, operation),
                expires: crate::storage::now_seconds() + 120,
                busy: false,
                uploaded: false,
            },
        );
    }
    let expires = pending
        .get(&key)
        .map(|upload| upload.expires)
        .unwrap_or(crate::storage::now_seconds() + 120);
    Ok(TransferGrant {
        operation_id: request.operation_id,
        relative_path: format!("/api/protobuf/v1/resources/uploads/{operation}"),
        expires_unix_seconds: expires,
        max_bytes: raw_length,
    })
}

pub struct UploadWriter {
    transfers: std::sync::Arc<Transfers>,
    key: (Uuid, Uuid),
    pub path: PathBuf,
    pub descriptor: AvatarDescriptor,
    sealed: bool,
}

impl Drop for UploadWriter {
    fn drop(&mut self) {
        if !self.sealed {
            let _ = std::fs::remove_file(&self.path);
        }
        if let Ok(mut pending) = self.transfers.pending.lock() {
            if let Some(upload) = pending.get_mut(&self.key) {
                upload.busy = false;
                upload.uploaded = self.sealed;
            }
        }
    }
}

impl UploadWriter {
    pub fn seal(&mut self) {
        self.sealed = true;
    }
}

pub fn begin_upload(
    state: &AppState,
    owner: Uuid,
    operation: Uuid,
) -> Result<UploadWriter, ErrorCode> {
    let mut pending = state
        .transfers
        .pending
        .lock()
        .map_err(|_| ErrorCode::Internal)?;
    let upload = pending
        .get_mut(&(owner, operation))
        .ok_or(ErrorCode::NotFound)?;
    if upload.expires <= crate::storage::now_seconds() {
        return Err(ErrorCode::Unauthenticated);
    }
    if upload.busy || upload.uploaded {
        return Err(ErrorCode::Conflict);
    }
    upload.busy = true;
    Ok(UploadWriter {
        transfers: state.transfers.clone(),
        key: (owner, operation),
        path: upload.path.clone(),
        descriptor: upload.request.resource.clone().ok_or(ErrorCode::Internal)?,
        sealed: false,
    })
}

pub async fn commit_upload(
    state: &AppState,
    owner: Uuid,
    request: &CommitUpload,
) -> Result<AvatarDescriptor, ErrorCode> {
    let operation = operation_id(&request.operation_id)?;
    if let Some(completed) = state.storage.completed_upload(owner, request)? {
        state
            .transfers
            .pending
            .lock()
            .map_err(|_| ErrorCode::Internal)?
            .remove(&(owner, operation));
        return Ok(completed);
    }
    let (descriptor, fingerprint, path) = {
        let pending = state
            .transfers
            .pending
            .lock()
            .map_err(|_| ErrorCode::Internal)?;
        let upload = pending
            .get(&(owner, operation))
            .ok_or(ErrorCode::NotFound)?;
        if upload.expires <= crate::storage::now_seconds() || !upload.uploaded || upload.busy {
            return Err(ErrorCode::Conflict);
        }
        let descriptor = upload.request.resource.clone().ok_or(ErrorCode::Internal)?;
        if request.raw_sha256 != descriptor.raw_sha256
            || request.raw_length != descriptor.raw_length
        {
            return Err(ErrorCode::Conflict);
        }
        (descriptor, upload.fingerprint.clone(), upload.path.clone())
    };
    let mut descriptor = descriptor;
    let source = if path.exists() {
        path.clone()
    } else {
        state.storage.blob_path(&descriptor.raw_sha256)?
    };
    descriptor.legacy_figura_hash = legacy_file_hash(&source).await?;
    if descriptor.content_sha256.is_empty() {
        descriptor.content_sha256 = descriptor.raw_sha256.clone();
    }
    state
        .storage
        .publish_staged(&path, &descriptor.raw_sha256)?;
    let max_assets = state.config.read().await.limitations.max_avatars as usize;
    let committed = state.storage.commit_avatar(
        owner,
        &request.operation_id,
        &fingerprint,
        descriptor,
        max_assets,
    )?;
    state
        .transfers
        .pending
        .lock()
        .map_err(|_| ErrorCode::Internal)?
        .remove(&(owner, operation));
    Ok(committed)
}

async fn legacy_file_hash(path: &std::path::Path) -> Result<String, ErrorCode> {
    // Legacy identity is SHA-256 over streaming base64 text, including final padding.
    use base64::Engine;
    use tokio::io::AsyncReadExt;
    let mut file = fs::File::open(path)
        .await
        .map_err(|_| ErrorCode::Internal)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 3 * 4096];
    let mut carry = Vec::new();
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .map_err(|_| ErrorCode::Internal)?;
        if count == 0 {
            break;
        }
        carry.extend_from_slice(&buffer[..count]);
        let aligned = carry.len() / 3 * 3;
        hash.update(
            base64::engine::general_purpose::STANDARD
                .encode(&carry[..aligned])
                .as_bytes(),
        );
        carry.drain(..aligned);
    }
    if !carry.is_empty() {
        hash.update(
            base64::engine::general_purpose::STANDARD
                .encode(&carry)
                .as_bytes(),
        );
    }
    Ok(faster_hex::hex_string(hash.finish().as_ref()))
}

pub async fn ensure_legacy_import(
    state: &AppState,
    owner: Uuid,
    force_equipped: bool,
) -> Result<(), ErrorCode> {
    use base64::Engine;
    let path = legacy_path(owner, false);
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::metadata(&path).await.map_err(|_| ErrorCode::Internal)?;
    let limit = crate::utils::get_limit_as_bytes(
        state.config.read().await.limitations.max_avatar_size as usize,
    );
    if metadata.len() > limit as u64 {
        return Err(ErrorCode::LimitExceeded);
    }
    let bytes = fs::read(path).await.map_err(|_| ErrorCode::Internal)?;
    let hash = crate::storage::sha256(&bytes);
    if !force_equipped && state.storage.legacy_hash_matches(owner, &hash) {
        return Ok(());
    }
    let legacy_hash = crate::storage::sha256(
        base64::engine::general_purpose::STANDARD
            .encode(&bytes)
            .as_bytes(),
    );
    state.storage.publish_blob(&bytes, &hash)?;
    state.storage.import_legacy(
        AvatarDescriptor {
            owner: owner.to_string(),
            asset_id: "avatar".into(),
            model_type: ModelType::ModelFigura as i32,
            distribution_format: DistributionFormat::FiguraCompressedNbt as i32,
            raw_sha256: hash.clone(),
            raw_length: bytes.len() as u64,
            content_sha256: hash,
            legacy_figura_hash: faster_hex::hex_string(&legacy_hash),
            required_capabilities: vec![1],
            ..Default::default()
        },
        force_equipped,
    )?;
    Ok(())
}

pub fn legacy_path(owner: Uuid, temporary: bool) -> PathBuf {
    let root = PathBuf::from(&*AVATARS_VAR);
    let root = if temporary { root.join("temp") } else { root };
    root.join(format!("{owner}.moon"))
}

pub async fn upload_legacy(state: &AppState, token: &str, bytes: &[u8]) -> ApiResult<()> {
    let user = super::session::authenticate(state, token)?;
    let allowed = state.config.read().await.limitations.can_upload;
    if !state.user_manager.upload_state(user.uuid, allowed) {
        return Err(ApiError::Forbidden);
    }
    // write() completes the full buffer before success, including small packages.
    write_legacy(user.uuid, false, bytes).await?;
    ensure_legacy_import(state, user.uuid, true)
        .await
        .map_err(legacy_error)
}

pub async fn write_legacy(owner: Uuid, temporary: bool, bytes: &[u8]) -> ApiResult<()> {
    fs::write(legacy_path(owner, temporary), bytes)
        .await
        .map_err(internal_and_log)
}

pub async fn download_legacy(
    state: &AppState,
    owner: Uuid,
    requesting_self: bool,
) -> ApiResult<Vec<u8>> {
    let temporary_path = legacy_path(owner, true);
    let temporary = requesting_self && temporary_path.exists();
    let path = if temporary {
        temporary_path
    } else {
        ensure_legacy_import(state, owner, false)
            .await
            .map_err(legacy_error)?;
        let equipped = state.storage.equipped(owner).map_err(legacy_error)?;
        let avatar = equipped.figura_compatibility.ok_or(ApiError::NotFound)?;
        state
            .storage
            .blob_path(&avatar.raw_sha256)
            .map_err(legacy_error)?
    };
    let bytes = fs::read(&path).await.map_err(|_| ApiError::NotFound)?;
    if temporary {
        fs::remove_file(path).await.map_err(internal_and_log)?;
    }
    Ok(bytes)
}

pub async fn delete_legacy(state: &AppState, token: &str) -> ApiResult<()> {
    let user = super::session::authenticate(state, token)?;
    delete_legacy_owner(state, user.uuid).await
}

/// Called only after the adapter has authorized the owner or an administrator.
pub async fn delete_legacy_owner(state: &AppState, owner: Uuid) -> ApiResult<()> {
    ensure_legacy_import(state, owner, false)
        .await
        .map_err(legacy_error)?;
    let equipped = state.storage.equipped(owner).map_err(legacy_error)?;
    let avatar = match equipped.figura_compatibility {
        Some(avatar) => avatar,
        None => state
            .storage
            .avatar(owner, "avatar", 0)
            .map_err(legacy_error)?,
    };
    state
        .storage
        .delete(
            owner,
            &crate::protocol::v1::Delete {
                operation_id: new_operation_id(),
                asset_id: avatar.asset_id.clone(),
                expected_revision: avatar.revision,
            },
        )
        .map_err(legacy_error)?;
    if avatar.asset_id == "avatar" && legacy_path(owner, false).exists() {
        fs::remove_file(legacy_path(owner, false))
            .await
            .map_err(internal_and_log)?;
    }
    super::equipment::deleted(state, owner).await;
    Ok(())
}

pub fn new_operation_id() -> String {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&crate::utils::rand()[..16]);
    Uuid::from_bytes(bytes).to_string()
}

pub fn legacy_error(code: ErrorCode) -> ApiError {
    match code {
        ErrorCode::Unauthenticated => ApiError::Unauthorized,
        ErrorCode::Forbidden => ApiError::Forbidden,
        ErrorCode::NotFound => ApiError::NotFound,
        ErrorCode::InvalidMessage => ApiError::BadRequest,
        _ => ApiError::Internal,
    }
}
