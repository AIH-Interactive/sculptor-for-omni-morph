-- Per-equipment intent is independent of the source model's catalog visibility.
-- Absence of a row means false for existing bindings and reserved source assets.
CREATE TABLE library_equipment_settings (
    resource_owner TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    show_private_equipment INTEGER NOT NULL DEFAULT 0
        CHECK(show_private_equipment IN (0, 1)),
    PRIMARY KEY(resource_owner, asset_id, revision),
    FOREIGN KEY(resource_owner, asset_id, revision)
        REFERENCES library_resource_bindings(resource_owner, asset_id, revision)
);

-- Immutable blobs can be shared across players and transport wrappers. Only
-- server-computed identities are stored here; no manifest-row foreign keys.
CREATE TABLE library_verified_blobs (
    raw_sha256 TEXT NOT NULL CHECK(length(raw_sha256) = 64),
    hash_kind TEXT NOT NULL CHECK(hash_kind IN (
        'RAW_SHA256', 'NBT_SHA256', 'YSM_CIPHERTEXT_SHA256', 'LEGACY_FIGURA_BASE64_SHA256'
    )),
    verified_hash TEXT NOT NULL CHECK(length(verified_hash) = 64),
    PRIMARY KEY(raw_sha256, hash_kind, verified_hash)
);
CREATE INDEX library_verified_identity ON library_verified_blobs(hash_kind, verified_hash, raw_sha256);

-- One active hash ban applies to every existing copy, independent of which
-- catalog record the moderator acted on. The underlying visibility is also
-- forced private during moderation so removing a ban never republishes copies.
CREATE VIEW library_hash_blocked_models AS
SELECT DISTINCT r.model_id
FROM library_revision_hashes r JOIN library_blocked_hashes b
    ON b.hash_kind = r.hash_kind AND b.verified_hash = r.verified_hash
WHERE b.active = 1;

CREATE VIEW library_effective_models AS
SELECT m.model_id, m.uploader_uuid, m.uploader_name_snapshot, m.display_name,
    m.internal_name, m.authors_json, m.model_type, m.category_id, m.description,
    CASE WHEN h.model_id IS NOT NULL THEN 'PRIVATE' ELSE m.visibility END AS visibility,
    m.requires_authorization,
    CASE WHEN m.moderation_status = 'DELETED' THEN 'DELETED'
         WHEN h.model_id IS NOT NULL THEN 'BLOCKED'
         ELSE m.moderation_status END AS moderation_status,
    m.current_revision, m.policy_revision, m.created_at, m.updated_at, m.published
FROM library_models m LEFT JOIN library_hash_blocked_models h ON h.model_id = m.model_id;
