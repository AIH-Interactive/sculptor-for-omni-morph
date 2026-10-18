use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportInput {
    pub operation_id: String,
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportQuery {
    #[serde(default = "first_page")]
    pub page: u32,
    #[serde(default)]
    pub resolved: bool,
}

fn first_page() -> u32 {
    1
}

impl Storage {
    pub fn library_report(&self, actor: Actor, id: &str, input: &ReportInput) -> Result<Value> {
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let reason = input.reason.trim();
        text(reason, 512, false)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["report", id, reason])).map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            // Replays must not reveal a model that has become private or blocked.
            let item = model(transaction, id, actor, false)?;
            if item.policy.moderation != Moderation::Active {
                return Err(ErrorCode::Forbidden);
            }
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let existing: Option<String> = transaction.query_row(
                    "SELECT report_id FROM library_reports WHERE model_id = ?1 AND reporter_uuid = ?2 AND resolved_at IS NULL",
                    params![id, actor.uuid.to_string()], |row| row.get(0),
                ).optional().map_err(sql_error)?;
                if existing.is_some() {
                    return Err(ErrorCode::Conflict);
                }
                let report = random_id();
                transaction.execute(
                    "INSERT INTO library_reports(report_id, model_id, reporter_uuid, reason, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![report, id, actor.uuid.to_string(), reason, library::sql_integer(now_seconds())?],
                ).map_err(sql_error)?;
                serde_json::to_vec(&json!({"report_id": report, "submitted": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_reports(&self, actor: Actor, query: &ReportQuery) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if query.page == 0 || query.page > 1_000_000 {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_read(|database| {
            let total: i64 = database.query_row(
                "SELECT count(*) FROM library_reports WHERE (resolved_at IS NOT NULL) = ?1",
                [query.resolved], |row| row.get(0),
            ).map_err(sql_error)?;
            let mut statement = database.prepare(
                "SELECT r.report_id, r.model_id, m.display_name, r.reporter_uuid, r.reason,
                 r.created_at, r.resolved_at, r.resolved_by, r.revision FROM library_reports r
                 JOIN library_models m ON m.model_id = r.model_id
                 WHERE (r.resolved_at IS NOT NULL) = ?1 ORDER BY r.created_at, r.report_id LIMIT 8 OFFSET ?2",
            ).map_err(sql_error)?;
            let rows = statement.query_map(params![query.resolved, i64::from(query.page - 1) * 8], |row| {
                Ok(json!({"report_id": row.get::<_, String>(0)?, "model_id": row.get::<_, String>(1)?,
                    "model_name": row.get::<_, String>(2)?, "reporter_uuid": row.get::<_, String>(3)?,
                    "reason": row.get::<_, String>(4)?, "created_at": row.get::<_, i64>(5)?,
                    "resolved_at": row.get::<_, Option<i64>>(6)?, "resolved_by": row.get::<_, Option<String>>(7)?,
                    "revision": row.get::<_, i64>(8)?}))
            }).map_err(sql_error)?;
            let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>().map_err(sql_error)?;
            Ok(json!({"items": items, "total": total, "page": query.page, "page_size": 8,
                "has_next": i64::from(query.page) * 8 < total}))
        })
    }

    pub fn library_resolve_report(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        super::super::resource::operation_id(id)?;
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["resolve_report", id, input.expected_revision]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let changed = transaction.execute(
                    "UPDATE library_reports SET resolved_at = ?1, resolved_by = ?2, revision = revision + 1
                     WHERE report_id = ?3 AND revision = ?4 AND resolved_at IS NULL",
                    params![library::sql_integer(now_seconds())?, actor.uuid.to_string(), id,
                        library::sql_integer(input.expected_revision)?],
                ).map_err(sql_error)?;
                if changed != 1 {
                    return Err(ErrorCode::Conflict);
                }
                serde_json::to_vec(&json!({"report_id": id, "resolved": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }
}
