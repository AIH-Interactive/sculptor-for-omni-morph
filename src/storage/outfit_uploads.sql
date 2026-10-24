-- Durable private intent exists before a native upload grant is issued.
-- Its asset identity stays reserved across retries and process restarts.
CREATE TABLE outfit_private_uploads (
    owner_uuid TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    input_json TEXT NOT NULL CHECK(length(CAST(input_json AS BLOB)) <= 32768),
    PRIMARY KEY(owner_uuid, operation_id),
    UNIQUE(owner_uuid, asset_id)
);
