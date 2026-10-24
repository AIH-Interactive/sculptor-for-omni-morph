use crate::protocol::v1::{Ack, AvatarDescriptor, Delete, Equip, EquippedState, ErrorCode};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

mod sqlite;
pub(crate) mod outfit;
pub(crate) mod outfit_apply;
pub(crate) mod outfit_resources;
pub(crate) mod outfit_uploads;
mod outfit_sources;
pub(crate) mod outfit_projection;
pub(crate) mod outfit_state;
mod outfit_legacy;
mod legacy_publication;
pub mod library;
pub(crate) mod recycle;
pub(crate) mod library_equipment;
pub(crate) mod library_wardrobe;
pub(crate) mod wardrobe_recycle;
pub(crate) mod library_admin_preview;
mod layout;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod layout_tests;

pub type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone, Default, Deserialize, Serialize)]
struct StoredAvatar {
    descriptor: Vec<u8>,
    tombstone: bool,
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq)]
struct AssetRef {
    id: String,
    revision: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Owner {
    revision: u64,
    primary: Option<AssetRef>,
    compatibility: Option<AssetRef>,
    legacy_sha256: Option<Vec<u8>>,
    assets: BTreeMap<String, BTreeMap<u64, StoredAvatar>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Operation {
    kind: String,
    fingerprint: Vec<u8>,
    result: Vec<u8>,
    expires: u64,
}

#[derive(Clone, Deserialize, Serialize)]
struct Manifest {
    schema_version: u32,
    instance_id: String,
    owners: BTreeMap<Uuid, Owner>,
    operations: BTreeMap<String, Operation>,
    #[serde(default)]
    ysm: BTreeMap<Uuid, crate::service::ysm::Record>,
}

/// Immutable package blobs plus transactional SQLite metadata.
/// The old .moon tree is never the destination of a new-protocol file upload.
pub struct Storage {
    root: PathBuf,
    uploads: PathBuf,
    models: PathBuf,
    wardrobe: PathBuf,
    recycle: PathBuf,
    committed: Mutex<Manifest>,
    blob_publication: Mutex<()>,
    database: Mutex<rusqlite::Connection>,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Storage").finish_non_exhaustive()
    }
}

pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

impl Storage {
    #[cfg(test)]
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        Self::open_paths(
            root.clone(),
            root.join("metadata.sqlite3"),
            root.join("uploads"),
            root.join("blobs"),
        )
    }

    pub fn open_avatars(avatars: impl AsRef<Path>, database: impl AsRef<Path>) -> Result<Self> {
        let avatars = avatars.as_ref();
        let database = database.as_ref();
        layout::migrate(avatars, database)?;
        let storage = Self::open_paths(
            avatars.join("cloud-v1"),
            database.to_path_buf(),
            avatars.join("upload"),
            avatars.join("models"),
        )?;
        storage.separate_wardrobe_files()?;
        storage.reconcile_library_recycle()?;
        // Mark only after files and metadata are usable. Retained backups must not
        // resurrect deleted models or consumed temporary uploads on later starts.
        {
            let database = storage.database.lock().map_err(|_| ErrorCode::Internal)?;
            database
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('avatars_layout', 'models-upload-v1')
                     ON CONFLICT(key) DO NOTHING",
                    [],
                )
                .map_err(sqlite::sql_error)?;
        }
        Ok(storage)
    }

    fn open_paths(
        root: PathBuf,
        database_path: PathBuf,
        uploads: PathBuf,
        models: PathBuf,
    ) -> Result<Self> {
        fs::create_dir_all(&models).map_err(|_| ErrorCode::Internal)?;
        let parent = models.parent().ok_or(ErrorCode::Internal)?;
        let wardrobe = parent.join("wardrobe");
        let recycle = parent.join("recycle");
        fs::create_dir_all(&wardrobe).map_err(|_| ErrorCode::Internal)?;
        fs::create_dir_all(&recycle).map_err(|_| ErrorCode::Internal)?;
        fs::create_dir_all(&uploads).map_err(|_| ErrorCode::Internal)?;
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent).map_err(|_| ErrorCode::Internal)?;
        }
        let (database, manifest) = sqlite::open(&root, &database_path)?;
        // Upload grants live in memory. After restart no previous .part can be committed.
        // Never clean an unknown schema; keep immutable blobs from interrupted commits.
        for entry in fs::read_dir(&uploads).map_err(|_| ErrorCode::Internal)? {
            let entry = entry.map_err(|_| ErrorCode::Internal)?;
            let path = entry.path();
            let is_file = entry
                .file_type()
                .map_err(|_| ErrorCode::Internal)?
                .is_file();
            if is_file
                && path
                    .extension()
                    .is_some_and(|extension| extension == "part")
            {
                fs::remove_file(path).map_err(|_| ErrorCode::Internal)?;
            }
        }
        let storage = Self {
            root,
            uploads,
            models,
            wardrobe,
            recycle,
            committed: Mutex::new(manifest),
            blob_publication: Mutex::new(()),
            database: Mutex::new(database),
        };
        storage.recover_legacy_publications()?;
        storage.transaction(|_| Ok(()))?;
        Ok(storage)
    }

    pub fn instance_id(&self) -> String {
        self.committed
            .lock()
            .expect("storage mutex poisoned")
            .instance_id
            .clone()
    }

    pub fn upload_path(&self, owner: Uuid, operation: Uuid) -> PathBuf {
        self.uploads.join(format!("{owner}-{operation}.part"))
    }

    pub fn blob_path(&self, hash: &[u8]) -> Result<PathBuf> {
        if hash.len() != 32 {
            return Err(ErrorCode::InvalidMessage);
        }
        let library = self.library_blob_path(hash)?;
        let wardrobe = self.wardrobe.join(faster_hex::hex_string(hash));
        Ok(if library.is_file() { library } else { wardrobe })
    }

    pub fn library_blob_path(&self, hash: &[u8]) -> Result<PathBuf> {
        if hash.len() != 32 {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(self.models.join(faster_hex::hex_string(hash)))
    }

    fn transaction<T>(&self, change: impl FnOnce(&mut Manifest) -> Result<T>) -> Result<T> {
        self.transaction_with_library(change, |_| Ok(()))
    }

    fn transaction_with_library<T>(
        &self,
        change: impl FnOnce(&mut Manifest) -> Result<T>,
        library_change: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<T> {
        self.transaction_with_library_result(change, |transaction, _| library_change(transaction))
    }

    fn transaction_with_library_result<T>(
        &self,
        change: impl FnOnce(&mut Manifest) -> Result<T>,
        library_change: impl FnOnce(&rusqlite::Transaction<'_>, &T) -> Result<()>,
    ) -> Result<T> {
        let mut committed = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let mut candidate = committed.clone();
        let result = change(&mut candidate)?;
        // Equipment and its durable recovery revocation belong to one commit.
        for (owner, record) in &mut candidate.ysm {
            if candidate.owners.get(owner).is_none_or(|account| {
                account.revision != record.equipment_revision || account.primary.is_none()
            }) {
                record.active = false;
            }
        }
        candidate
            .operations
            .retain(|_, operation| operation.expires > now_seconds());
        let mut database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        sqlite::save_with(&mut database, &candidate, &self.root, |transaction| {
            library_change(transaction, &result)
        })?;
        *committed = candidate;
        Ok(result)
    }

    pub fn users(&self) -> Result<Vec<crate::auth::Userinfo>> {
        let database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        let mut statement = database
            .prepare("SELECT profile FROM users")
            .map_err(sqlite::sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite::sql_error)?;
        rows.map(|row| {
            serde_json::from_str(&row.map_err(sqlite::sql_error)?).map_err(|_| ErrorCode::Internal)
        })
        .collect()
    }

    pub fn has_owner(&self, uuid: Uuid) -> bool {
        self.committed
            .lock()
            .is_ok_and(|manifest| manifest.owners.contains_key(&uuid))
    }

    pub fn save_user(&self, user: &crate::auth::Userinfo) -> Result<()> {
        let mut persistent = user.clone();
        persistent.token = None;
        let profile = serde_json::to_string(&persistent).map_err(|_| ErrorCode::Internal)?;
        let database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        database.execute(
            "INSERT INTO users(uuid, profile) VALUES (?1, ?2) ON CONFLICT(uuid) DO UPDATE SET profile = excluded.profile",
            rusqlite::params![user.uuid.to_string(), profile],
        ).map_err(sqlite::sql_error)?;
        Ok(())
    }

    pub fn upload_permissions(&self) -> Result<Vec<(Uuid, bool)>> {
        let database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        let mut statement = database
            .prepare("SELECT uuid, allowed FROM upload_permissions")
            .map_err(sqlite::sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .map_err(sqlite::sql_error)?;
        rows.map(|row| {
            let (uuid, allowed) = row.map_err(sqlite::sql_error)?;
            Ok((
                Uuid::parse_str(&uuid).map_err(|_| ErrorCode::Internal)?,
                allowed,
            ))
        })
        .collect()
    }

    pub fn save_upload_permission(&self, uuid: Uuid, allowed: bool) -> Result<()> {
        let database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        database.execute(
            "INSERT INTO upload_permissions VALUES (?1, ?2) ON CONFLICT(uuid) DO UPDATE SET allowed = excluded.allowed",
            rusqlite::params![uuid.to_string(), allowed],
        ).map_err(sqlite::sql_error)?;
        Ok(())
    }

    pub fn avatar(&self, owner: Uuid, id: &str, revision: u64) -> Result<AvatarDescriptor> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let history = manifest
            .owners
            .get(&owner)
            .and_then(|owner| owner.assets.get(id))
            .ok_or(ErrorCode::NotFound)?;
        let stored = if revision == 0 {
            history.last_key_value().map(|(_, stored)| stored)
        } else {
            history.get(&revision)
        }
        .filter(|stored| !stored.tombstone)
        .ok_or(ErrorCode::NotFound)?;
        AvatarDescriptor::decode(stored.descriptor.as_slice()).map_err(|_| ErrorCode::Internal)
    }

    pub fn equipped(&self, owner: Uuid) -> Result<EquippedState> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        equipment(owner, manifest.owners.get(&owner))
    }

    pub fn publish_ysm(
        &self,
        owner: Uuid,
        publisher_key: &[u8],
        body: &crate::protocol::v1::envelope::Body,
        now: u64,
    ) -> Result<crate::service::ysm::Published> {
        self.transaction(|manifest| {
            let equipped = equipment(owner, manifest.owners.get(&owner))?;
            let record = manifest.ysm.entry(owner).or_default();
            crate::service::ysm::publish(record, owner, &equipped, publisher_key, body, now)
        })
    }

    pub fn publish_legacy_ysm(
        &self,
        owner: Uuid,
        publisher_key: &[u8],
        bytes: &[u8],
        now: u64,
    ) -> Result<Vec<crate::protocol::v1::envelope::Body>> {
        self.transaction(|manifest| {
            let equipped = equipment(owner, manifest.owners.get(&owner))?;
            let avatar = equipped.primary.as_ref().ok_or(ErrorCode::NotFound)?;
            let frame = crate::service::ysm::legacy::decode(bytes, avatar.revision)?;
            let record = manifest.ysm.entry(owner).or_default();
            crate::service::ysm::publish_legacy(
                record,
                owner,
                &equipped,
                publisher_key,
                &frame,
                &sha256(bytes),
                now,
            )
        })
    }

    pub fn ysm_snapshot(&self, owner: Uuid, now: u64) -> Result<crate::protocol::v1::YsmSnapshot> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let equipped = equipment(owner, manifest.owners.get(&owner))?;
        let record = manifest
            .ysm
            .get(&owner)
            .ok_or(ErrorCode::SnapshotRequired)?;
        crate::service::ysm::snapshot(record, &equipped, now)
    }

    pub fn ysm_resume(
        &self,
        cursor: &crate::protocol::v1::Resume,
        server_epoch: u64,
        now: u64,
    ) -> Result<Vec<crate::protocol::v1::envelope::Body>> {
        let owner = Uuid::parse_str(&cursor.owner).map_err(|_| ErrorCode::InvalidMessage)?;
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let equipped = equipment(owner, manifest.owners.get(&owner))?;
        let record = manifest
            .ysm
            .get(&owner)
            .ok_or(ErrorCode::SnapshotRequired)?;
        crate::service::ysm::resume(record, &equipped, cursor, server_epoch, now)
    }

    pub fn ysm_event_deadline(
        &self,
        owner: Uuid,
        id: &crate::protocol::v1::YsmIdentity,
    ) -> Result<u64> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let record = manifest
            .ysm
            .get(&owner)
            .ok_or(ErrorCode::SnapshotRequired)?;
        crate::service::ysm::event_deadline(record, id)
    }

    pub fn legacy_hash_matches(&self, owner: Uuid, hash: &[u8]) -> bool {
        self.committed
            .lock()
            .ok()
            .and_then(|manifest| {
                manifest
                    .owners
                    .get(&owner)
                    .and_then(|owner| owner.legacy_sha256.clone())
            })
            .as_deref()
            == Some(hash)
    }

    pub fn import_legacy(
        &self,
        avatar: AvatarDescriptor,
        force_equipped: bool,
    ) -> Result<AvatarDescriptor> {
        self.import_legacy_with_outfit_limits(
            avatar,
            force_equipped,
            &crate::service::outfit_limits::OutfitLimits::default(),
        )
    }

    pub(crate) fn import_legacy_with_outfit_limits(
        &self,
        avatar: AvatarDescriptor,
        force_equipped: bool,
        limits: &crate::service::outfit_limits::OutfitLimits,
    ) -> Result<AvatarDescriptor> {
        self.import_legacy_publication(avatar, force_equipped, limits, None)
    }

    fn import_legacy_publication(
        &self,
        mut avatar: AvatarDescriptor,
        force_equipped: bool,
        limits: &crate::service::outfit_limits::OutfitLimits,
        publication: Option<Uuid>,
    ) -> Result<AvatarDescriptor> {
        let owner = Uuid::parse_str(&avatar.owner).map_err(|_| ErrorCode::InvalidMessage)?;
        let prepare = |manifest: &mut Manifest| {
            let account = manifest.owners.entry(owner).or_default();
            let unchanged = account.legacy_sha256.as_ref() == Some(&avatar.raw_sha256);
            if unchanged {
                if let Some(stored) = account
                    .assets
                    .get("avatar")
                    .and_then(|history| history.last_key_value())
                    .map(|(_, stored)| stored)
                    .filter(|stored| !stored.tombstone)
                {
                    let current = AvatarDescriptor::decode(stored.descriptor.as_slice())
                        .map_err(|_| ErrorCode::Internal)?;
                    if force_equipped && current.raw_sha256 == avatar.raw_sha256 {
                        let compatibility = (current.model_type == 1).then(|| asset_ref(&current));
                        select(account, Some(asset_ref(&current)), compatibility);
                    }
                    if current.raw_sha256 == avatar.raw_sha256 {
                        return Ok((current, force_equipped));
                    }
                }
            }
            account.legacy_sha256 = Some(avatar.raw_sha256.clone());
            let history = account.assets.entry("avatar".into()).or_default();
            avatar.revision = history
                .last_key_value()
                .map(|(revision, _)| revision + 1)
                .unwrap_or(1);
            history.insert(
                avatar.revision,
                StoredAvatar {
                    descriptor: avatar.encode_to_vec(),
                    tombstone: false,
                },
            );
            let compatibility = (avatar.model_type == 1).then(|| asset_ref(&avatar));
            select(account, Some(asset_ref(&avatar)), compatibility);
            Ok((avatar.clone(), true))
        };
        let (result, _) =
            self.transaction_with_library_result(prepare, |transaction, (avatar, normalize)| {
                if *normalize {
                    outfit_legacy::normalize_native(transaction, owner, Some(avatar), limits)?;
                }
                if let Some(operation) = publication {
                    legacy_publication::finish(transaction, owner, operation)?;
                }
                Ok(())
            })?;
        Ok(result)
    }

    pub fn commit_avatar(
        &self,
        owner: Uuid,
        operation: &str,
        fingerprint: &[u8],
        avatar: AvatarDescriptor,
        max_assets: usize,
    ) -> Result<AvatarDescriptor> {
        if self.library_asset_bound(owner, &avatar.asset_id)? {
            return Err(ErrorCode::Forbidden);
        }
        self.commit_avatar_with_library(
            owner,
            operation,
            fingerprint,
            avatar,
            max_assets,
            |_, _| Ok(()),
        )
    }

    pub(crate) fn commit_avatar_with_library(
        &self,
        owner: Uuid,
        operation: &str,
        fingerprint: &[u8],
        mut avatar: AvatarDescriptor,
        max_assets: usize,
        library_change: impl FnOnce(&rusqlite::Transaction<'_>, &AvatarDescriptor) -> Result<()>,
    ) -> Result<AvatarDescriptor> {
        self.transaction_with_library_result(
            |manifest| {
                if let Some(result) =
                    replay::<AvatarDescriptor>(manifest, owner, operation, fingerprint)?
                {
                    return Ok(result);
                }
                let account = manifest.owners.entry(owner).or_default();
                if !account.assets.contains_key(&avatar.asset_id)
                    && account.assets.len() >= max_assets
                {
                    return Err(ErrorCode::LimitExceeded);
                }
                let history = account.assets.entry(avatar.asset_id.clone()).or_default();
                if history.len() >= 1024 {
                    return Err(ErrorCode::LimitExceeded);
                }
                avatar.owner = owner.to_string();
                avatar.revision = history
                    .last_key_value()
                    .map(|(revision, _)| revision + 1)
                    .unwrap_or(1);
                history.insert(
                    avatar.revision,
                    StoredAvatar {
                        descriptor: avatar.encode_to_vec(),
                        tombstone: false,
                    },
                );
                remember(manifest, owner, operation, fingerprint, &avatar)?;
                Ok(avatar.clone())
            },
            library_change,
        )
    }

    pub fn replay_avatar(
        &self,
        owner: Uuid,
        operation: &str,
        fingerprint: &[u8],
    ) -> Result<Option<AvatarDescriptor>> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        replay(&manifest, owner, operation, fingerprint)
    }

    pub fn completed_upload(
        &self,
        owner: Uuid,
        request: &crate::protocol::v1::CommitUpload,
    ) -> Result<Option<AvatarDescriptor>> {
        let manifest = self.committed.lock().map_err(|_| ErrorCode::Internal)?;
        let Some(saved) = manifest
            .operations
            .get(&format!("{owner}/{}", request.operation_id))
            .filter(|saved| saved.expires > now_seconds())
        else {
            return Ok(None);
        };
        if saved.kind != AvatarDescriptor::KIND {
            return Err(ErrorCode::Conflict);
        }
        let avatar =
            AvatarDescriptor::decode(saved.result.as_slice()).map_err(|_| ErrorCode::Internal)?;
        if avatar.raw_sha256 != request.raw_sha256 || avatar.raw_length != request.raw_length {
            return Err(ErrorCode::Conflict);
        }
        Ok(Some(avatar))
    }

    pub fn publish_staged(&self, path: &Path, hash: &[u8]) -> Result<()> {
        let _publication = self.lock_blob_files()?;
        self.publish_staged_locked(path, hash, false)
    }

    pub(crate) fn lock_blob_files(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.blob_publication.lock().map_err(|_| ErrorCode::Internal)
    }

    /// Caller holds lock_blob_files through the corresponding metadata commit.
    pub(crate) fn publish_staged_locked(&self, path: &Path, hash: &[u8], library: bool) -> Result<()> {
        if hash.len() != 32 {
            return Err(ErrorCode::InvalidMessage);
        }
        let target = if library {
            self.library_blob_path(hash)?
        } else {
            self.wardrobe.join(faster_hex::hex_string(hash))
        };
        if target.exists() {
            return Ok(());
        }
        fs::rename(path, target).map_err(|_| ErrorCode::Internal)
    }

    pub fn equip(&self, owner: Uuid, request: &Equip) -> Result<EquippedState> {
        self.equip_with_outfit_limits(
            owner,
            request,
            &crate::service::outfit_limits::OutfitLimits::default(),
        )
    }

    pub(crate) fn equip_with_outfit_limits(
        &self,
        owner: Uuid,
        request: &Equip,
        limits: &crate::service::outfit_limits::OutfitLimits,
    ) -> Result<EquippedState> {
        if !request.clear && self.library_asset_bound(owner, &request.asset_id)? {
            return Err(ErrorCode::Forbidden);
        }
        let fingerprint = sha256(&[b"equip".as_slice(), &request.encode_to_vec()].concat());
        let prepare = |manifest: &mut Manifest| {
            if let Some(result) =
                replay::<EquippedState>(manifest, owner, &request.operation_id, &fingerprint)?
            {
                return Ok((result, false));
            }
            let account = manifest.owners.entry(owner).or_default();
            if account.revision != request.expected_revision {
                return Err(ErrorCode::Conflict);
            }
            if request.clear {
                select(account, None, None);
            } else {
                let avatar = account
                    .assets
                    .get(&request.asset_id)
                    .and_then(|history| history.get(&request.asset_revision))
                    .filter(|stored| !stored.tombstone)
                    .ok_or(ErrorCode::NotFound)?;
                let descriptor = AvatarDescriptor::decode(avatar.descriptor.as_slice())
                    .map_err(|_| ErrorCode::Internal)?;
                let reference = Some(asset_ref(&descriptor));
                let compatibility = match descriptor.model_type {
                    1 => reference.clone(),
                    2 | 3 if request.retain_figura_compatibility => account.compatibility.clone(),
                    2 | 3 => None,
                    _ => return Err(ErrorCode::UnsupportedCapability),
                };
                select(account, reference, compatibility);
            }
            let result = equipment(owner, Some(account))?;
            remember(
                manifest,
                owner,
                &request.operation_id,
                &fingerprint,
                &result,
            )?;
            Ok((result, true))
        };
        let (result, _) =
            self.transaction_with_library_result(prepare, |transaction, (result, fresh)| {
                if *fresh {
                    outfit_legacy::normalize_native(
                        transaction,
                        owner,
                        result.primary.as_ref(),
                        limits,
                    )?;
                }
                Ok(())
            })?;
        Ok(result)
    }

    pub fn delete(&self, owner: Uuid, request: &Delete) -> Result<Ack> {
        self.delete_with_outfit_limits(
            owner,
            request,
            &crate::service::outfit_limits::OutfitLimits::default(),
            false,
        )
    }

    pub(crate) fn delete_with_outfit_limits(
        &self,
        owner: Uuid,
        request: &Delete,
        limits: &crate::service::outfit_limits::OutfitLimits,
        clear_entire_outfit: bool,
    ) -> Result<Ack> {
        let _publication = self.lock_blob_files()?;
        if self.library_asset_bound(owner, &request.asset_id)? {
            return Err(ErrorCode::Forbidden);
        }
        let kind = if clear_entire_outfit {
            b"delete-legacy-outfit".as_slice()
        } else {
            b"delete".as_slice()
        };
        let fingerprint = sha256(&[kind, &request.encode_to_vec()].concat());
        let prepare = |manifest: &mut Manifest| {
            if let Some(result) =
                replay::<Ack>(manifest, owner, &request.operation_id, &fingerprint)?
            {
                return Ok((result, false));
            }
            let account = manifest.owners.get_mut(&owner).ok_or(ErrorCode::NotFound)?;
            let removed_base = account
                .primary
                .as_ref()
                .is_some_and(|reference| reference.id == request.asset_id);
            let history = account
                .assets
                .get_mut(&request.asset_id)
                .ok_or(ErrorCode::NotFound)?;
            let (revision, latest) = history.last_key_value().ok_or(ErrorCode::NotFound)?;
            if *revision != request.expected_revision {
                return Err(ErrorCode::Conflict);
            }
            if latest.tombstone {
                return Err(ErrorCode::NotFound);
            }
            self.archive_wardrobe(owner, request, history)?;
            for stored in history.values_mut() {
                stored.tombstone = true;
            }
            let primary = account
                .primary
                .clone()
                .filter(|reference| !clear_entire_outfit && reference.id != request.asset_id);
            let compatibility = account
                .compatibility
                .clone()
                .filter(|reference| !clear_entire_outfit && reference.id != request.asset_id);
            let previous_revision = account.revision;
            select(account, primary, compatibility);
            if account.revision == previous_revision {
                account.revision += 1;
            }
            let result = Ack {
                operation_id: request.operation_id.clone(),
                revision: account.revision,
                sequence: 0,
            };
            remember(
                manifest,
                owner,
                &request.operation_id,
                &fingerprint,
                &result,
            )?;
            Ok((result, clear_entire_outfit || removed_base))
        };
        let (result, _) =
            self.transaction_with_library_result(prepare, |transaction, (_, normalize)| {
                if *normalize {
                    outfit_legacy::normalize_native(transaction, owner, None, limits)?;
                }
                Ok(())
            })?;
        self.clean_deleted_wardrobe(owner, &request.asset_id)?;
        Ok(result)
    }
}

fn asset_ref(avatar: &AvatarDescriptor) -> AssetRef {
    AssetRef {
        id: avatar.asset_id.clone(),
        revision: avatar.revision,
    }
}

fn select(account: &mut Owner, primary: Option<AssetRef>, compatibility: Option<AssetRef>) {
    if account.primary != primary || account.compatibility != compatibility {
        account.revision += 1;
        account.primary = primary;
        account.compatibility = compatibility;
    }
}

fn equipment(owner: Uuid, account: Option<&Owner>) -> Result<EquippedState> {
    let descriptor = |reference: &Option<AssetRef>| -> Result<Option<AvatarDescriptor>> {
        let Some(reference) = reference else {
            return Ok(None);
        };
        let stored = account
            .and_then(|owner| owner.assets.get(&reference.id))
            .and_then(|history| history.get(&reference.revision))
            .filter(|stored| !stored.tombstone)
            .ok_or(ErrorCode::Internal)?;
        AvatarDescriptor::decode(stored.descriptor.as_slice())
            .map(Some)
            .map_err(|_| ErrorCode::Internal)
    };
    match account {
        Some(account) => Ok(EquippedState {
            owner: owner.to_string(),
            revision: account.revision,
            primary: descriptor(&account.primary)?,
            figura_compatibility: descriptor(&account.compatibility)?,
        }),
        None => Ok(EquippedState {
            owner: owner.to_string(),
            ..Default::default()
        }),
    }
}

trait StoredResult: Message + Default {
    const KIND: &'static str;
}

impl StoredResult for AvatarDescriptor {
    const KIND: &'static str = "avatar";
}
impl StoredResult for EquippedState {
    const KIND: &'static str = "equipped";
}
impl StoredResult for Ack {
    const KIND: &'static str = "ack";
}

fn replay<T: StoredResult>(
    manifest: &Manifest,
    owner: Uuid,
    operation: &str,
    fingerprint: &[u8],
) -> Result<Option<T>> {
    let Some(saved) = manifest
        .operations
        .get(&format!("{owner}/{operation}"))
        .filter(|operation| operation.expires > now_seconds())
    else {
        return Ok(None);
    };
    if saved.kind != T::KIND || saved.fingerprint != fingerprint {
        return Err(ErrorCode::Conflict);
    }
    T::decode(saved.result.as_slice())
        .map(Some)
        .map_err(|_| ErrorCode::Internal)
}

fn remember<T: StoredResult>(
    manifest: &mut Manifest,
    owner: Uuid,
    operation: &str,
    fingerprint: &[u8],
    result: &T,
) -> Result<()> {
    let prefix = format!("{owner}/");
    if manifest
        .operations
        .iter()
        .filter(|(key, value)| key.starts_with(&prefix) && value.expires > now_seconds())
        .count()
        >= 128
    {
        return Err(ErrorCode::LimitExceeded);
    }
    manifest.operations.insert(
        format!("{owner}/{operation}"),
        Operation {
            kind: T::KIND.into(),
            fingerprint: fingerprint.to_vec(),
            result: result.encode_to_vec(),
            expires: now_seconds() + 3600,
        },
    );
    Ok(())
}
