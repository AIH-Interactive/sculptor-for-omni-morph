-- Receipts refer to either immutable model bytes or immutable outfit JSON.
-- Preserve the original columns, counters and deduplication key.
CREATE TABLE library_download_receipts_new (
    transfer_id TEXT PRIMARY KEY,
    player_uuid TEXT NOT NULL,
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    revision INTEGER NOT NULL,
    purpose TEXT NOT NULL CHECK(purpose IN ('preview','download')),
    policy_revision INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    bytes_sent INTEGER NOT NULL DEFAULT 0 CHECK(bytes_sent IN (0,1)),
    completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0,1)),
    counted_day INTEGER
);
CREATE TRIGGER library_receipt_revision_insert BEFORE INSERT ON library_download_receipts_new
WHEN NOT EXISTS(SELECT 1 FROM library_revisions WHERE model_id = NEW.model_id AND revision = NEW.revision)
 AND NOT EXISTS(SELECT 1 FROM library_outfit_contents WHERE model_id = NEW.model_id AND revision = NEW.revision)
BEGIN
    SELECT RAISE(ABORT, 'receipt resource revision missing');
END;
CREATE TRIGGER library_receipt_revision_update BEFORE UPDATE OF model_id, revision ON library_download_receipts_new
WHEN NOT EXISTS(SELECT 1 FROM library_revisions WHERE model_id = NEW.model_id AND revision = NEW.revision)
 AND NOT EXISTS(SELECT 1 FROM library_outfit_contents WHERE model_id = NEW.model_id AND revision = NEW.revision)
BEGIN
    SELECT RAISE(ABORT, 'receipt resource revision missing');
END;
INSERT INTO library_download_receipts_new SELECT * FROM library_download_receipts;
DROP TABLE library_download_receipts;
ALTER TABLE library_download_receipts_new RENAME TO library_download_receipts;
CREATE UNIQUE INDEX library_download_dedup ON library_download_receipts(player_uuid, model_id, revision, counted_day)
    WHERE counted_day IS NOT NULL;

-- Retain the former composite-FK restriction when either content table changes.
CREATE TRIGGER library_model_receipt_delete BEFORE DELETE ON library_revisions
WHEN EXISTS(SELECT 1 FROM library_download_receipts WHERE model_id = OLD.model_id AND revision = OLD.revision)
BEGIN
    SELECT RAISE(ABORT, 'model revision has receipts');
END;
CREATE TRIGGER library_model_receipt_update BEFORE UPDATE OF model_id, revision ON library_revisions
WHEN EXISTS(SELECT 1 FROM library_download_receipts WHERE model_id = OLD.model_id AND revision = OLD.revision)
 AND (NEW.model_id != OLD.model_id OR NEW.revision != OLD.revision)
BEGIN
    SELECT RAISE(ABORT, 'model revision has receipts');
END;
CREATE TRIGGER library_outfit_receipt_delete BEFORE DELETE ON library_outfit_contents
WHEN EXISTS(SELECT 1 FROM library_download_receipts WHERE model_id = OLD.model_id AND revision = OLD.revision)
BEGIN
    SELECT RAISE(ABORT, 'outfit revision has receipts');
END;
CREATE TRIGGER library_outfit_receipt_update BEFORE UPDATE OF model_id, revision ON library_outfit_contents
WHEN EXISTS(SELECT 1 FROM library_download_receipts WHERE model_id = OLD.model_id AND revision = OLD.revision)
 AND (NEW.model_id != OLD.model_id OR NEW.revision != OLD.revision)
BEGIN
    SELECT RAISE(ABORT, 'outfit revision has receipts');
END;
