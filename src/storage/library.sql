-- Library keys intentionally do not reference manifest-rewritten tables.
CREATE TABLE library_models (
    model_id TEXT PRIMARY KEY,
    uploader_uuid TEXT NOT NULL,
    uploader_name_snapshot TEXT NOT NULL,
    display_name TEXT NOT NULL,
    internal_name TEXT NOT NULL,
    authors_json TEXT NOT NULL,
    model_type TEXT NOT NULL CHECK(model_type IN ('FIGURA','YSM_PLAIN','YSM_ENCRYPTED')),
    category_id TEXT NOT NULL,
    description TEXT NOT NULL,
    visibility TEXT NOT NULL CHECK(visibility IN ('PUBLIC','PRIVATE')),
    requires_authorization INTEGER NOT NULL CHECK(requires_authorization IN (0,1)),
    moderation_status TEXT NOT NULL CHECK(moderation_status IN ('ACTIVE','BLOCKED','DELETED')),
    current_revision INTEGER NOT NULL CHECK(current_revision > 0),
    policy_revision INTEGER NOT NULL CHECK(policy_revision > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    published INTEGER NOT NULL DEFAULT 0 CHECK(published IN (0,1)),
    CHECK(moderation_status != 'BLOCKED' OR visibility = 'PRIVATE')
);
CREATE INDEX library_public ON library_models(published, moderation_status, visibility, updated_at, model_id);
CREATE INDEX library_uploader ON library_models(uploader_uuid, moderation_status, updated_at, model_id);
CREATE INDEX library_category_type ON library_models(category_id, model_type, updated_at, model_id);
CREATE TABLE library_revisions (
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    resource_owner TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    resource_revision INTEGER NOT NULL CHECK(resource_revision > 0),
    raw_sha256 TEXT NOT NULL CHECK(length(raw_sha256) = 64),
    byte_length INTEGER NOT NULL CHECK(byte_length > 0),
    distribution_format TEXT NOT NULL,
    metadata_source TEXT NOT NULL,
    metadata_validation TEXT NOT NULL CHECK(metadata_validation IN ('SERVER_VERIFIED','CLIENT_EXTRACTED')),
    source_filename TEXT NOT NULL,
    PRIMARY KEY(model_id, revision)
);
CREATE TABLE library_revision_hashes (
    model_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    hash_kind TEXT NOT NULL CHECK(hash_kind IN ('RAW_SHA256','NBT_SHA256','YSM_CIPHERTEXT_SHA256','LEGACY_FIGURA_BASE64_SHA256')),
    verified_hash TEXT NOT NULL CHECK(length(verified_hash) = 64),
    PRIMARY KEY(model_id, revision, hash_kind, verified_hash),
    FOREIGN KEY(model_id, revision) REFERENCES library_revisions(model_id, revision)
);
CREATE INDEX library_identity ON library_revision_hashes(hash_kind, verified_hash);
CREATE TABLE library_tags (
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    normalized_tag TEXT NOT NULL,
    display_tag TEXT NOT NULL,
    PRIMARY KEY(model_id, normalized_tag)
);
CREATE INDEX library_tag_lookup ON library_tags(normalized_tag, model_id);
CREATE TABLE library_images (
    model_id TEXT PRIMARY KEY REFERENCES library_models(model_id),
    image_revision INTEGER NOT NULL CHECK(image_revision > 0),
    hash TEXT NOT NULL CHECK(length(hash) = 64),
    media_type TEXT NOT NULL CHECK(media_type IN ('image/png','image/jpeg')),
    width INTEGER NOT NULL CHECK(width BETWEEN 1 AND 2048),
    height INTEGER NOT NULL CHECK(height BETWEEN 1 AND 2048),
    byte_length INTEGER NOT NULL CHECK(byte_length BETWEEN 1 AND 2097152)
);
CREATE TABLE library_favorites (
    player_uuid TEXT NOT NULL,
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(player_uuid, model_id)
);
CREATE TABLE library_grants (
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    player_uuid TEXT NOT NULL,
    name_snapshot TEXT NOT NULL,
    granted_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(model_id, player_uuid)
);
CREATE INDEX library_grant_player ON library_grants(player_uuid, model_id);
CREATE TABLE library_stats (
    model_id TEXT PRIMARY KEY REFERENCES library_models(model_id),
    downloads INTEGER NOT NULL DEFAULT 0 CHECK(downloads >= 0)
);
CREATE TABLE library_download_receipts (
    transfer_id TEXT PRIMARY KEY,
    player_uuid TEXT NOT NULL,
    model_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    purpose TEXT NOT NULL CHECK(purpose IN ('preview','download')),
    policy_revision INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    bytes_sent INTEGER NOT NULL DEFAULT 0 CHECK(bytes_sent IN (0,1)),
    completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0,1)),
    counted_day INTEGER,
    FOREIGN KEY(model_id, revision) REFERENCES library_revisions(model_id, revision)
);
CREATE UNIQUE INDEX library_download_dedup ON library_download_receipts(player_uuid, model_id, revision, counted_day)
    WHERE counted_day IS NOT NULL;
CREATE TABLE library_resource_bindings (
    resource_owner TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    source_model_id TEXT NOT NULL,
    source_revision INTEGER NOT NULL,
    PRIMARY KEY(resource_owner, asset_id, revision),
    FOREIGN KEY(source_model_id, source_revision) REFERENCES library_revisions(model_id, revision)
);
CREATE INDEX library_binding_source ON library_resource_bindings(source_model_id, source_revision);
CREATE TABLE library_blocked_hashes (
    hash_kind TEXT NOT NULL CHECK(hash_kind IN ('RAW_SHA256','NBT_SHA256','YSM_CIPHERTEXT_SHA256','LEGACY_FIGURA_BASE64_SHA256')),
    verified_hash TEXT NOT NULL CHECK(length(verified_hash) = 64),
    blocking_model_id TEXT NOT NULL REFERENCES library_models(model_id),
    active INTEGER NOT NULL CHECK(active IN (0,1)),
    reason TEXT NOT NULL,
    actor_uuid TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(hash_kind, verified_hash, blocking_model_id)
);
CREATE INDEX library_blocked_identity ON library_blocked_hashes(hash_kind, verified_hash, active);
CREATE TABLE library_operations (
    actor_uuid TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    fingerprint BLOB NOT NULL,
    result BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY(actor_uuid, operation_id)
);
CREATE TABLE library_audit (
    id INTEGER PRIMARY KEY,
    actor_uuid TEXT NOT NULL,
    action TEXT NOT NULL,
    model_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    result TEXT NOT NULL
);
CREATE TABLE library_admin_sessions (
    token_hash BLOB PRIMARY KEY,
    refresh_hash BLOB UNIQUE,
    family_id TEXT NOT NULL,
    player_uuid TEXT NOT NULL,
    origin TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    config_revision BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    refresh_expires_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0 CHECK(revoked IN (0,1)),
    refresh_used INTEGER NOT NULL DEFAULT 0 CHECK(refresh_used IN (0,1))
);
CREATE INDEX library_admin_family ON library_admin_sessions(family_id);
