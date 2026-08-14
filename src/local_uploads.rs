use std::collections::HashMap;

use anyhow::{Context, bail};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

const LOCAL_UPLOAD_LIST_LIMIT: i64 = 100;

#[derive(Debug, Clone, Serialize)]
pub struct LocalUploadDto {
    pub id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub status: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct LocalUploadRow {
    id: String,
    user_id: i64,
    filename: String,
    size_bytes: i64,
    status: String,
    inbox_path: String,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

impl LocalUploadRow {
    fn dto(&self) -> LocalUploadDto {
        LocalUploadDto {
            id: self.id.clone(),
            filename: self.filename.clone(),
            size_bytes: u64::try_from(self.size_bytes).unwrap_or(0),
            status: self.status.clone(),
            error: self.error.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            completed_at: self.completed_at.clone(),
        }
    }
}

pub async fn create(
    pool: &PgPool,
    id: &str,
    user_id: i64,
    filename: &str,
    size_bytes: u64,
    inbox_path: &str,
) -> anyhow::Result<()> {
    let now = now_string();
    sqlx::query(
        r#"INSERT INTO furumusic__local_upload
              (id, user_id, filename, size_bytes, status, inbox_path, error,
               created_at, updated_at, completed_at)
           VALUES ($1, $2, $3, $4, 'uploading', $5, NULL, $6, $6, NULL)"#,
    )
    .bind(id)
    .bind(user_id)
    .bind(filename)
    .bind(i64::try_from(size_bytes).unwrap_or(i64::MAX))
    .bind(inbox_path)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_queued(pool: &PgPool, id: &str, user_id: i64) -> anyhow::Result<LocalUploadDto> {
    update_status(pool, id, user_id, "queued", None).await?;
    load(pool, user_id, id).await.map(|row| row.dto())
}

pub async fn mark_failed(pool: &PgPool, id: &str, user_id: i64, error: &str) -> anyhow::Result<()> {
    update_status(pool, id, user_id, "failed", Some(error)).await
}

pub async fn list(
    pool: &PgPool,
    user_id: i64,
    inbox_dir: &str,
) -> anyhow::Result<Vec<LocalUploadDto>> {
    sync_statuses(pool, user_id, inbox_dir).await?;
    let rows: Vec<LocalUploadRow> = sqlx::query_as(
        r#"SELECT id, user_id, filename, size_bytes, status, inbox_path, error,
                  created_at, updated_at, completed_at
           FROM furumusic__local_upload
           WHERE user_id = $1
           ORDER BY created_at DESC, id DESC
           LIMIT $2"#,
    )
    .bind(user_id)
    .bind(LOCAL_UPLOAD_LIST_LIMIT)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(LocalUploadRow::dto).collect())
}

pub async fn remove(pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<()> {
    let result = sqlx::query("DELETE FROM furumusic__local_upload WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        bail!("file upload history entry not found");
    }
    Ok(())
}

async fn sync_statuses(pool: &PgPool, user_id: i64, inbox_dir: &str) -> anyhow::Result<()> {
    let inbox_dir = inbox_dir.trim();
    if inbox_dir.is_empty() {
        bail!("agent_inbox_dir is not configured");
    }
    let inbox_root = crate::media_paths::resolve_config_path_buf(inbox_dir);
    if !inbox_root.is_absolute() {
        bail!("agent_inbox_dir must be an absolute path");
    }

    let rows: Vec<LocalUploadRow> = sqlx::query_as(
        r#"SELECT id, user_id, filename, size_bytes, status, inbox_path, error,
                  created_at, updated_at, completed_at
           FROM furumusic__local_upload
           WHERE user_id = $1 AND status <> 'complete'"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let inbox_paths: Vec<String> = rows.iter().map(|row| row.inbox_path.clone()).collect();
    let state_rows: Vec<(String, String, i64)> = sqlx::query_as(
        r#"SELECT input_path, status::text, COUNT(*)
           FROM furumusic__pending_review
           WHERE input_path = ANY($1)
           GROUP BY input_path, status"#,
    )
    .bind(&inbox_paths)
    .fetch_all(pool)
    .await?;
    let mut states_by_path: HashMap<String, HashMap<String, i64>> = HashMap::new();
    for (input_path, status, total) in state_rows {
        states_by_path
            .entry(input_path)
            .or_default()
            .insert(status, total);
    }
    let error_rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT DISTINCT ON (input_path) input_path, error_message
           FROM furumusic__pending_review
           WHERE input_path = ANY($1) AND status = 'failed'
             AND error_message IS NOT NULL
           ORDER BY input_path, id DESC"#,
    )
    .bind(&inbox_paths)
    .fetch_all(pool)
    .await?;
    let errors_by_path: HashMap<String, String> = error_rows.into_iter().collect();

    for row in rows {
        let counts = states_by_path
            .get(&row.inbox_path)
            .cloned()
            .unwrap_or_default();
        let total: i64 = counts.values().sum();

        let mut terminal_error = None;
        let next = if total == 0 {
            if matches!(row.status.as_str(), "uploading" | "failed" | "needs_review") {
                continue;
            }
            let full_path = crate::media_paths::resolve_path_from_root(inbox_dir, &row.inbox_path);
            if tokio::fs::try_exists(full_path).await.unwrap_or(false) {
                "queued"
            } else {
                "complete"
            }
        } else if count(&counts, "processing") > 0 {
            "ai_processing"
        } else if count(&counts, "queued") > 0 {
            "queued"
        } else if count(&counts, "failed") > 0 {
            terminal_error = errors_by_path.get(&row.inbox_path).cloned();
            "failed"
        } else if count(&counts, "pending") > 0 || count(&counts, "rejected") > 0 {
            "needs_review"
        } else if count(&counts, "approved") > 0 || count(&counts, "auto_approved") > 0 {
            "complete"
        } else {
            "queued"
        };

        if row.status != next || row.error != terminal_error {
            update_status(pool, &row.id, row.user_id, next, terminal_error.as_deref()).await?;
        }
    }
    Ok(())
}

async fn update_status(
    pool: &PgPool,
    id: &str,
    user_id: i64,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let now = now_string();
    let completed_at =
        matches!(status, "complete" | "failed" | "needs_review").then(|| now.clone());
    sqlx::query(
        r#"UPDATE furumusic__local_upload
           SET status = $3, error = $4, updated_at = $5, completed_at = $6
           WHERE id = $1 AND user_id = $2"#,
    )
    .bind(id)
    .bind(user_id)
    .bind(status)
    .bind(error.map(trim_error))
    .bind(&now)
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn load(pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<LocalUploadRow> {
    sqlx::query_as(
        r#"SELECT id, user_id, filename, size_bytes, status, inbox_path, error,
                  created_at, updated_at, completed_at
           FROM furumusic__local_upload WHERE id = $1 AND user_id = $2"#,
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .context("file upload history entry not found")
}

fn count(counts: &HashMap<String, i64>, status: &str) -> i64 {
    counts.get(status).copied().unwrap_or(0)
}

fn trim_error(value: &str) -> String {
    value.chars().take(4_000).collect()
}

fn now_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
