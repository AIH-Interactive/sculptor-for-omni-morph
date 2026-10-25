-- The cover-image policy is configurable at runtime. The table stores a generous
-- absolute ceiling; the service enforces the configured value on every write.
ALTER TABLE library_images RENAME TO library_images_legacy;
CREATE TABLE library_images (
    model_id TEXT PRIMARY KEY REFERENCES library_models(model_id),
    image_revision INTEGER NOT NULL CHECK(image_revision > 0),
    hash TEXT NOT NULL CHECK(length(hash) = 64),
    media_type TEXT NOT NULL CHECK(media_type IN ('image/png','image/jpeg')),
    width INTEGER NOT NULL CHECK(width BETWEEN 1 AND 8192),
    height INTEGER NOT NULL CHECK(height BETWEEN 1 AND 8192),
    byte_length INTEGER NOT NULL CHECK(byte_length BETWEEN 1 AND 67108864)
);
INSERT INTO library_images SELECT * FROM library_images_legacy;
DROP TABLE library_images_legacy;
