-- Only an explicit owner binding makes a native revision outfit-private.
-- Automatic legacy normalization must not reclassify historical public resources.
-- No native-row FK: manifest publication rewrites those rows within its transaction.
CREATE TABLE outfit_private_sources (
    owner_uuid TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    asset_revision INTEGER NOT NULL CHECK(asset_revision > 0),
    PRIMARY KEY(owner_uuid, asset_id, asset_revision)
);
