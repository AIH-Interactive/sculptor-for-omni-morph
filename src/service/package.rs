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

const MAX_DECOMPRESSED: u64 = 256 * 1024 * 1024;
const MAX_RESOURCE: u64 = 32 * 1024 * 1024;
const MAX_CIPHERTEXT: u64 = 64 * 1024 * 1024;

type Result<T> = std::result::Result<T, ErrorCode>;

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
        assert!(inspect_with_limits(bytes.as_slice(), MAX_DECOMPRESSED, None).is_ok());
        assert!(matches!(
            inspect_with_limits(bytes.as_slice(), MAX_DECOMPRESSED, Some(262_144)),
            Err(ErrorCode::LimitExceeded)
        ));
        assert!(matches!(
            inspect_with_limits(bytes.as_slice(), 8, None),
            Err(ErrorCode::LimitExceeded)
        ));
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
}

struct Scanner<R> {
    source: R,
    remaining: u64,
    started: Instant,
    nodes: usize,
    max_nodes: Option<u64>,
    format: String,
    content_hash: Option<Vec<u8>>,
    ciphertext_hash: Option<Vec<u8>>,
    encrypted: bool,
    encrypted_forbidden_field: bool,
    nbt_hash: ring::digest::Context,
    internal_name: Option<String>,
    authors_text: String,
}

pub fn inspect(path: &Path) -> Result<PackageIdentity> {
    inspect_with_budget(
        File::open(path).map_err(|_| ErrorCode::Internal)?,
        MAX_DECOMPRESSED,
    )
}

/// Node quotas apply when admitting uploads. Reading accepted blobs must not
/// silently impose the former fixed quota after an administrator disables it.
pub fn inspect_upload(path: &Path, max_nodes: i64) -> Result<PackageIdentity> {
    let max_nodes = match max_nodes {
        -1 => None,
        value if value >= 0 => Some(value as u64),
        _ => return Err(ErrorCode::InvalidMessage),
    };
    inspect_with_limits(
        File::open(path).map_err(|_| ErrorCode::Internal)?,
        MAX_DECOMPRESSED,
        max_nodes,
    )
}

fn inspect_with_budget(source: impl Read, budget: u64) -> Result<PackageIdentity> {
    inspect_with_limits(source, budget, None)
}

fn inspect_with_limits(
    source: impl Read,
    budget: u64,
    max_nodes: Option<u64>,
) -> Result<PackageIdentity> {
    let mut scanner = Scanner {
        source: GzDecoder::new(source),
        remaining: budget,
        started: Instant::now(),
        nodes: 0,
        max_nodes,
        format: String::new(),
        content_hash: None,
        ciphertext_hash: None,
        encrypted: false,
        encrypted_forbidden_field: false,
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
        if self.started.elapsed() > Duration::from_secs(10) {
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
        if depth > 64 {
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
                        MAX_CIPHERTEXT
                    } else {
                        MAX_RESOURCE
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
                if length > 4096 || (length > 0 && element == 0) {
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
                    let allowed = match path {
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
