//! Bounded inspection of the existing client distribution container.
//! This scanner never constructs a model, executes scripts or decrypts ciphertext.
use crate::protocol::v1::{ErrorCode, ModelType};
use flate2::read::GzDecoder;
use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_DECOMPRESSED: u64 = 256 * 1024 * 1024;
pub const MAX_RESOURCE: u64 = 32 * 1024 * 1024;
pub const MAX_CIPHERTEXT: u64 = 64 * 1024 * 1024;
pub const MAX_LIST_ENTRIES: u64 = 65_536;
pub const MAX_DEPTH: usize = 64;
pub const INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);

type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageLimits {
    pub max_nodes: Option<u64>,
    pub max_list_entries: Option<u64>,
    pub max_depth: usize,
    pub inspection_timeout: Duration,
    pub max_decompressed: u64,
    pub max_resource: u64,
    pub max_ciphertext: u64,
}

impl PackageLimits {
    pub fn from_limitations(limitations: &crate::state::Limitations) -> Self {
        Self {
            max_nodes: optional_limit(limitations.max_package_nodes),
            max_list_entries: optional_limit(limitations.max_package_list_entries),
            max_depth: limitations.max_package_depth,
            inspection_timeout: Duration::from_secs(
                limitations.max_package_inspection_seconds,
            ),
            max_decompressed: limitations.max_package_decompressed_bytes,
            max_resource: limitations.max_package_resource_bytes,
            max_ciphertext: limitations.max_package_ciphertext_bytes,
        }
    }

    /// Trusted reads only enforce transport-like safety budgets. Upload policy
    /// (node/list/depth quotas) must never make an already accepted blob unreadable.
    pub fn trusted() -> Self {
        Self {
            max_nodes: None,
            max_list_entries: None,
            max_depth: 4096,
            inspection_timeout: Duration::from_secs(30),
            max_decompressed: MAX_DECOMPRESSED,
            max_resource: MAX_RESOURCE,
            max_ciphertext: MAX_CIPHERTEXT,
        }
    }
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_nodes: Some(262_144),
            max_list_entries: Some(MAX_LIST_ENTRIES),
            max_depth: MAX_DEPTH,
            inspection_timeout: INSPECTION_TIMEOUT,
            max_decompressed: MAX_DECOMPRESSED,
            max_resource: MAX_RESOURCE,
            max_ciphertext: MAX_CIPHERTEXT,
        }
    }
}

fn optional_limit(value: i64) -> Option<u64> {
    match value {
        -1 => None,
        value if value >= 0 => Some(value as u64),
        _ => None,
    }
}


/// Java DataOutputStream writes UTF-16 code units using modified UTF-8, including
/// surrogate pairs. Decode metadata exactly rather than replacing non-BMP names.
fn modified_utf8(bytes: &[u8]) -> Result<String> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let (width, value) = if first < 0x80 {
            (1, u16::from(first))
        } else if first & 0xe0 == 0xc0 {
            let second = *bytes.get(index + 1).ok_or(ErrorCode::InvalidMessage)?;
            if second & 0xc0 != 0x80 {
                return Err(ErrorCode::InvalidMessage);
            }
            let value = (u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f);
            if value < 0x80 && value != 0 {
                return Err(ErrorCode::InvalidMessage);
            }
            (2, value)
        } else if first & 0xf0 == 0xe0 {
            let second = *bytes.get(index + 1).ok_or(ErrorCode::InvalidMessage)?;
            let third = *bytes.get(index + 2).ok_or(ErrorCode::InvalidMessage)?;
            if second & 0xc0 != 0x80 || third & 0xc0 != 0x80 {
                return Err(ErrorCode::InvalidMessage);
            }
            let value = (u16::from(first & 0x0f) << 12)
                | (u16::from(second & 0x3f) << 6)
                | u16::from(third & 0x3f);
            if value < 0x800 {
                return Err(ErrorCode::InvalidMessage);
            }
            (3, value)
        } else {
            return Err(ErrorCode::InvalidMessage);
        };
        units.push(value);
        index += width;
    }
    String::from_utf16(&units).map_err(|_| ErrorCode::InvalidMessage)
}

pub struct PackageIdentity {
    pub model_type: ModelType,
    pub content_hash: Option<Vec<u8>>,
    pub nbt_sha256: Vec<u8>,
    pub ciphertext_sha256: Option<Vec<u8>>,
    pub internal_name: Option<String>,
    pub authors: Vec<String>,
    pub authors_text: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    fn text(output: &mut Vec<u8>, value: &str) {
        output.extend_from_slice(&(value.len() as u16).to_be_bytes());
        output.extend_from_slice(value.as_bytes());
    }

    fn string_tag(output: &mut Vec<u8>, name: &str, value: &str) {
        output.push(8);
        text(output, name);
        text(output, value);
    }

    pub(crate) fn fixture(encrypted: bool, private_field: bool) -> (Vec<u8>, Vec<u8>) {
        let ciphertext = b"YSGP\0\0\0\x01synthetic-ciphertext-not-a-real-model";
        let identity = crate::storage::sha256(if encrypted {
            ciphertext
        } else {
            b"synthetic-source-identity"
        });
        let mut nbt = vec![10, 0, 0, 10];
        text(&mut nbt, "metadata");
        string_tag(&mut nbt, "format", "ysm-native");
        nbt.extend_from_slice(&[0, 10]);
        text(&mut nbt, "ysm");
        string_tag(
            &mut nbt,
            "content_sha256",
            &faster_hex::hex_string(&identity),
        );
        if encrypted {
            nbt.push(7);
            text(&mut nbt, "source_ciphertext");
            nbt.extend_from_slice(&(ciphertext.len() as i32).to_be_bytes());
            nbt.extend_from_slice(ciphertext);
        } else {
            string_tag(&mut nbt, "kind", "PLAYER");
        }
        if private_field {
            string_tag(&mut nbt, "decryption_key", "must-never-be-published");
        }
        nbt.extend_from_slice(&[0, 0]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&nbt).unwrap();
        (encoder.finish().unwrap(), identity)
    }

    #[test]
    fn large_model_is_not_rejected_by_a_read_time_node_quota() {
        let (bytes, _) = large_node_fixture();
        assert!(inspect_with_budget(bytes.as_slice(), MAX_DECOMPRESSED).is_ok());
        assert!(inspect_with_limits(bytes.as_slice(), PackageLimits::trusted()).is_ok());
        assert!(matches!(
            inspect_with_limits(
                bytes.as_slice(),
                PackageLimits {
                    max_nodes: Some(262_144),
                    ..PackageLimits::default()
                }
            ),
            Err(ErrorCode::LimitExceeded)
        ));
        assert!(matches!(
            inspect_with_limits(
                bytes.as_slice(),
                PackageLimits {
                    max_decompressed: 8,
                    ..PackageLimits::default()
                }
            ),
            Err(ErrorCode::LimitExceeded)
        ));
    }

    #[test]
    fn oversized_list_limit_is_configurable() {
        let bytes = oversized_list_fixture(8_192);
        assert!(matches!(
            inspect_with_limits(
                bytes.as_slice(),
                PackageLimits {
                    max_list_entries: Some(4_096),
                    ..PackageLimits::default()
                }
            ),
            Err(ErrorCode::LimitExceeded)
        ));
        assert!(inspect_with_limits(
            bytes.as_slice(),
            PackageLimits {
                max_list_entries: Some(8_192),
                ..PackageLimits::default()
            }
        )
        .is_ok());
        assert!(inspect_with_limits(
            bytes.as_slice(),
            PackageLimits {
                max_list_entries: None,
                ..PackageLimits::default()
            }
        )
        .is_ok());
    }

    fn oversized_list_fixture(length: i32) -> Vec<u8> {
        let (bytes, _) = fixture(false, false);
        let mut nbt = Vec::new();
        GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut nbt)
            .unwrap();
        nbt.pop();
        nbt.push(9);
        text(&mut nbt, "vertices");
        nbt.push(1);
        nbt.extend_from_slice(&length.to_be_bytes());
        nbt.extend(vec![0u8; length as usize]);
        nbt.push(0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&nbt).unwrap();
        encoder.finish().unwrap()
    }

    pub(crate) fn large_node_fixture() -> (Vec<u8>, Vec<u8>) {
        let (bytes, identity) = fixture(false, false);
        let mut nbt = Vec::new();
        GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut nbt)
            .unwrap();
        nbt.pop();
        for index in 0..513 {
            nbt.push(9);
            text(&mut nbt, &format!("vertices_{index}"));
            nbt.push(5);
            nbt.extend_from_slice(&512i32.to_be_bytes());
            nbt.extend_from_slice(&[0; 512 * 4]);
        }
        nbt.push(0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&nbt).unwrap();
        (encoder.finish().unwrap(), identity)
    }

    #[test]
    fn inspect_container_without_decryption_and_enforce_budgets() {
        for encrypted in [false, true] {
            let (bytes, hash) = fixture(encrypted, false);
            let identity = inspect_with_budget(bytes.as_slice(), MAX_DECOMPRESSED).unwrap();
            assert_eq!(
                identity.model_type,
                if encrypted {
                    ModelType::ModelYsmEncrypted
                } else {
                    ModelType::ModelYsmPlain
                }
            );
            assert_eq!(identity.content_hash, Some(hash));
            assert!(matches!(
                inspect_with_budget(bytes.as_slice(), 8),
                Err(ErrorCode::LimitExceeded)
            ));
            assert!(inspect_with_budget(&bytes[..bytes.len() - 4], MAX_DECOMPRESSED).is_err());
        }
        let (private, _) = fixture(true, true);
        assert!(matches!(
            inspect_with_budget(private.as_slice(), MAX_DECOMPRESSED),
            Err(ErrorCode::InvalidMessage)
        ));
    }

    #[test]
    fn encrypted_accessory_wrappers_preserve_the_portable_source_boundary() {
        for (marker, manifest, field, accepted) in [
            ("ACCESSORY", "{}", "scripts", true),
            ("ACCESSORY", "{}", "sounds", true),
            ("ACCESSORY", "{}", "resources", true),
            ("AVATAR", "{}", "scripts", false),
            ("ACCESSORY", "not json", "scripts", false),
            ("ACCESSORY", "[]", "scripts", false),
            ("ACCESSORY", "{}", "models", false),
            ("ACCESSORY", "{}", "textures", false),
        ] {
            let (bytes, _) = fixture(true, false);
            let mut nbt = Vec::new();
            GzDecoder::new(bytes.as_slice())
                .read_to_end(&mut nbt)
                .unwrap();
            let needle = b"ysm-native";
            let offset = nbt
                .windows(needle.len())
                .position(|bytes| bytes == needle)
                .unwrap()
                + needle.len();
            let mut metadata = Vec::new();
            string_tag(&mut metadata, "resource_kind", marker);
            string_tag(&mut metadata, "cosmetic_json", manifest);
            nbt.splice(offset..offset, metadata);
            nbt.pop();
            nbt.push(10);
            text(&mut nbt, field);
            nbt.extend_from_slice(&[0, 0]);
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&nbt).unwrap();
            let result = inspect_with_budget(encoder.finish().unwrap().as_slice(), MAX_DECOMPRESSED);
            assert_eq!(result.is_ok(), accepted, "{marker}/{manifest}/{field}");
        }
    }

}

struct Scanner<R> {
    source: R,
    remaining: u64,
    started: Instant,
    nodes: usize,
    max_nodes: Option<u64>,
    max_list_entries: Option<u64>,
    max_depth: usize,
    inspection_timeout: Duration,
    max_resource: u64,
    max_ciphertext: u64,
    format: String,
    content_hash: Option<Vec<u8>>,
    ciphertext_hash: Option<Vec<u8>>,
    encrypted: bool,
    encrypted_forbidden_field: bool,
    encrypted_wrapper_field: bool,
    accessory: bool,
    cosmetic_manifest: bool,
    nbt_hash: ring::digest::Context,
    internal_name: Option<String>,
    authors_text: String,
}

pub fn inspect(path: &Path) -> Result<PackageIdentity> {
    inspect_with_limits(
        File::open(path).map_err(|_| ErrorCode::Internal)?,
        PackageLimits::trusted(),
    )
}

/// Upload quotas come from the live configuration. Accepted blobs are read with
/// the more permissive `trusted` limits so a policy change cannot hide old data.
pub fn inspect_upload(path: &Path, limits: PackageLimits) -> Result<PackageIdentity> {
    inspect_with_limits(
        File::open(path).map_err(|_| ErrorCode::Internal)?,
        limits,
    )
}

#[cfg(test)]
fn inspect_with_budget(source: impl Read, budget: u64) -> Result<PackageIdentity> {
    inspect_with_limits(
        source,
        PackageLimits {
            max_nodes: None,
            max_decompressed: budget,
            ..PackageLimits::default()
        },
    )
}

fn inspect_with_limits(source: impl Read, limits: PackageLimits) -> Result<PackageIdentity> {
    let mut scanner = Scanner {
        source: GzDecoder::new(source),
        remaining: limits.max_decompressed,
        started: Instant::now(),
        nodes: 0,
        max_nodes: limits.max_nodes,
        max_list_entries: limits.max_list_entries,
        max_depth: limits.max_depth,
        inspection_timeout: limits.inspection_timeout,
        max_resource: limits.max_resource,
        max_ciphertext: limits.max_ciphertext,
        format: String::new(),
        content_hash: None,
        ciphertext_hash: None,
        encrypted: false,
        encrypted_forbidden_field: false,
        encrypted_wrapper_field: false,
        accessory: false,
        cosmetic_manifest: false,
        nbt_hash: ring::digest::Context::new(&ring::digest::SHA256),
        internal_name: None,
        authors_text: String::new(),
    };
    if scanner.byte()? != 10 {
        return Err(ErrorCode::InvalidMessage);
    }
    scanner.string()?; // The compressed NBT root is named in the existing wire format.
    scanner.value(10, "", 0)?;
    let mut trailing = [0];
    if scanner
        .source
        .read(&mut trailing)
        .map_err(|_| ErrorCode::InvalidMessage)?
        != 0
    {
        return Err(ErrorCode::InvalidMessage);
    }
    let model_type = if scanner.format == "ysm-native" {
        if scanner.content_hash.is_none() {
            return Err(ErrorCode::InvalidMessage);
        }
        if scanner.encrypted {
            if scanner.encrypted_forbidden_field
                || (scanner.encrypted_wrapper_field && !(scanner.accessory && scanner.cosmetic_manifest))
                || scanner.ciphertext_hash.is_none()
                || scanner.ciphertext_hash != scanner.content_hash
            {
                return Err(ErrorCode::InvalidMessage);
            }
            ModelType::ModelYsmEncrypted
        } else {
            ModelType::ModelYsmPlain
        }
    } else {
        if scanner.encrypted {
            return Err(ErrorCode::InvalidMessage);
        }
        ModelType::ModelFigura
    };
    let authors = scanner
        .authors_text
        .split('\n')
        .map(str::trim)
        .filter(|author| !author.is_empty())
        .fold(Vec::new(), |mut authors, author| {
            if !authors.iter().any(|existing| existing == author) {
                authors.push(author.to_owned());
            }
            authors
        });
    Ok(PackageIdentity {
        model_type,
        content_hash: scanner.content_hash,
        nbt_sha256: scanner.nbt_hash.finish().as_ref().to_vec(),
        ciphertext_sha256: scanner.ciphertext_hash,
        internal_name: scanner.internal_name,
        authors,
        authors_text: scanner.authors_text,
    })
}

impl<R: Read> Scanner<R> {
    fn limit(&self, reason: &'static str) -> ErrorCode {
        tracing::warn!(
            limit = reason,
            nodes = self.nodes,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "Model package inspection limit exceeded"
        );
        ErrorCode::LimitExceeded
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<()> {
        if bytes.len() as u64 > self.remaining {
            return Err(self.limit("decompressed_bytes"));
        }
        if self.started.elapsed() > self.inspection_timeout {
            return Err(self.limit("inspection_time"));
        }
        self.remaining -= bytes.len() as u64;
        self.source
            .read_exact(bytes)
            .map_err(|_| ErrorCode::InvalidMessage)?;
        self.nbt_hash.update(bytes);
        Ok(())
    }

    fn byte(&mut self) -> Result<u8> {
        let mut bytes = [0];
        self.read(&mut bytes)?;
        Ok(bytes[0])
    }

    fn length(&mut self) -> Result<u64> {
        let mut bytes = [0; 4];
        self.read(&mut bytes)?;
        let length = i32::from_be_bytes(bytes);
        u64::try_from(length).map_err(|_| ErrorCode::InvalidMessage)
    }

    fn string(&mut self) -> Result<String> {
        let mut length = [0; 2];
        self.read(&mut length)?;
        let mut bytes = vec![0; u16::from_be_bytes(length) as usize];
        self.read(&mut bytes)?;
        modified_utf8(&bytes)
    }

    fn skip(&mut self, mut length: u64, hash: bool) -> Result<Option<Vec<u8>>> {
        if length > self.remaining {
            return Err(self.limit("decompressed_bytes"));
        }
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 8192];
        let mut first = true;
        while length != 0 {
            let size = length.min(buffer.len() as u64) as usize;
            self.read(&mut buffer[..size])?;
            if hash {
                if first {
                    let bytes = &buffer[..size];
                    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
                    if !bytes.starts_with(b"YSGP") {
                        return Err(ErrorCode::InvalidMessage);
                    }
                }
                digest.update(&buffer[..size]);
            }
            first = false;
            length -= size as u64;
        }
        Ok(hash.then(|| digest.finish().as_ref().to_vec()))
    }

    fn value(&mut self, kind: u8, path: &str, depth: usize) -> Result<()> {
        self.nodes += 1;
        if depth > self.max_depth {
            return Err(self.limit("nesting_depth"));
        }
        if self
            .max_nodes
            .is_some_and(|limit| self.nodes as u64 > limit)
        {
            return Err(self.limit("node_count"));
        }
        match kind {
            1 => {
                self.skip(1, false)?;
            }
            2 => {
                self.skip(2, false)?;
            }
            3 if path == "ysm/crypto_version" => {
                let mut bytes = [0; 4];
                self.read(&mut bytes)?;
                let version = i32::from_be_bytes(bytes);
                if !(0..=3).contains(&version) {
                    return Err(ErrorCode::UnsupportedVersion);
                }
                self.encrypted |= version > 0;
            }
            3 | 5 => {
                self.skip(4, false)?;
            }
            4 | 6 => {
                self.skip(8, false)?;
            }
            7 | 11 | 12 => {
                let count = self.length()?;
                let width = match kind {
                    11 => 4,
                    12 => 8,
                    _ => 1,
                };
                let length = count.checked_mul(width).ok_or(ErrorCode::LimitExceeded)?;
                let ciphertext = path == "ysm/source_ciphertext";
                if length
                    > if ciphertext {
                        self.max_ciphertext
                    } else {
                        self.max_resource
                    }
                {
                    return Err(self.limit("resource_bytes"));
                }
                if ciphertext {
                    if kind != 7 || length == 0 {
                        return Err(ErrorCode::InvalidMessage);
                    }
                    self.encrypted = true;
                    self.ciphertext_hash = self.skip(length, true)?;
                } else {
                    self.skip(length, false)?;
                }
            }
            8 => {
                let value = self.string()?;
                match path {
                    "metadata/name" => self.internal_name = Some(value),
                    "metadata/authors" => {
                        // Figura flattens author entries with newlines. An entry may itself
                        // contain newlines, so splitting cannot recover the original array.
                        self.authors_text = value;
                    }
                    "metadata/format" => self.format = value,
                    "metadata/resource_kind" => self.accessory = value == "ACCESSORY",
                    "metadata/cosmetic_json" => {
                        self.cosmetic_manifest = serde_json::from_str::<serde_json::Value>(&value)
                            .is_ok_and(|value| value.is_object());
                    }
                    "ysm/container_kind" if value == "YSGP" => self.encrypted = true,
                    "ysm/content_sha256" => {
                        let mut hash = vec![0; 32];
                        faster_hex::hex_decode(value.as_bytes(), &mut hash)
                            .map_err(|_| ErrorCode::InvalidMessage)?;
                        self.content_hash = Some(hash);
                    }
                    _ => {}
                }
            }
            9 => {
                let element = self.byte()?;
                let length = self.length()?;
                if self
                    .max_list_entries
                    .is_some_and(|limit| length > limit)
                    || (length > 0 && element == 0)
                {
                    return Err(self.limit("list_entries"));
                }
                for _ in 0..length {
                    self.value(element, path, depth + 1)?;
                }
            }
            10 => {
                let mut names = HashSet::new();
                loop {
                    let child = self.byte()?;
                    if child == 0 {
                        break;
                    }
                    let name = self.string()?;
                    if !names.insert(name.clone()) {
                        return Err(ErrorCode::InvalidMessage);
                    }
                    let wrapper = match path {
                        "" => matches!(name.as_str(), "scripts" | "sounds" | "resources"),
                        "metadata" => matches!(name.as_str(), "resource_kind" | "cosmetic_json"),
                        _ => false,
                    };
                    self.encrypted_wrapper_field |= wrapper;
                    let allowed = wrapper || match path {
                        "" => matches!(name.as_str(), "metadata" | "ysm"),
                        "metadata" => matches!(
                            name.as_str(),
                            "name"
                                | "authors"
                                | "ver"
                                | "uuid"
                                | "format"
                                | "ysm_kind"
                                | "ysm_texture"
                                | "color"
                        ),
                        "ysm" => matches!(
                            name.as_str(),
                            "source_ciphertext"
                                | "content_sha256"
                                | "container_kind"
                                | "engine_schema_version"
                                | "crypto_version"
                                | "binary_format_version"
                                | "kind"
                                | "texture_id"
                                | "display_name"
                        ),
                        _ => true,
                    };
                    self.encrypted_forbidden_field |= !allowed;
                    let name = name.replace('/', "\0");
                    let child_path = if path.is_empty() {
                        name
                    } else {
                        format!("{path}/{name}")
                    };
                    self.value(child, &child_path, depth + 1)?;
                }
            }
            _ => return Err(ErrorCode::InvalidMessage),
        }
        Ok(())
    }
}
