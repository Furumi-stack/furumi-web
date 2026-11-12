/// Job scheduler: models, migrations, Job trait, JobRegistry, and scheduler loop.
use std::collections::HashMap;
use std::sync::Arc;

use cot::db::migrations::{self, Field, Operation, SyncDynMigration};
use cot::db::{Auto, Database, DatabaseField, Identifier, LimitedString, Model};

use crate::config::AppConfig;

// ---------------------------------------------------------------------------
// ScheduledJob
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct ScheduledJob {
    #[model(primary_key)]
    pub name: String,
    pub description: String,
    pub cron_expression: LimitedString<100>,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
}

fn now_iso() -> LimitedString<32> {
    LimitedString::new(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap()
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }

    let suffix_budget = max_bytes - 3;
    let mut suffix = Vec::new();
    let mut suffix_len = 0;
    for ch in value.chars().rev() {
        let ch_len = ch.len_utf8();
        if suffix_len + ch_len > suffix_budget {
            break;
        }
        suffix.push(ch);
        suffix_len += ch_len;
    }

    let mut result = String::from("...");
    for ch in suffix.iter().rev() {
        result.push(*ch);
    }
    result
}

fn limited_string<const N: u32>(value: &str) -> LimitedString<N> {
    LimitedString::new(value).unwrap_or_else(|_| {
        let truncated = truncate_utf8_bytes(value, N as usize);
        LimitedString::new(&truncated).unwrap()
    })
}

impl ScheduledJob {
    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn get_by_name(db: &Database, name: &str) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, name.to_owned()).await
    }

    pub async fn upsert(
        db: &Database,
        name: &str,
        description: &str,
        cron_expression: &str,
    ) -> cot::db::Result<Self> {
        if let Some(mut existing) = Self::get_by_name(db, name).await? {
            // The registered schedule is a default for new jobs; retain admin overrides.
            let mut changed = false;
            if existing.description != description {
                existing.description = description.to_owned();
                changed = true;
            }
            if changed {
                existing.updated_at = now_iso();
                existing.save(db).await?;
            }
            return Ok(existing);
        }
        let now = now_iso();
        let next = compute_next_run(cron_expression);
        let mut job = Self {
            name: name.to_owned(),
            description: description.to_owned(),
            cron_expression: LimitedString::new(cron_expression).unwrap(),
            enabled: true,
            last_run_at: None,
            next_run_at: next,
            created_at: now.clone(),
            updated_at: now,
        };
        job.insert(db).await?;
        Ok(job)
    }

    pub fn name_str(&self) -> &str {
        &self.name
    }

    pub fn description_str(&self) -> &str {
        &self.description
    }

    pub fn cron_expression_str(&self) -> &str {
        &self.cron_expression
    }

    pub fn last_run_at_str(&self) -> &str {
        self.last_run_at.as_deref().unwrap_or("")
    }

    pub fn next_run_at_str(&self) -> &str {
        self.next_run_at.as_deref().unwrap_or("")
    }

    pub async fn delete_by_name(db: &Database, name: &str) -> cot::db::Result<()> {
        db.raw(&format!(
            "DELETE FROM furumusic__scheduled_job WHERE name = '{}'",
            name.replace('\'', "''")
        ))
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// JobRun
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct JobRun {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub job_name: LimitedString<100>,
    pub status: LimitedString<32>,
    pub started_at: LimitedString<32>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub log_output: Option<String>,
    pub error_message: Option<String>,
    pub trigger: LimitedString<32>,
}

#[allow(dead_code)]
impl JobRun {
    pub async fn create_running(
        db: &Database,
        job_name: &str,
        trigger: &str,
    ) -> cot::db::Result<Self> {
        let mut run = Self {
            id: Auto::auto(),
            job_name: limited_string(job_name),
            status: LimitedString::new("running").unwrap(),
            started_at: now_iso(),
            finished_at: None,
            duration_ms: None,
            log_output: None,
            error_message: None,
            trigger: limited_string(trigger),
        };
        run.insert(db).await?;
        Ok(run)
    }

    pub async fn set_completed(
        &mut self,
        db: &Database,
        duration_ms: i64,
        log: &str,
    ) -> cot::db::Result<()> {
        crate::metrics::record_scheduler_job(
            self.job_name.as_str(),
            self.trigger.as_str(),
            "completed",
            duration_ms,
        );
        self.status = LimitedString::new("completed").unwrap();
        self.finished_at = Some(now_iso().to_string());
        self.duration_ms = Some(duration_ms);
        self.log_output = Some(log.to_owned());
        self.save(db).await
    }

    pub async fn set_failed(
        &mut self,
        db: &Database,
        duration_ms: i64,
        log: &str,
        error: &str,
    ) -> cot::db::Result<()> {
        crate::metrics::record_scheduler_job(
            self.job_name.as_str(),
            self.trigger.as_str(),
            "failed",
            duration_ms,
        );
        self.status = LimitedString::new("failed").unwrap();
        self.finished_at = Some(now_iso().to_string());
        self.duration_ms = Some(duration_ms);
        self.log_output = Some(log.to_owned());
        self.error_message = Some(error.to_owned());
        self.save(db).await
    }

    pub async fn get_by_id(db: &Database, id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(id)).await
    }

    pub async fn list_by_job(
        pool: &sqlx::PgPool,
        job_name: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<Self>> {
        let rows = sqlx::query_as::<_, JobRunRow>(
            "SELECT id, job_name, status, started_at, finished_at, duration_ms, log_output, error_message, trigger \
             FROM furumusic__job_run WHERE job_name = $1 ORDER BY id DESC LIMIT $2"
        )
        .bind(job_name)
        .bind(limit)
        .fetch_all(pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into_model()).collect())
    }

    /// Mark all "running" job runs as "failed" — called at scheduler startup
    /// to recover from process crashes.
    pub async fn recover_stale(pool: &sqlx::PgPool) -> anyhow::Result<u64> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let result = sqlx::query(
            "UPDATE furumusic__job_run \
             SET status = 'failed', \
                 finished_at = $1, \
                 error_message = 'Process restarted while job was running' \
             WHERE status = 'running'",
        )
        .bind(&now)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_by_job(pool: &sqlx::PgPool, job_name: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM furumusic__job_run WHERE job_name = $1")
            .bind(job_name)
            .execute(pool)
            .await?;
        Ok(())
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }

    pub fn status_str(&self) -> &str {
        &self.status
    }

    pub fn started_at_str(&self) -> &str {
        &self.started_at
    }

    pub fn finished_at_str(&self) -> &str {
        self.finished_at.as_deref().unwrap_or("")
    }

    pub fn duration_display(&self) -> String {
        match self.duration_ms {
            Some(ms) if ms >= 1000 => format!("{:.1}s", ms as f64 / 1000.0),
            Some(ms) => format!("{}ms", ms),
            None => "-".into(),
        }
    }

    pub fn log_output_str(&self) -> &str {
        self.log_output.as_deref().unwrap_or("")
    }

    pub fn error_message_str(&self) -> &str {
        self.error_message.as_deref().unwrap_or("")
    }

    pub fn trigger_str(&self) -> &str {
        &self.trigger
    }

    pub fn status_badge_class(&self) -> &str {
        match self.status.as_str() {
            "completed" => "badge-completed",
            "failed" => "badge-failed",
            "running" => "badge-processing",
            _ => "badge-default",
        }
    }
}

#[derive(sqlx::FromRow)]
struct JobRunRow {
    id: i64,
    job_name: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<i64>,
    log_output: Option<String>,
    error_message: Option<String>,
    trigger: String,
}

impl JobRunRow {
    fn into_model(self) -> JobRun {
        JobRun {
            id: Auto::Fixed(self.id),
            job_name: limited_string(&self.job_name),
            status: limited_string(&self.status),
            started_at: limited_string(&self.started_at),
            finished_at: self.finished_at,
            duration_ms: self.duration_ms,
            log_output: self.log_output,
            error_message: self.error_message,
            trigger: limited_string(&self.trigger),
        }
    }
}

// ---------------------------------------------------------------------------
// PendingReview
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct PendingReview {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub job_run_id: i64,
    pub review_type: LimitedString<64>,
    pub input_path: Option<String>,
    pub context_json: Option<String>,
    pub result_json: Option<String>,
    pub status: LimitedString<32>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
    pub error_message: Option<String>,
}

#[allow(dead_code)]
impl PendingReview {
    pub async fn create(
        db: &Database,
        job_run_id: i64,
        review_type: &str,
        input_path: Option<&str>,
        context_json: Option<&str>,
        result_json: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let mut review = Self {
            id: Auto::auto(),
            job_run_id,
            review_type: LimitedString::new(review_type).unwrap(),
            input_path: input_path.map(|s| s.to_owned()),
            context_json: context_json.map(|s| s.to_owned()),
            result_json: result_json.map(|s| s.to_owned()),
            status: LimitedString::new("pending").unwrap(),
            created_at: now.clone(),
            updated_at: now,
            error_message: None,
        };
        review.insert(db).await?;
        Ok(review)
    }

    pub async fn create_queued(
        db: &Database,
        job_run_id: i64,
        review_type: &str,
        input_path: Option<&str>,
        context_json: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let mut review = Self {
            id: Auto::auto(),
            job_run_id,
            review_type: LimitedString::new(review_type).unwrap(),
            input_path: input_path.map(|s| s.to_owned()),
            context_json: context_json.map(|s| s.to_owned()),
            result_json: None,
            status: LimitedString::new("queued").unwrap(),
            created_at: now.clone(),
            updated_at: now,
            error_message: None,
        };
        review.insert(db).await?;
        Ok(review)
    }

    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn list_by_status(db: &Database, status: &str) -> cot::db::Result<Vec<Self>> {
        let status_val: LimitedString<32> = LimitedString::new(status).unwrap();
        cot::db::query!(PendingReview, $status == status_val)
            .all(db)
            .await
    }

    pub async fn get_by_id(db: &Database, id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(id)).await
    }

    pub async fn set_approved(&mut self, db: &Database) -> cot::db::Result<()> {
        self.status = LimitedString::new("approved").unwrap();
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_rejected(&mut self, db: &Database) -> cot::db::Result<()> {
        self.status = LimitedString::new("rejected").unwrap();
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_queued(&mut self, db: &Database) -> cot::db::Result<()> {
        self.status = LimitedString::new("queued").unwrap();
        self.error_message = None;
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_processing(&mut self, db: &Database) -> cot::db::Result<()> {
        self.status = LimitedString::new("processing").unwrap();
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_auto_approved(&mut self, db: &Database) -> cot::db::Result<()> {
        self.status = LimitedString::new("auto_approved").unwrap();
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_result_json(
        &mut self,
        db: &Database,
        result_json: String,
    ) -> cot::db::Result<()> {
        self.result_json = Some(result_json);
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_failed(&mut self, db: &Database, error: &str) -> cot::db::Result<()> {
        self.status = LimitedString::new("failed").unwrap();
        self.error_message = Some(error.to_owned());
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn list_queued(db: &Database) -> cot::db::Result<Vec<Self>> {
        let status_val: LimitedString<32> = LimitedString::new("queued").unwrap();
        cot::db::query!(PendingReview, $status == status_val)
            .all(db)
            .await
    }

    /// Latest review row for an inbox path: `(id, status, updated_at)`.
    ///
    /// Used by inbox_discover to decide whether a file needs a new review,
    /// a requeue of its existing row, or nothing at all — without creating
    /// a fresh row per retry.
    pub async fn latest_for_path(
        pool: &sqlx::PgPool,
        path: &str,
    ) -> anyhow::Result<Option<(i64, String, String)>> {
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT id, status::text, updated_at::text \
             FROM furumusic__pending_review WHERE input_path = $1 \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(path)
        .fetch_optional(pool)
        .await?;
        Ok(row)
    }

    /// Mark all "processing" reviews as "failed" — called at scheduler
    /// startup to recover from process crashes. These reviews will never
    /// complete because the process restarted; the user can re-queue them
    /// manually from the admin UI.
    pub async fn recover_stale(pool: &sqlx::PgPool) -> anyhow::Result<u64> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let result = sqlx::query(
            "UPDATE furumusic__pending_review \
             SET status = 'failed', \
                 error_message = 'Process restarted while review was being processed', \
                 updated_at = $1 \
             WHERE status = 'processing'",
        )
        .bind(&now)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub fn error_message_str(&self) -> &str {
        self.error_message.as_deref().unwrap_or("")
    }

    pub async fn delete_all(db: &Database) -> cot::db::Result<()> {
        db.raw("DELETE FROM furumusic__pending_review").await?;
        Ok(())
    }

    pub async fn delete_by_status(db: &Database, status: &str) -> cot::db::Result<()> {
        let status_val: LimitedString<32> = LimitedString::new(status).unwrap();
        cot::db::query!(PendingReview, $status == status_val)
            .delete(db)
            .await?;
        Ok(())
    }

    pub async fn delete_by_ids(db: &Database, ids: &[i64]) -> cot::db::Result<()> {
        for chunk in ids.chunks(1000) {
            if chunk.is_empty() {
                continue;
            }
            let id_list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            db.raw(&format!(
                "DELETE FROM furumusic__pending_review WHERE id IN ({id_list})"
            ))
            .await?;
        }
        Ok(())
    }

    pub async fn requeue_by_ids(db: &Database, ids: &[i64]) -> cot::db::Result<()> {
        let now = now_iso().to_string();
        for chunk in ids.chunks(1000) {
            if chunk.is_empty() {
                continue;
            }
            let id_list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            db.raw(&format!(
                "UPDATE furumusic__pending_review \
                 SET status = 'queued', error_message = NULL, updated_at = '{}' \
                 WHERE id IN ({id_list})",
                now.replace('\'', "''")
            ))
            .await?;
        }
        Ok(())
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }

    pub fn status_str(&self) -> &str {
        &self.status
    }

    pub fn review_type_str(&self) -> &str {
        &self.review_type
    }

    pub fn input_path_str(&self) -> &str {
        self.input_path.as_deref().unwrap_or("")
    }

    pub fn context_json_str(&self) -> &str {
        self.context_json.as_deref().unwrap_or("")
    }

    pub fn result_json_str(&self) -> &str {
        self.result_json.as_deref().unwrap_or("")
    }

    pub fn confidence(&self) -> Option<f64> {
        let rj = self.result_json.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(rj).ok()?;
        v.get("confidence")?.as_f64()
    }

    pub fn status_badge_class(&self) -> &str {
        match self.status.as_str() {
            "approved" | "auto_approved" => "badge-completed",
            "rejected" | "failed" => "badge-failed",
            "pending" => "badge-pending",
            "queued" => "badge-queued",
            "processing" => "badge-processing",
            _ => "badge-default",
        }
    }

    pub fn created_at_str(&self) -> &str {
        &self.created_at
    }
}

// ---------------------------------------------------------------------------
// ProcessingStats — per-file LLM processing statistics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct ProcessingStats {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub pending_review_id: i64,
    pub model_name: LimitedString<128>,
    pub llm_duration_ms: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub created_at: LimitedString<32>,
}

impl ProcessingStats {
    pub async fn create(
        db: &Database,
        pending_review_id: i64,
        model_name: &str,
        llm_duration_ms: i64,
        prompt_tokens: i64,
        completion_tokens: i64,
    ) -> cot::db::Result<Self> {
        let mut stats = Self {
            id: Auto::auto(),
            pending_review_id,
            model_name: LimitedString::new(model_name).unwrap(),
            llm_duration_ms,
            prompt_tokens,
            completion_tokens,
            created_at: now_iso(),
        };
        stats.insert(db).await?;
        Ok(stats)
    }

    pub async fn get_by_review_id(db: &Database, review_id: i64) -> cot::db::Result<Option<Self>> {
        let all = cot::db::query!(ProcessingStats, $pending_review_id == review_id)
            .all(db)
            .await?;
        Ok(all.into_iter().next())
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

pub mod db_migrations {
    use super::*;

    #[derive(Debug, Copy, Clone)]
    pub struct M0022CreateScheduledJob;

    impl migrations::Migration for M0022CreateScheduledJob {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0022_create_scheduled_job";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0021_create_trgm_indexes",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__scheduled_job"))
            .fields(&[
                Field::new(Identifier::new("name"), <String as DatabaseField>::TYPE)
                    .primary_key()
                    .set_null(<String as DatabaseField>::NULLABLE),
                Field::new(
                    Identifier::new("description"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("cron_expression"),
                    <LimitedString<100> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("enabled"), <bool as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("last_run_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("next_run_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0023CreateJobRun;

    impl migrations::Migration for M0023CreateJobRun {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0023_create_job_run";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0022_create_scheduled_job",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__job_run"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("job_name"),
                    <LimitedString<100> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("status"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("started_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("finished_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("duration_ms"), <i64 as DatabaseField>::TYPE)
                    .set_null(true),
                Field::new(
                    Identifier::new("log_output"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("error_message"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("trigger"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0024CreatePendingReview;

    impl migrations::Migration for M0024CreatePendingReview {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0024_create_pending_review";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0023_create_job_run",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__pending_review"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("job_run_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("review_type"),
                    <LimitedString<64> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("input_path"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("context_json"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("result_json"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("status"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_scheduler_indexes(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        let stmts = [
            "CREATE INDEX idx_job_run_job_name ON furumusic__job_run (job_name, id DESC)",
            "CREATE INDEX idx_job_run_status ON furumusic__job_run (status)",
            "CREATE INDEX idx_pending_review_status ON furumusic__pending_review (status, created_at)",
            "CREATE INDEX idx_pending_review_job_run ON furumusic__pending_review (job_run_id)",
        ];
        for stmt in stmts {
            ctx.db.raw(stmt).await?;
        }
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0025CreateSchedulerIndexes;

    impl migrations::Migration for M0025CreateSchedulerIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0025_create_scheduler_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0024_create_pending_review",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_scheduler_indexes).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn add_pending_review_error_message(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furumusic__pending_review ADD COLUMN error_message TEXT")
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0026AddPendingReviewErrorMessage;

    impl migrations::Migration for M0026AddPendingReviewErrorMessage {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0026_add_pending_review_error_message";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0025_create_scheduler_indexes",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(add_pending_review_error_message).build()];
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0027CreateProcessingStats;

    impl migrations::Migration for M0027CreateProcessingStats {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0027_create_processing_stats";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0026_add_pending_review_error_message",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__processing_stats"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("pending_review_id"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("model_name"),
                    <LimitedString<128> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("llm_duration_ms"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("prompt_tokens"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("completion_tokens"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    pub const MIGRATIONS: &[&SyncDynMigration] = &[
        &M0022CreateScheduledJob,
        &M0023CreateJobRun,
        &M0024CreatePendingReview,
        &M0025CreateSchedulerIndexes,
        &M0026AddPendingReviewErrorMessage,
        &M0027CreateProcessingStats,
    ];
}

// ---------------------------------------------------------------------------
// Job Trait + JobContext + JobLog
// ---------------------------------------------------------------------------

pub struct JobContext {
    pub config: Arc<AppConfig>,
    pub db: Database,
    pub pool: sqlx::PgPool,
    pub run_id: i64,
    pub registry: Arc<JobRegistry>,
}

pub struct JobLog {
    lines: Vec<String>,
    pool: Option<sqlx::PgPool>,
    run_id: i64,
}

#[allow(dead_code)]
impl JobLog {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            pool: None,
            run_id: 0,
        }
    }

    pub fn with_live_flush(pool: sqlx::PgPool, run_id: i64) -> Self {
        Self {
            lines: Vec::new(),
            pool: Some(pool),
            run_id,
        }
    }

    pub fn info(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        self.lines.push(format!("[{ts} INFO] {msg}"));
        tracing::info!("{msg}");
        self.flush_to_db();
    }

    pub fn warn(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        self.lines.push(format!("[{ts} WARN] {msg}"));
        tracing::warn!("{msg}");
        self.flush_to_db();
    }

    pub fn error(&mut self, msg: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S");
        self.lines.push(format!("[{ts} ERROR] {msg}"));
        tracing::error!("{msg}");
        self.flush_to_db();
    }

    pub fn output(&self) -> String {
        self.lines.join("\n")
    }

    fn flush_to_db(&self) {
        if let Some(pool) = &self.pool {
            let output = self.output();
            let run_id = self.run_id;
            let pool = pool.clone();
            tokio::spawn(async move {
                let _ = sqlx::query("UPDATE furumusic__job_run SET log_output = $1 WHERE id = $2")
                    .bind(&output)
                    .bind(run_id)
                    .execute(&pool)
                    .await;
            });
        }
    }
}

#[async_trait::async_trait]
pub trait Job: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn default_cron(&self) -> &'static str;
    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> anyhow::Result<()>;
}

pub struct JobRegistry {
    jobs: HashMap<&'static str, Box<dyn Job>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    pub fn register(&mut self, job: impl Job) {
        self.jobs.insert(job.name(), Box::new(job));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Job> {
        self.jobs.get(name).map(|b| b.as_ref())
    }

    pub fn all_jobs(&self) -> Vec<&dyn Job> {
        self.jobs.values().map(|b| b.as_ref()).collect()
    }
}

// ---------------------------------------------------------------------------
// Cron helper — uses croner (via tokio-cron-scheduler) for 6-field cron
// ---------------------------------------------------------------------------

pub fn compute_next_run(cron_expr: &str) -> Option<String> {
    let cron = croner::parser::CronParser::new().parse(cron_expr).ok()?;
    let now = chrono::Utc::now();
    let next = cron.find_next_occurrence(&now, false).ok()?;
    Some(next.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

// ---------------------------------------------------------------------------
// SchedulerHandle — shared state for the cron scheduler
// ---------------------------------------------------------------------------

pub struct SchedulerHandle {
    pub scheduler: tokio_cron_scheduler::JobScheduler,
    pub registry: Arc<JobRegistry>,
    job_uuids: tokio::sync::RwLock<HashMap<String, uuid::Uuid>>,
    /// Shared database connection — avoids creating a new one per cron fire.
    shared_db: Database,
    /// Shared connection pool — avoids creating a new pool per cron fire.
    shared_pool: sqlx::PgPool,
}

impl SchedulerHandle {
    /// Start a job immediately in the background and return the created run id.
    pub async fn trigger_job_now_background(
        self: Arc<Self>,
        job_name: &str,
    ) -> anyhow::Result<i64> {
        self.registry
            .get(job_name)
            .ok_or_else(|| anyhow::anyhow!("unknown job: {job_name}"))?;

        let db = self.shared_db.clone();
        let pool = self.shared_pool.clone();
        let (live_config, _) = AppConfig::load_with_db(&db).await;
        let run = JobRun::create_running(&db, job_name, "manual")
            .await
            .map_err(|e| anyhow::anyhow!("failed to create job run: {e}"))?;
        let run_id = run.id_val();
        let job_name = job_name.to_owned();
        let handle = Arc::clone(&self);

        tokio::spawn(async move {
            handle
                .finish_manual_run(job_name, live_config, db, pool, run)
                .await;
        });

        Ok(run_id)
    }

    /// Execute a job immediately (manual or programmatic trigger).
    pub async fn trigger_job_now(&self, job_name: &str) -> anyhow::Result<i64> {
        let job_impl = self
            .registry
            .get(job_name)
            .ok_or_else(|| anyhow::anyhow!("unknown job: {job_name}"))?;

        let db = &self.shared_db;
        let pool = &self.shared_pool;

        let (live_config, _) = AppConfig::load_with_db(db).await;

        let mut run = JobRun::create_running(db, job_name, "manual")
            .await
            .map_err(|e| anyhow::anyhow!("failed to create job run: {e}"))?;

        let start = std::time::Instant::now();
        let ctx = JobContext {
            config: Arc::new(live_config),
            db: db.clone(),
            pool: pool.clone(),
            run_id: run.id_val(),
            registry: Arc::clone(&self.registry),
        };
        let mut log = JobLog::with_live_flush(pool.clone(), run.id_val());

        match job_impl.run(&ctx, &mut log).await {
            Ok(()) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = run.set_completed(db, duration_ms, &log.output()).await;
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = run
                    .set_failed(db, duration_ms, &log.output(), &e.to_string())
                    .await;
            }
        }

        if let Ok(Some(mut sched_job)) = ScheduledJob::get_by_name(db, job_name).await {
            sched_job.last_run_at = Some(now_iso().to_string());
            sched_job.updated_at = now_iso();
            let _ = sched_job.save(db).await;
        }

        Ok(run.id_val())
    }

    async fn finish_manual_run(
        self: Arc<Self>,
        job_name: String,
        live_config: AppConfig,
        db: Database,
        pool: sqlx::PgPool,
        mut run: JobRun,
    ) {
        let Some(job_impl) = self.registry.get(&job_name) else {
            let _ = run
                .set_failed(&db, 0, "", &format!("unknown job: {job_name}"))
                .await;
            return;
        };

        let start = std::time::Instant::now();
        let ctx = JobContext {
            config: Arc::new(live_config),
            db: db.clone(),
            pool: pool.clone(),
            run_id: run.id_val(),
            registry: Arc::clone(&self.registry),
        };
        let mut log = JobLog::with_live_flush(pool, run.id_val());

        match job_impl.run(&ctx, &mut log).await {
            Ok(()) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = run.set_completed(&db, duration_ms, &log.output()).await;
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = run
                    .set_failed(&db, duration_ms, &log.output(), &e.to_string())
                    .await;
            }
        }

        if let Ok(Some(mut sched_job)) = ScheduledJob::get_by_name(&db, &job_name).await {
            sched_job.last_run_at = Some(now_iso().to_string());
            sched_job.updated_at = now_iso();
            let _ = sched_job.save(&db).await;
        }
    }

    /// Remove a cron job from the scheduler and re-add it with a new cron
    /// expression.  Also updates the DB row.
    pub async fn reschedule_job(&self, job_name: &str, new_cron: &str) -> anyhow::Result<()> {
        // Remove old UUID if present
        {
            let mut uuids = self.job_uuids.write().await;
            if let Some(old_uuid) = uuids.remove(job_name) {
                let _ = self.scheduler.remove(&old_uuid).await;
            }
        }

        // Editing a disabled job must not start it.
        let job = ScheduledJob::get_by_name(&self.shared_db, job_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("job not found: {job_name}"))?;
        if job.enabled {
            self.add_cron_job(job_name, new_cron).await?;
        }

        // Update DB
        if let Ok(Some(mut sched_job)) = ScheduledJob::get_by_name(&self.shared_db, job_name).await
        {
            sched_job.cron_expression = LimitedString::new(new_cron).unwrap();
            sched_job.next_run_at = compute_next_run(new_cron);
            sched_job.updated_at = now_iso();
            sched_job.save(&self.shared_db).await?;
        }

        Ok(())
    }

    /// Enable or disable a job.  When disabling, removes it from the cron
    /// scheduler.  When enabling, adds it back.
    pub async fn toggle_job(&self, job_name: &str, enabled: bool) -> anyhow::Result<()> {
        let mut sched_job = ScheduledJob::get_by_name(&self.shared_db, job_name)
            .await
            .map_err(|e| anyhow::anyhow!("db: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("job not found: {job_name}"))?;

        if !enabled {
            // Remove from scheduler
            let mut uuids = self.job_uuids.write().await;
            if let Some(old_uuid) = uuids.remove(job_name) {
                let _ = self.scheduler.remove(&old_uuid).await;
            }
        } else {
            // Add to scheduler with current cron
            let cron = sched_job.cron_expression_str().to_owned();
            self.add_cron_job(job_name, &cron).await?;
        }

        sched_job.enabled = enabled;
        if enabled {
            sched_job.next_run_at = compute_next_run(sched_job.cron_expression_str());
        }
        sched_job.updated_at = now_iso();
        let _ = sched_job.save(&self.shared_db).await;

        Ok(())
    }

    /// Internal: create a tokio-cron-scheduler Job and register it.
    async fn add_cron_job(&self, job_name: &str, cron_expr: &str) -> anyhow::Result<()> {
        let name = job_name.to_owned();
        let registry = Arc::clone(&self.registry);
        let shared_db = self.shared_db.clone();
        let shared_pool = self.shared_pool.clone();

        let cron_job = tokio_cron_scheduler::Job::new_async(cron_expr, move |_uuid, _lock| {
            let name = name.clone();
            let registry = Arc::clone(&registry);
            let db = shared_db.clone();
            let pool = shared_pool.clone();
            Box::pin(async move {
                run_scheduled_job(&name, &registry, &db, &pool).await;
            })
        })?;

        let uuid = self.scheduler.add(cron_job).await?;
        self.job_uuids
            .write()
            .await
            .insert(job_name.to_owned(), uuid);

        Ok(())
    }
}

/// Runs a single scheduled job — called from cron closure.
async fn run_scheduled_job(
    job_name: &str,
    registry: &Arc<JobRegistry>,
    db: &Database,
    pool: &sqlx::PgPool,
) {
    tracing::info!(job = job_name, "Cron fire received");

    let job_impl = match registry.get(job_name) {
        Some(j) => j,
        None => {
            tracing::error!(job = job_name, "Cron fired but job not found in registry");
            return;
        }
    };

    // Check agent_enabled (re-read from DB every run)
    let (live_config, _) = AppConfig::load_with_db(db).await;
    if !live_config.agent_enabled
        && job_name != "lastfm_popularity"
        && job_name != "lastfm_scrobble"
        && job_name != "archive_cleanup"
        && job_name != "artwork_backfill"
    {
        tracing::warn!(job = job_name, "Skipping: agent_enabled=false");
        return;
    }

    // Check if job is still enabled in DB
    match ScheduledJob::get_by_name(db, job_name).await {
        Ok(Some(sj)) if !sj.enabled => {
            tracing::warn!(job = job_name, "Skipping: job disabled in DB");
            return;
        }
        _ => {}
    }

    // Pre-check: skip inbox_process if orchestrator is already running
    // (avoids creating a JobRun that will immediately exit)
    if job_name == "inbox_process" && crate::jobs::inbox_process::is_orchestrator_running() {
        tracing::info!(
            job = job_name,
            "Scheduler: skipping — orchestrator already running (pre-check)"
        );
        return;
    }

    tracing::info!(job = job_name, "Scheduler: starting job");

    let mut run = match JobRun::create_running(db, job_name, "scheduled").await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(job = job_name, "Failed to create job run: {e}");
            return;
        }
    };

    let start = std::time::Instant::now();
    let ctx = JobContext {
        config: Arc::new(live_config),
        db: db.clone(),
        pool: pool.clone(),
        run_id: run.id_val(),
        registry: Arc::clone(registry),
    };
    let mut log = JobLog::with_live_flush(pool.clone(), run.id_val());

    match job_impl.run(&ctx, &mut log).await {
        Ok(()) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            tracing::info!(job = job_name, duration_ms, "Job completed successfully");
            let _ = run.set_completed(db, duration_ms, &log.output()).await;
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            tracing::error!(job = job_name, duration_ms, "Job failed: {e}");
            let _ = run
                .set_failed(db, duration_ms, &log.output(), &e.to_string())
                .await;
        }
    }

    // Update ScheduledJob last_run_at + next_run_at
    let now = now_iso();
    let cron_expr = match ScheduledJob::get_by_name(db, job_name).await {
        Ok(Some(j)) => j.cron_expression_str().to_owned(),
        _ => return,
    };
    let next = compute_next_run(&cron_expr);
    let next_str = next.as_deref().unwrap_or("");
    let _ = sqlx::query(
        "UPDATE furumusic__scheduled_job SET last_run_at = $1, next_run_at = $2, updated_at = $3 WHERE name = $4"
    )
        .bind(now.as_str())
        .bind(next_str)
        .bind(now.as_str())
        .bind(job_name)
        .execute(pool)
        .await;
}

// ---------------------------------------------------------------------------
// start_scheduler — creates and starts the SchedulerHandle
// ---------------------------------------------------------------------------

pub async fn start_scheduler(
    config: &Arc<AppConfig>,
    registry: Arc<JobRegistry>,
) -> anyhow::Result<Arc<SchedulerHandle>> {
    if config.database_url.is_empty() {
        anyhow::bail!("No database URL configured, scheduler will not start");
    }

    let db = Database::new(&config.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("scheduler db connect: {e}"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await?;

    // Recover stale runs and reviews from previous process crash
    match JobRun::recover_stale(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!("Recovered {n} stale job run(s) stuck in 'running' status"),
        Err(e) => tracing::error!("Failed to recover stale job runs: {e}"),
    }
    match PendingReview::recover_stale(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!("Marked {n} stale review(s) stuck in 'processing' as 'failed'"),
        Err(e) => tracing::error!("Failed to recover stale reviews: {e}"),
    }

    let (live_config, _) = AppConfig::load_with_db(&db).await;
    if !live_config.agent_storage_dir.trim().is_empty() {
        match crate::media_paths::normalize_media_file_paths(&pool, &live_config.agent_storage_dir)
            .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!("Normalized {n} media file path(s) to relative storage paths"),
            Err(e) => tracing::warn!("Failed to normalize media file paths: {e:#}"),
        }
    }
    if !live_config.agent_inbox_dir.trim().is_empty() {
        match crate::media_paths::normalize_pending_review_paths(
            &pool,
            &live_config.agent_inbox_dir,
        )
        .await
        {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!("Normalized {n} pending review path(s) to relative inbox paths")
            }
            Err(e) => tracing::warn!("Failed to normalize pending review paths: {e:#}"),
        }
    }

    // Upsert ScheduledJob rows
    for job in registry.all_jobs() {
        ScheduledJob::upsert(&db, job.name(), job.description(), job.default_cron())
            .await
            .map_err(|e| anyhow::anyhow!("failed to upsert scheduled job {}: {e}", job.name()))?;
    }

    // Clean orphans
    if let Ok(all_jobs) = ScheduledJob::list_all(&db).await {
        for sched_job in &all_jobs {
            if registry.get(sched_job.name_str()).is_none() {
                tracing::warn!(
                    "Removing orphaned scheduled job '{}' (no longer registered)",
                    sched_job.name_str()
                );
                let _ = ScheduledJob::delete_by_name(&db, sched_job.name_str()).await;
            }
        }
    }

    // Create scheduler
    let sched = tokio_cron_scheduler::JobScheduler::new().await?;

    let handle = Arc::new(SchedulerHandle {
        scheduler: sched,
        registry: Arc::clone(&registry),
        job_uuids: tokio::sync::RwLock::new(HashMap::new()),
        shared_db: db.clone(),
        shared_pool: pool.clone(),
    });

    // Register cron jobs for enabled jobs
    let mut cron_count = 0u32;
    if let Ok(all_jobs) = ScheduledJob::list_all(&db).await {
        for sched_job in &all_jobs {
            if !sched_job.enabled {
                continue;
            }
            let cron_expr = sched_job.cron_expression_str();
            if cron_expr.is_empty() {
                continue;
            }
            match handle.add_cron_job(sched_job.name_str(), cron_expr).await {
                Ok(()) => {
                    cron_count += 1;
                    // Update next_run_at in DB
                    if let Some(next) = compute_next_run(cron_expr) {
                        let _ = sqlx::query(
                            "UPDATE furumusic__scheduled_job SET next_run_at = $1 WHERE name = $2",
                        )
                        .bind(&next)
                        .bind(sched_job.name_str())
                        .execute(&pool)
                        .await;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        job = sched_job.name_str(),
                        cron = cron_expr,
                        "Failed to register cron job: {e}"
                    );
                }
            }
        }
    }

    handle.scheduler.start().await?;
    tracing::info!("Scheduler started with {cron_count} cron jobs");

    Ok(handle)
}

// ---------------------------------------------------------------------------
// Standalone trigger — for programmatic (job→job) chaining
// ---------------------------------------------------------------------------

/// Trigger a job immediately without needing a SchedulerHandle.
/// Used by jobs to chain-spawn other jobs (e.g. inbox_discover → inbox_process).
/// Reuses the caller's db/pool to avoid creating new connections.
pub async fn trigger_job_now(
    _config: &Arc<AppConfig>,
    db: &Database,
    pool: &sqlx::PgPool,
    registry: &Arc<JobRegistry>,
    job_name: &str,
) -> anyhow::Result<i64> {
    // Pre-check: skip inbox_process if orchestrator is already running
    if job_name == "inbox_process" && crate::jobs::inbox_process::is_orchestrator_running() {
        tracing::info!(
            job = job_name,
            "trigger_job_now: skipping — orchestrator already running (pre-check)"
        );
        anyhow::bail!("orchestrator already running");
    }

    let job_impl = registry
        .get(job_name)
        .ok_or_else(|| anyhow::anyhow!("unknown job: {job_name}"))?;

    let (live_config, _) = AppConfig::load_with_db(db).await;

    let mut run = JobRun::create_running(db, job_name, "programmatic")
        .await
        .map_err(|e| anyhow::anyhow!("failed to create job run: {e}"))?;

    let start = std::time::Instant::now();
    let ctx = JobContext {
        config: Arc::new(live_config),
        db: db.clone(),
        pool: pool.clone(),
        run_id: run.id_val(),
        registry: Arc::clone(registry),
    };
    let mut log = JobLog::with_live_flush(pool.clone(), run.id_val());

    match job_impl.run(&ctx, &mut log).await {
        Ok(()) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            let _ = run.set_completed(db, duration_ms, &log.output()).await;
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            let _ = run
                .set_failed(db, duration_ms, &log.output(), &e.to_string())
                .await;
        }
    }

    if let Ok(Some(mut sched_job)) = ScheduledJob::get_by_name(db, job_name).await {
        sched_job.last_run_at = Some(now_iso().to_string());
        sched_job.updated_at = now_iso();
        let _ = sched_job.save(db).await;
    }

    Ok(run.id_val())
}
