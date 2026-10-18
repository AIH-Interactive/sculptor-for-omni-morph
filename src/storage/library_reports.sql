-- Reports are private moderation records, independent of ordinary catalog responses.
CREATE TABLE library_reports (
    report_id TEXT PRIMARY KEY,
    model_id TEXT NOT NULL REFERENCES library_models(model_id),
    reporter_uuid TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 512),
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT,
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0)
);
CREATE UNIQUE INDEX library_reports_one_open
    ON library_reports(model_id, reporter_uuid) WHERE resolved_at IS NULL;
CREATE INDEX library_reports_queue ON library_reports(resolved_at, created_at, report_id);
