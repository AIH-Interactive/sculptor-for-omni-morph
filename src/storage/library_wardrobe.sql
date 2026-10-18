CREATE TABLE library_wardrobe_blocks (
    operation_id TEXT NOT NULL,
    hash_kind TEXT NOT NULL CHECK(hash_kind IN ('RAW_SHA256','NBT_SHA256','YSM_CIPHERTEXT_SHA256','LEGACY_FIGURA_BASE64_SHA256')),
    verified_hash TEXT NOT NULL CHECK(length(verified_hash) = 64),
    owner_uuid TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    asset_revision INTEGER NOT NULL,
    actor_uuid TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
    PRIMARY KEY(operation_id, hash_kind, verified_hash)
);
CREATE INDEX library_wardrobe_blocked_identity ON library_wardrobe_blocks(hash_kind, verified_hash, active);
-- An absent wardrobe setting means public, independently of library-private equipment intent.
CREATE TABLE library_wardrobe_settings (
    owner_uuid TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    asset_revision INTEGER NOT NULL,
    show_equipment INTEGER NOT NULL CHECK(show_equipment IN (0,1)),
    PRIMARY KEY(owner_uuid, asset_id, asset_revision)
);
CREATE VIEW library_active_blocked_hashes AS
SELECT hash_kind, verified_hash, active FROM library_blocked_hashes
UNION ALL SELECT hash_kind, verified_hash, active FROM library_wardrobe_blocks;

DROP VIEW library_effective_models;
DROP VIEW library_hash_blocked_models;
CREATE VIEW library_hash_blocked_models AS
SELECT DISTINCT r.model_id
FROM library_revision_hashes r JOIN library_active_blocked_hashes b
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
