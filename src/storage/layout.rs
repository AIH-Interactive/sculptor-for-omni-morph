use super::{sqlite::sql_error, ErrorCode, Result};
use rusqlite::OptionalExtension;
use std::{fs, io::Read, path::Path};

/// Copy legacy data before publishing the relocated database. Existing canonical files win.
pub(super) fn migrate(avatars: &Path, database: &Path) -> Result<()> {
    if database.is_file() {
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(sql_error)?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        if version > super::library::SCHEMA_VERSION {
            return Err(ErrorCode::UnsupportedVersion);
        }
        if version >= 1 {
            let completed: Option<String> = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'avatars_layout'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            if completed.as_deref() == Some("models-upload-v1") {
                return Ok(());
            }
        }
    }
    let legacy = avatars.join("cloud-v1");
    let models = avatars.join("models");
    let uploads = avatars.join("upload");
    fs::create_dir_all(&models).map_err(|_| ErrorCode::Internal)?;
    fs::create_dir_all(&uploads).map_err(|_| ErrorCode::Internal)?;
    copy_models(&legacy.join("blobs"), &models, true)?;
    copy_models(avatars, &models, false)?;
    copy_models(&avatars.join("temp"), &uploads, false)?;

    // Prefer the previous canonical database over an older cloud-v1 copy.
    // Explicit existing destinations remain authoritative on every later start.
    let old_database = [
        database.with_extension("sqlite3"),
        legacy.join("metadata.db"),
        legacy.join("metadata.sqlite3"),
    ]
    .into_iter()
    .find(|path| path != database && path.is_file());
    if let Some(old_database) = old_database.filter(|_| !database.exists()) {
        let parent = database.parent().ok_or(ErrorCode::Internal)?;
        fs::create_dir_all(parent).map_err(|_| ErrorCode::Internal)?;
        let source = rusqlite::Connection::open_with_flags(
            old_database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(sql_error)?;
        let version: u32 = source
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        if version > super::library::SCHEMA_VERSION {
            return Err(ErrorCode::UnsupportedVersion);
        }
        let staged = database.with_extension(format!(
            "{}.migration.db",
            faster_hex::hex_string(&crate::utils::rand()[..8])
        ));
        // SQLite's backup reads a consistent snapshot, including committed WAL pages.
        if let Err(error) = source.backup("main", &staged, None) {
            let _ = fs::remove_file(&staged);
            return Err(sql_error(error));
        }
        fs::rename(&staged, database).map_err(|_| ErrorCode::Internal)?;
    }
    Ok(())
}

fn copy_models(source: &Path, destination: &Path, hashes: bool) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(source).map_err(|_| ErrorCode::Internal)? {
        let entry = entry.map_err(|_| ErrorCode::Internal)?;
        if !entry
            .file_type()
            .map_err(|_| ErrorCode::Internal)?
            .is_file()
        {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let accepted = if hashes {
            name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        } else {
            name.strip_suffix(".moon")
                .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok())
        };
        if !accepted {
            continue;
        }
        let target = destination.join(name.as_ref());
        if target.exists() {
            if hashes && digest(&target)? != name.to_ascii_lowercase() {
                return Err(ErrorCode::Internal);
            }
            continue;
        }
        let staged = destination.join(format!(
            ".migrate-{}-{}.part",
            name,
            faster_hex::hex_string(&crate::utils::rand()[..8])
        ));
        let mut input = fs::File::open(entry.path()).map_err(|_| ErrorCode::Internal)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|_| ErrorCode::Internal)?;
        std::io::copy(&mut input, &mut output).map_err(|_| ErrorCode::Internal)?;
        output.sync_all().map_err(|_| ErrorCode::Internal)?;
        drop(output);
        if hashes && digest(&staged)? != name.to_ascii_lowercase() {
            let _ = fs::remove_file(&staged);
            return Err(ErrorCode::Internal);
        }
        fs::rename(staged, target).map_err(|_| ErrorCode::Internal)?;
    }
    Ok(())
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|_| ErrorCode::Internal)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut bytes = [0; 8192];
    loop {
        let count = file.read(&mut bytes).map_err(|_| ErrorCode::Internal)?;
        if count == 0 {
            return Ok(faster_hex::hex_string(hash.finish().as_ref()));
        }
        hash.update(&bytes[..count]);
    }
}
