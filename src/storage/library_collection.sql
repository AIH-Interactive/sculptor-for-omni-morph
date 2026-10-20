CREATE TABLE library_collection (
    player_uuid TEXT NOT NULL,
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    joined_at INTEGER NOT NULL,
    join_source TEXT NOT NULL CHECK(join_source IN ('DOWNLOAD', 'UPLOAD')),
    in_outfit INTEGER NOT NULL DEFAULT 0 CHECK(in_outfit IN (0, 1)),
    PRIMARY KEY(player_uuid, model_id)
);
CREATE INDEX library_collection_page ON library_collection(player_uuid, joined_at, model_id);

-- Only successfully published, currently active submissions become initial members.
INSERT INTO library_collection(player_uuid, model_id, joined_at, join_source)
SELECT uploader_uuid, model_id, created_at, 'UPLOAD'
FROM library_effective_models
WHERE published = 1 AND moderation_status = 'ACTIVE';
