//! PSF-only package authentication and purpose-bound, session-bound key grants.
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::GzDecoder;
use ring::{aead, hmac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

pub fn digest(bytes: &[u8]) -> String {
    faster_hex::hex_string(&crate::storage::sha256(bytes))
}

pub const MAX_SOURCE: usize = 64 * 1024 * 1024;
const MAX_PLAIN: u64 = 512 * 1024 * 1024;
const MAX_RESOURCE: usize = 32 * 1024 * 1024;
const GRANT_SECONDS: u64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    KeyUnavailable,
    KeyLength,
    InvalidPackage,
    Forbidden,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Purpose {
    Publication,
    Fitting,
    Consumption,
}

#[derive(Deserialize, Serialize)]
struct Claims {
    account: Uuid,
    session: String,
    content_sha256: String,
    purpose: Purpose,
    version: u32,
    expires_at: u64,
}

// Intentionally has no Debug implementation: this response contains a secret.
#[derive(Serialize)]
pub struct KeyGrant {
    pub content_sha256: String,
    pub package_key: String,
    pub grant: String,
    pub expires_at: u64,
    pub purpose: Purpose,
    pub version: u32,
}

/// Ephemeral admission for an HTTP publication. Never serialized into a source or reservation.
pub(crate) struct Publication {
    token: String,
    session: String,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl Publication {
    pub(crate) fn new(
        token: String,
        session: String,
        current: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            token,
            session,
            current,
        }
    }

    pub(crate) fn verify(
        &self,
        keys: &Keys,
        account: Uuid,
        content: &[u8],
        now: u64,
    ) -> Result<(), Error> {
        if !(self.current)() || content.len() != 32 {
            return Err(Error::Forbidden);
        }
        keys.verify(
            &self.token,
            account,
            &self.session,
            &faster_hex::hex_string(content),
            Purpose::Publication,
            now,
        )
    }
}

pub struct Keys {
    path: PathBuf,
    signer: hmac::Key,
    pub preparation_slots: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.debug_struct("PsfKeys").finish_non_exhaustive()
    }
}

impl Default for Keys {
    fn default() -> Self {
        let executable = std::env::current_exe().expect("Executable path is required");
        Self::beside_executable(&executable)
    }
}

impl Keys {
    pub fn check_configuration(&self) -> Result<(), Error> {
        let file = std::fs::File::open(&self.path).map_err(|_| Error::KeyUnavailable)?;
        let length = file.metadata().map_err(|_| Error::KeyUnavailable)?.len();
        if length != 32 {
            return Err(Error::KeyLength);
        }
        Ok(())
    }

    pub fn beside_executable(executable: &Path) -> Self {
        Self {
            path: executable
                .parent()
                .expect("Executable directory is required")
                .join("keys")
                .join("application.bin"),
            // Grant signatures must not use the application key: local fitting users may possess it.
            signer: hmac::Key::new(hmac::HMAC_SHA256, &rand::random::<[u8; 32]>()),
            preparation_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    #[cfg(test)]
    pub(crate) fn reference(path: PathBuf) -> Self {
        let mut keys = Self::beside_executable(Path::new("reference/sculptor.exe"));
        keys.path = path;
        keys
    }

    pub fn authorize(
        &self,
        source: &[u8],
        account: Uuid,
        session: &str,
        purpose: Purpose,
        now: u64,
    ) -> Result<KeyGrant, Error> {
        // Consumption must resolve an existing source and access policy, not accept arbitrary input.
        if purpose == Purpose::Consumption {
            return Err(Error::Forbidden);
        }
        self.issue(source, account, session, purpose, now)
    }

    /// Only an existing-resource handler may call this, after resolving the
    /// stored source and checking current access. The arbitrary-source prepare
    /// endpoint deliberately continues to reject Consumption.
    pub(crate) fn consume_authorized(
        &self,
        source: &[u8],
        account: Uuid,
        session: &str,
        now: u64,
    ) -> Result<KeyGrant, Error> {
        self.issue(source, account, session, Purpose::Consumption, now)
    }

    fn issue(
        &self,
        source: &[u8],
        account: Uuid,
        session: &str,
        purpose: Purpose,
        now: u64,
    ) -> Result<KeyGrant, Error> {
        let mut package_key = self.package_key(source)?;
        let claims = Claims {
            account,
            session: session.to_owned(),
            content_sha256: crate::service::psf::digest(source),
            purpose,
            version: 1,
            expires_at: now.saturating_add(GRANT_SECONDS),
        };
        let bytes = serde_json::to_vec(&claims).map_err(|_| Error::InvalidPackage)?;
        let signature = hmac::sign(&self.signer, &bytes);
        let grant = format!(
            "{}.{}",
            STANDARD.encode(&bytes),
            STANDARD.encode(signature.as_ref())
        );
        let response = KeyGrant {
            content_sha256: claims.content_sha256,
            package_key: STANDARD.encode(package_key),
            grant,
            expires_at: claims.expires_at,
            purpose,
            version: 1,
        };
        package_key.fill(0);
        Ok(response)
    }

    fn package_key(&self, source: &[u8]) -> Result<[u8; 32], Error> {
        let mut input = std::fs::File::open(&self.path).map_err(|_| Error::KeyUnavailable)?;
        let mut master = Vec::with_capacity(33);
        input
            .by_ref()
            .take(33)
            .read_to_end(&mut master)
            .map_err(|_| Error::KeyUnavailable)?;
        if master.len() != 32 {
            master.fill(0);
            return Err(Error::KeyLength);
        }
        let result = authenticate(source, &master);
        master.fill(0);
        result
    }

    pub(crate) fn authenticate_source(&self, source: &[u8]) -> Result<(), Error> {
        let mut key = self.package_key(source)?;
        key.fill(0);
        Ok(())
    }

    pub fn verify(
        &self,
        token: &str,
        account: Uuid,
        session: &str,
        content: &str,
        purpose: Purpose,
        now: u64,
    ) -> Result<(), Error> {
        if token.len() > 4096 {
            return Err(Error::Forbidden);
        }
        let (payload, signature) = token.split_once('.').ok_or(Error::Forbidden)?;
        let bytes = STANDARD.decode(payload).map_err(|_| Error::Forbidden)?;
        let signature = STANDARD.decode(signature).map_err(|_| Error::Forbidden)?;
        hmac::verify(&self.signer, &bytes, &signature).map_err(|_| Error::Forbidden)?;
        let claims: Claims = serde_json::from_slice(&bytes).map_err(|_| Error::Forbidden)?;
        if claims.account != account
            || claims.session != session
            || claims.content_sha256 != content
            || claims.purpose != purpose
            || claims.version != 1
            || claims.expires_at <= now
        {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
}

pub fn authenticate(source: &[u8], master: &[u8]) -> Result<[u8; 32], Error> {
    if source.len() < 64
        || source.len() > MAX_SOURCE
        || &source[..8] != b"PSF1PGSF"
        || u16::from_le_bytes(source[8..10].try_into().unwrap()) != 1
        || u16::from_le_bytes(source[10..12].try_into().unwrap()) != 2
        || source[12..16] != [0; 4]
        || master.len() != 32
    {
        return Err(Error::InvalidPackage);
    }
    let expected = u32::from_le_bytes(source[44..48].try_into().unwrap()) as u64;
    if expected > MAX_PLAIN {
        return Err(Error::InvalidPackage);
    }
    let salt = hmac::Key::new(hmac::HMAC_SHA256, &source[16..32]);
    let extracted = hmac::sign(&salt, master);
    let key = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, extracted.as_ref()),
        b"PieceGrassSklnFiles/1\x01",
    );
    let mut package_key: [u8; 32] = key.as_ref().try_into().unwrap();
    let result = inspect(source, &package_key, expected);
    if result.is_err() {
        package_key.fill(0);
    }
    result.map(|()| package_key)
}

fn inspect(source: &[u8], package_key: &[u8; 32], expected: u64) -> Result<(), Error> {
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, package_key)
        .map_err(|_| Error::InvalidPackage)?;
    let key = aead::LessSafeKey::new(key);
    let nonce = aead::Nonce::try_assume_unique_for_key(&source[32..44])
        .map_err(|_| Error::InvalidPackage)?;
    let mut buffer = source[48..].to_vec();
    let compressed = key
        .open_in_place(nonce, aead::Aad::from(&source[..48]), &mut buffer)
        .map_err(|_| Error::InvalidPackage)?;
    let mut plain = Vec::new();
    let decoded = GzDecoder::new(&*compressed)
        .take(expected + 1)
        .read_to_end(&mut plain);
    if decoded.is_err() || plain.len() as u64 != expected {
        plain.fill(0);
        buffer.fill(0);
        return Err(Error::InvalidPackage);
    }
    let payload = serde_json::from_slice::<Value>(&plain);
    plain.fill(0);
    buffer.fill(0);
    let payload = payload.map_err(|_| Error::InvalidPackage)?;
    if payload["format"] != "PieceGrassSklnFiles"
        || payload["version"] != 1
        || payload["entry"]["category"] != "skin"
        || !payload["entry"]["id"].is_string()
        || !payload["entry"]["skin"].is_string()
        || !payload["models"].is_object()
        || !payload["textures"].is_object()
    {
        return Err(Error::InvalidPackage);
    }
    let files = payload["files"].as_object().ok_or(Error::InvalidPackage)?;
    if files.len() > 4096 {
        return Err(Error::InvalidPackage);
    }
    let mut names = std::collections::HashSet::new();
    let mut total = 0usize;
    for (name, file) in files {
        if name.starts_with('/')
            || name.contains('\\')
            || name.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.chars().any(|c| c < ' ' || c == ':')
            })
            || !names.insert(name.to_lowercase())
        {
            return Err(Error::InvalidPackage);
        }
        let size = file["size"].as_u64().ok_or(Error::InvalidPackage)?;
        let encoded = file["data"].as_str().ok_or(Error::InvalidPackage)?;
        if size > MAX_RESOURCE as u64 || encoded.len() > MAX_RESOURCE.div_ceil(3) * 4 {
            return Err(Error::InvalidPackage);
        }
        let mut bytes = STANDARD
            .decode(encoded)
            .map_err(|_| Error::InvalidPackage)?;
        total += bytes.len();
        let valid = bytes.len() as u64 == size
            && total <= 256 * 1024 * 1024
            && STANDARD.encode(&bytes) == encoded
            && file["sha256"] == crate::service::psf::digest(&bytes);
        bytes.fill(0);
        if !valid {
            return Err(Error::InvalidPackage);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
