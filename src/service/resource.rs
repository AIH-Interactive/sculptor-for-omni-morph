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

struct CommitPermit {
    transfers: std::sync::Arc<Transfers>,
    key: (Uuid, Uuid),
}

impl Drop for CommitPermit {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.transfers.pending.lock() {
            if let Some(upload) = pending.get_mut(&self.key) {
                upload.busy = false;
            }
        }
    }
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub struct Transfers {
    pending: Mutex<HashMap<(Uuid, Uuid), PendingUpload>>,
    legacy_active: Mutex<HashMap<Uuid, usize>>,
}

struct LegacyUploadPermit {
    transfers: std::sync::Arc<Transfers>,
    owner: Uuid,
}

impl LegacyUploadPermit {
    fn acquire(transfers: &std::sync::Arc<Transfers>, owner: Uuid) -> Result<Self, ErrorCode> {
        // Keep admission in the same lock order as protobuf grant creation.
        let mut pending = transfers.pending.lock().map_err(|_| ErrorCode::Internal)?;
        pending.retain(|_, upload| upload.expires > crate::storage::now_seconds() || upload.busy);
        let mut legacy = transfers
            .legacy_active
            .lock()
            .map_err(|_| ErrorCode::Internal)?;
        let active = legacy.entry(owner).or_default();
        let grants = pending
            .keys()
            .filter(|(account, _)| *account == owner)
            .count();
        if grants + *active >= 2 {
            return Err(ErrorCode::LimitExceeded);
        }
        *active += 1;
        Ok(Self {
            transfers: transfers.clone(),
            owner,
        })
    }
}

impl Drop for LegacyUploadPermit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.transfers.legacy_active.lock() {
            if let Some(count) = active.get_mut(&self.owner) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    active.remove(&self.owner);
                }
            }
        }
    }
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
    let storage = state.storage.clone();
    let asset = request
        .resource
        .as_ref()
        .ok_or(ErrorCode::InvalidMessage)?
        .asset_id
        .clone();
    if super::library::work(move || storage.library_asset_bound(owner, &asset)).await? {
        return Err(ErrorCode::Forbidden);
    }
    grant_upload_inner(state, owner, request, false).await
}

pub(crate) async fn grant_library_upload(
    state: &AppState,
    owner: Uuid,
    request: &UploadRequest,
) -> Result<TransferGrant, ErrorCode> {
    grant_upload_inner(state, owner, request, true).await
}

async fn grant_upload_inner(
    state: &AppState,
    owner: Uuid,
    request: &UploadRequest,
    library: bool,
) -> Result<TransferGrant, ErrorCode> {
    let operation = operation_id(&request.operation_id)?;
    let mut request = request.clone();
    let descriptor = request.resource.as_mut().ok_or(ErrorCode::InvalidMessage)?;
    if !descriptor.owner.is_empty() && descriptor.owner != owner.to_string() {
        return Err(ErrorCode::Forbidden);
    }
    descriptor.owner = owner.to_string();
    let expected_format = match ModelType::try_from(descriptor.model_type) {
        Ok(ModelType::ModelFigura) => DistributionFormat::FiguraCompressedNbt,
        Ok(ModelType::ModelYsmPlain | ModelType::ModelYsmEncrypted) => {
            DistributionFormat::YsmDistributionPackage
        }
        _ => return Err(ErrorCode::UnsupportedCapability),
    };
    if descriptor.distribution_format != expected_format as i32 {
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
            .any(|capability| *capability != descriptor.model_type)
        || (descriptor.model_type != 1
            && (descriptor.content_sha256.len() != 32 || descriptor.schema_sha256.len() != 32))
    {
        return Err(ErrorCode::InvalidMessage);
    }
    let config = state.config.read().await;
    let limit = if library {
        config.model_library.max_upload_bytes
    } else {
        config.limitations.max_avatar_size.saturating_mul(1000)
    };
    if descriptor.raw_length > limit {
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
        let legacy = state
            .transfers
            .legacy_active
            .lock()
            .map_err(|_| ErrorCode::Internal)?;
        if pending
            .keys()
            .filter(|(account, _)| *account == owner)
            .count()
            + legacy.get(&owner).copied().unwrap_or_default()
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
        let mut retained = false;
        if let Ok(mut pending) = self.transfers.pending.lock() {
            if let Some(upload) = pending.get_mut(&self.key) {
                upload.busy = false;
                upload.uploaded = self.sealed;
                retained = true;
            }
        }
        if !self.sealed || !retained {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl UploadWriter {
    pub fn seal(&mut self) {
        self.sealed = true;
    }
}

/// Shared bounded stream writer for both authenticated library adapters and v1.
pub(crate) async fn receive_upload(
    upload: &mut UploadWriter,
    body: axum::body::Body,
    authorize: impl Fn() -> Result<(), ErrorCode>,
) -> Result<(), ErrorCode> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&upload.path)
            .await
            .map_err(|_| ErrorCode::Internal)?;
        let mut stream = body.into_data_stream();
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut count = 0u64;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|_| ErrorCode::InvalidMessage)?;
            count = count
                .checked_add(bytes.len() as u64)
                .ok_or(ErrorCode::LimitExceeded)?;
            if count > upload.descriptor.raw_length {
                return Err(ErrorCode::LimitExceeded);
            }
            authorize()?;
            if !upload
                .transfers
                .pending
                .lock()
                .map_err(|_| ErrorCode::Internal)?
                .contains_key(&upload.key)
            {
                return Err(ErrorCode::Conflict);
            }
            hash.update(&bytes);
            file.write_all(&bytes)
                .await
                .map_err(|_| ErrorCode::Internal)?;
        }
        authorize()?;
        if count != upload.descriptor.raw_length
            || hash.finish().as_ref() != upload.descriptor.raw_sha256
        {
            return Err(ErrorCode::InvalidMessage);
        }
        file.sync_all().await.map_err(|_| ErrorCode::Internal)?;
        Ok(())
    })
    .await
    .map_err(|_| ErrorCode::Offline)?;
    result?;
    upload.seal();
    Ok(())
}

pub(crate) fn cancel_upload(
    state: &AppState,
    owner: Uuid,
    operation: Uuid,
) -> Result<(), ErrorCode> {
    state
        .transfers
        .pending
        .lock()
        .map_err(|_| ErrorCode::Internal)?
        .remove(&(owner, operation));
    Ok(())
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
    let storage = state.storage.clone();
    let operation = request.operation_id.clone();
    match super::library::work(move || storage.library_reservation(owner, &operation)).await {
        Ok(_) => return Err(ErrorCode::Forbidden),
        Err(ErrorCode::NotFound) => {}
        Err(error) => return Err(error),
    }
    commit_upload_inner(state, owner, request, None).await
}

pub(crate) async fn commit_library_upload(
    state: &AppState,
    owner: Uuid,
    request: &CommitUpload,
    context: super::library::uploads::CommitContext,
) -> Result<AvatarDescriptor, ErrorCode> {
    commit_upload_inner(state, owner, request, Some(context)).await
}

async fn commit_upload_inner(
    state: &AppState,
    owner: Uuid,
    request: &CommitUpload,
    context: Option<super::library::uploads::CommitContext>,
) -> Result<AvatarDescriptor, ErrorCode> {
    let operation = operation_id(&request.operation_id)?;
    if let Some(completed) = state.storage.completed_upload(owner, request)? {
        if let Some(context) = context {
            let storage = state.storage.clone();
            super::library::work(move || context.check_replay(&storage)).await?;
        }
        state
            .transfers
            .pending
            .lock()
            .map_err(|_| ErrorCode::Internal)?
            .remove(&(owner, operation));
        return Ok(completed);
    }
    let (descriptor, fingerprint, path) = {
        let mut pending = state
            .transfers
            .pending
            .lock()
            .map_err(|_| ErrorCode::Internal)?;
        let upload = pending
            .get_mut(&(owner, operation))
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
        upload.busy = true;
        (descriptor, upload.fingerprint.clone(), upload.path.clone())
    };
    let _commit_permit = CommitPermit {
        transfers: state.transfers.clone(),
        key: (owner, operation),
    };
    let mut descriptor = descriptor;
    let source = if path.exists() {
        path.clone()
    } else {
        state.storage.blob_path(&descriptor.raw_sha256)?
    };
    let inspection_path = source.clone();
    let inspected = tokio::task::spawn_blocking(move || super::package::inspect(&inspection_path))
        .await
        .map_err(|_| ErrorCode::Internal)?
        .and_then(|identity| {
            if identity.model_type as i32 != descriptor.model_type
                || identity
                    .content_hash
                    .as_ref()
                    .is_some_and(|hash| *hash != descriptor.content_sha256)
            {
                Err(ErrorCode::InvalidMessage)
            } else {
                Ok(identity)
            }
        });
    let inspected = match inspected {
        Ok(inspected) => inspected,
        Err(code) => {
            state
                .transfers
                .pending
                .lock()
                .map_err(|_| ErrorCode::Internal)?
                .remove(&(owner, operation));
            return Err(code);
        }
    };
    descriptor.legacy_figura_hash = legacy_file_hash(&source).await?;
    if descriptor.content_sha256.is_empty() {
        descriptor.content_sha256 = descriptor.raw_sha256.clone();
    }
    state
        .storage
        .publish_staged(&path, &descriptor.raw_sha256)?;
    let max_assets = if context.is_some() {
        1000
    } else {
        state.config.read().await.limitations.max_avatars as usize
    };
    let storage = state.storage.clone();
    let upload_operation = request.operation_id.clone();
    let committed = super::library::work(move || {
        storage.commit_avatar_with_library(
            owner,
            &upload_operation,
            &fingerprint,
            descriptor,
            max_assets,
            |transaction, descriptor| {
                if let Some(context) = context {
                    context.publish(transaction, descriptor, &inspected)?;
                }
                Ok(())
            },
        )
    })
    .await?;
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
    let path = legacy_read_path(owner, false);
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
    // Work from a bounded snapshot so an atomic legacy replacement cannot change
    // the bytes between classification, hashing and immutable publication.
    let operation = operation_id(&new_operation_id())?;
    let snapshot = StagedFile(state.storage.upload_path(owner, operation));
    let raw_length = copy_bounded(&path, &snapshot.0, limit as u64).await?;
    let hash = raw_file_hash(&snapshot.0).await?;
    if !force_equipped && state.storage.legacy_hash_matches(owner, &hash) {
        return Ok(());
    }
    let legacy_hash = legacy_file_hash(&snapshot.0).await?;
    let inspection_path = snapshot.0.clone();
    let identity = tokio::task::spawn_blocking(move || super::package::inspect(&inspection_path))
        .await
        .map_err(|_| ErrorCode::Internal)??;
    let model_type = identity.model_type as i32;
    state.storage.publish_staged(&snapshot.0, &hash)?;
    state.storage.import_legacy(
        AvatarDescriptor {
            owner: owner.to_string(),
            asset_id: "avatar".into(),
            model_type,
            distribution_format: if model_type == 1 {
                DistributionFormat::FiguraCompressedNbt
            } else {
                DistributionFormat::YsmDistributionPackage
            } as i32,
            raw_sha256: hash.clone(),
            raw_length,
            content_sha256: identity.content_hash.unwrap_or(hash),
            legacy_figura_hash: legacy_hash,
            required_capabilities: vec![model_type],
            ..Default::default()
        },
        force_equipped,
    )?;
    Ok(())
}

pub fn legacy_path(owner: Uuid, temporary: bool) -> PathBuf {
    let root = PathBuf::from(&*AVATARS_VAR);
    let root = root.join(if temporary { "upload" } else { "models" });
    root.join(format!("{owner}.moon"))
}

pub fn legacy_read_path(owner: Uuid, temporary: bool) -> PathBuf {
    let canonical = legacy_path(owner, temporary);
    // After layout migration, retained backups must not resurrect deleted models.
    if canonical.exists() || PathBuf::from(&*AVATARS_VAR).join("models").is_dir() {
        return canonical;
    }
    let root = PathBuf::from(&*AVATARS_VAR);
    let root = if temporary { root.join("temp") } else { root };
    root.join(format!("{owner}.moon"))
}

/// Cleans an unpublished staging file on failure, timeout or request cancellation.
struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn upload_legacy(state: &AppState, token: &str, body: axum::body::Body) -> ApiResult<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let user = super::session::authenticate(state, token)?;
    let config = state.config.read().await;
    let allowed = config.limitations.can_upload;
    let limit = config.limitations.max_avatar_size.saturating_mul(1000);
    drop(config);
    if !state.user_manager.upload_state(user.uuid, allowed) {
        return Err(ApiError::Forbidden);
    }
    let _permit = LegacyUploadPermit::acquire(&state.transfers, user.uuid).map_err(|code| {
        if code == ErrorCode::LimitExceeded {
            ApiError::TooManyRequests
        } else {
            legacy_error(code)
        }
    })?;
    let operation = Uuid::parse_str(&new_operation_id()).map_err(internal_and_log)?;
    let staging = StagedFile(state.storage.upload_path(user.uuid, operation));
    let received = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging.0)
            .await
            .map_err(internal_and_log)?;
        let mut stream = body.into_data_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            // The adapter's existing DefaultBodyLimit is still authoritative.
            let bytes = chunk.map_err(|_| ApiError::PayloadTooLarge)?;
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or(ApiError::PayloadTooLarge)?;
            if total > limit {
                return Err(ApiError::PayloadTooLarge);
            }
            super::session::authenticate(state, token)?;
            file.write_all(&bytes).await.map_err(internal_and_log)?;
        }
        file.sync_all().await.map_err(internal_and_log)?;
        drop(file);
        let path = staging.0.clone();
        tokio::task::spawn_blocking(move || super::package::inspect(&path))
            .await
            .map_err(internal_and_log)?
            .map_err(legacy_error)?;
        let target = legacy_path(user.uuid, false);
        fs::create_dir_all(target.parent().ok_or(ApiError::Internal)?)
            .await
            .map_err(internal_and_log)?;
        fs::rename(&staging.0, target)
            .await
            .map_err(internal_and_log)
    })
    .await
    .map_err(|_| ApiError::BadRequest)?;
    received?;
    ensure_legacy_import(state, user.uuid, true)
        .await
        .map_err(legacy_error)
}

pub async fn write_legacy(
    state: &AppState,
    owner: Uuid,
    temporary: bool,
    bytes: &[u8],
) -> ApiResult<()> {
    let target = legacy_path(owner, temporary);
    fs::create_dir_all(target.parent().ok_or(ApiError::Internal)?)
        .await
        .map_err(internal_and_log)?;
    let operation = operation_id(&new_operation_id()).map_err(legacy_error)?;
    let staging = StagedFile(state.storage.upload_path(owner, operation));
    fs::write(&staging.0, bytes)
        .await
        .map_err(internal_and_log)?;
    let inspection = staging.0.clone();
    tokio::task::spawn_blocking(move || super::package::inspect(&inspection))
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(legacy_error)?;
    fs::File::open(&staging.0)
        .await
        .map_err(internal_and_log)?
        .sync_all()
        .await
        .map_err(internal_and_log)?;
    fs::rename(&staging.0, target)
        .await
        .map_err(internal_and_log)
}

async fn copy_bounded(
    source: &std::path::Path,
    target: &std::path::Path,
    limit: u64,
) -> Result<u64, ErrorCode> {
    use tokio::io::AsyncReadExt;
    let source = fs::File::open(source)
        .await
        .map_err(|_| ErrorCode::Internal)?;
    let mut source = source.take(limit.saturating_add(1));
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .await
        .map_err(|_| ErrorCode::Internal)?;
    let count = tokio::io::copy(&mut source, &mut target)
        .await
        .map_err(|_| ErrorCode::Internal)?;
    if count > limit {
        return Err(ErrorCode::LimitExceeded);
    }
    target.sync_all().await.map_err(|_| ErrorCode::Internal)?;
    Ok(count)
}

async fn raw_file_hash(path: &std::path::Path) -> Result<Vec<u8>, ErrorCode> {
    use tokio::io::AsyncReadExt;
    let mut file = fs::File::open(path)
        .await
        .map_err(|_| ErrorCode::Internal)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; 8192];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .map_err(|_| ErrorCode::Internal)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finish().as_ref().to_vec())
}

pub async fn download_legacy(
    state: &AppState,
    owner: Uuid,
    requesting_self: bool,
    supports_ysm: bool,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    use tokio::io::AsyncReadExt;
    let temporary_path = legacy_read_path(owner, true);
    let temporary = requesting_self && temporary_path.exists();
    let path = if temporary {
        if !supports_ysm {
            let path = temporary_path.clone();
            let identity = tokio::task::spawn_blocking(move || super::package::inspect(&path))
                .await
                .map_err(|_| ApiError::Internal)?
                .map_err(legacy_error)?;
            if identity.model_type != ModelType::ModelFigura {
                return Err(ApiError::NotFound);
            }
        }
        temporary_path
    } else {
        ensure_legacy_import(state, owner, false)
            .await
            .map_err(legacy_error)?;
        let equipped = state.storage.equipped(owner).map_err(legacy_error)?;
        let avatar = if supports_ysm {
            equipped.primary
        } else {
            equipped.figura_compatibility
        }
        .ok_or(ApiError::NotFound)?;
        state
            .storage
            .blob_path(&avatar.raw_sha256)
            .map_err(legacy_error)?
    };
    let file = fs::File::open(&path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let length = file.metadata().await.map_err(internal_and_log)?.len();
    if temporary {
        // Windows permits deleting an open Tokio file; the handle serves this one-shot body.
        fs::remove_file(&path).await.map_err(internal_and_log)?;
    }
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file.take(length)));
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-length", &length.to_string()),
        ],
        body,
    )
        .into_response())
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
    if avatar.asset_id == "avatar" && legacy_read_path(owner, false).exists() {
        fs::remove_file(legacy_read_path(owner, false))
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
