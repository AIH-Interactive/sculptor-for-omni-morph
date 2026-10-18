CREATE TABLE library_appeals (
    appeal_id TEXT PRIMARY KEY,
    target_kind TEXT NOT NULL CHECK(target_kind IN ('LIBRARY','WARDROBE')),
    target_id TEXT NOT NULL,
    target_revision INTEGER NOT NULL,
    uploader_uuid TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 512),
    created_at INTEGER NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    resolved_at INTEGER,
    resolved_by TEXT,
    response TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX library_appeals_one_open ON library_appeals(target_kind, target_id, uploader_uuid)
    WHERE resolved_at IS NULL;
CREATE INDEX library_appeals_queue ON library_appeals(resolved_at, created_at, appeal_id);
