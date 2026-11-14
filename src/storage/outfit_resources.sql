-- Local provenance and the verified distribution body are distinct identities.
-- Like library_resource_bindings, this side table survives manifest row rewrites.
-- Every use must recheck the native revision, tombstone and current moderation.
CREATE TABLE outfit_local_resources (
    owner_uuid TEXT NOT NULL,
    local_id TEXT NOT NULL,
    source_hash TEXT NOT NULL CHECK(length(source_hash) = 64),
    model_type TEXT NOT NULL CHECK(model_type IN ('FIGURA','YSM_PLAIN','YSM_ENCRYPTED')),
    binding_json TEXT NOT NULL CHECK(length(CAST(binding_json AS BLOB)) <= 32768),
    PRIMARY KEY(owner_uuid, local_id, source_hash, model_type)
);
