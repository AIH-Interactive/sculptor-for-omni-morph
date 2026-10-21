-- Resource purpose is independent of the unchanged model format enumeration.
-- Side tables avoid altering the published v1 resource/manifest identities.
CREATE TABLE IF NOT EXISTS library_resource_kinds (
    model_id TEXT PRIMARY KEY REFERENCES library_models(model_id),
    resource_kind TEXT NOT NULL CHECK(resource_kind IN ('AVATAR','ACCESSORY','OUTFIT'))
);
CREATE TABLE IF NOT EXISTS library_accessory_metadata (
    model_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    metadata_json TEXT NOT NULL CHECK(length(CAST(metadata_json AS BLOB)) <= 16384),
    PRIMARY KEY(model_id, revision),
    FOREIGN KEY(model_id, revision) REFERENCES library_revisions(model_id, revision)
);
CREATE TABLE IF NOT EXISTS outfit_configurations (
    owner_uuid TEXT PRIMARY KEY,
    revision INTEGER NOT NULL CHECK(revision >= 0),
    configuration_json TEXT NOT NULL CHECK(length(CAST(configuration_json AS BLOB)) <= 65536)
);
CREATE TABLE IF NOT EXISTS outfit_presets (
    owner_uuid TEXT NOT NULL,
    preset_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    name TEXT NOT NULL,
    configuration_json TEXT NOT NULL CHECK(length(CAST(configuration_json AS BLOB)) <= 65536),
    PRIMARY KEY(owner_uuid, preset_id)
);
CREATE TABLE IF NOT EXISTS outfit_preset_dependencies (
    owner_uuid TEXT NOT NULL,
    preset_id TEXT NOT NULL,
    dependency_index INTEGER NOT NULL,
    reference_json TEXT NOT NULL,
    PRIMARY KEY(owner_uuid, preset_id, dependency_index),
    FOREIGN KEY(owner_uuid, preset_id) REFERENCES outfit_presets(owner_uuid, preset_id) ON DELETE CASCADE
);
-- Published presets share library identity, images, policy and collection membership.
-- They have no model bytes, model format, or library_revisions row.
CREATE TABLE IF NOT EXISTS library_outfit_contents (
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    configuration_json TEXT NOT NULL CHECK(length(CAST(configuration_json AS BLOB)) <= 65536),
    PRIMARY KEY(model_id, revision)
);
