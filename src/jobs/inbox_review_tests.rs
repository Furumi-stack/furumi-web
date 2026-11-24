use super::inbox_discover::InboxDiscoverJob;
use crate::agent::dto::NormalizedFields;
use crate::config::AppConfig;
use crate::scheduler::{Job, JobContext, JobLog, JobRegistry, PendingReview};
use cot::App;
use cot::db::Database;
use cot::db::migrations::MigrationEngine;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::fs;

fn tagged_mp3(title: &str) -> anyhow::Result<Vec<u8>> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=0.25",
            "-metadata",
            &format!("title={title}"),
            "-f",
            "mp3",
            "pipe:1",
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn reviews_for_path(pool: &PgPool, path: &str) -> anyhow::Result<Vec<(i64, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, status::text, context_json::jsonb->>'sha256'
         FROM furumusic__pending_review WHERE input_path = $1 ORDER BY id",
    )
    .bind(path)
    .fetch_all(pool)
    .await?)
}

async fn review_for_hash(db: &Database, path: &str, sha: &str) -> anyhow::Result<PendingReview> {
    Ok(PendingReview::create(
        db,
        0,
        "new_file",
        Some(path),
        Some(&json!({"sha256": sha}).to_string()),
        Some(r#"{"title":"saved title"}"#),
    )
    .await?)
}

async fn scan(ctx: &JobContext) -> anyhow::Result<()> {
    InboxDiscoverJob.run(ctx, &mut JobLog::new()).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL in FURU_IMPORT_TEST_DATABASE_URL"]
async fn inbox_review_rediscovery_respects_dismissals_and_verifies_catalog_bytes()
-> anyhow::Result<()> {
    let url = std::env::var("FURU_IMPORT_TEST_DATABASE_URL")?;
    let db = Database::new(&url).await?;
    let registry = std::sync::Arc::new(JobRegistry::new());
    let app = crate::admin::AdminApp::new(Default::default(), registry.clone(), Default::default());
    MigrationEngine::new(app.migrations())?.run(&db).await?;
    let root = tempfile::tempdir()?;
    let inbox = root.path().join("inbox");
    let library = root.path().join("library");
    let scope = uuid::Uuid::new_v4().to_string();
    let ctx = JobContext {
        config: AppConfig {
            agent_inbox_dir: inbox.to_str().unwrap().into(),
            agent_storage_dir: library.to_str().unwrap().into(),
            ..Default::default()
        }
        .into(),
        db,
        pool: PgPool::connect(&url).await?,
        run_id: 0,
        registry,
    };
    fs::create_dir_all(inbox.join(&scope))?;
    let path = format!("{scope}/upload.mp3");
    let first = tagged_mp3(&format!("{scope} first"))?;
    let replacement = tagged_mp3(&format!("{scope} replacement"))?;
    fs::write(inbox.join(&path), &first)?;
    scan(&ctx).await?;
    let reviews = reviews_for_path(&ctx.pool, &path).await?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(
        PendingReview::dismiss(&ctx.pool, &[reviews[0].0], None).await?,
        1
    );
    scan(&ctx).await?;
    assert!(reviews_for_path(&ctx.pool, &path).await?.is_empty());
    assert_eq!(fs::read(inbox.join(&path))?, first);
    fs::write(inbox.join(&path), &replacement)?;
    scan(&ctx).await?;
    let reviews = reviews_for_path(&ctx.pool, &path).await?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].1, "queued");
    assert_eq!(reviews[0].2, hash(&replacement));

    let fields = NormalizedFields {
        artist: Some(scope.clone()),
        title: Some("original".into()),
        album: Some("album".into()),
        ..Default::default()
    };
    let import_path = format!("{scope}/library.mp3");
    fs::write(inbox.join(&import_path), &first)?;
    let sha = hash(&first);
    scan(&ctx).await?;
    let newly_uploaded = reviews_for_path(&ctx.pool, &import_path).await?;
    assert_eq!(newly_uploaded.len(), 1);
    assert_eq!(newly_uploaded[0].2, sha);
    super::inbox_process::finalize_approved(
        &ctx.db,
        &ctx.pool,
        &ctx.config,
        &import_path,
        &fields,
        &json!({ "sha256": sha, "file_size": first.len() }),
        super::inbox_process::ReviewApproval::Check(None),
    )
    .await?;
    let stored = library.join(&scope).join("album/original.mp3");
    assert_eq!(fs::read(&stored)?, first);
    let missing_path = format!("{scope}/consumed.mp3");
    let consumed = review_for_hash(&ctx.db, &missing_path, &sha).await?;
    fs::remove_file(inbox.join(&path))?;
    // A catalog row with the right hash but different bytes cannot resolve a review.
    fs::write(&stored, b"wrong contents")?;
    scan(&ctx).await?;
    assert_ne!(
        PendingReview::get_by_id(&ctx.db, consumed.id_val())
            .await?
            .unwrap()
            .status_str(),
        "auto_approved"
    );
    fs::write(&stored, &first)?;
    scan(&ctx).await?;
    let consumed = PendingReview::get_by_id(&ctx.db, consumed.id_val())
        .await?
        .unwrap();
    assert_eq!(consumed.status_str(), "auto_approved");
    assert_eq!(consumed.result_json_str(), r#"{"title":"saved title"}"#);
    assert!(consumed.error_message.is_none());
    assert!(!inbox.join(&missing_path).exists());

    fs::create_dir_all(inbox.join(&scope))?;
    let conflict_path = format!("{scope}/edition.mp3");
    let conflict_bytes = tagged_mp3(&format!("{scope} another edition"))?;
    fs::write(inbox.join(&conflict_path), &conflict_bytes)?;
    let mut conflict = review_for_hash(&ctx.db, &conflict_path, &hash(&conflict_bytes)).await?;
    let error = crate::agent::dedup::DuplicateReview("track 42: duration differs".into()).into();
    conflict.set_approval_error(&ctx.db, &error).await?;
    assert_eq!(conflict.status_str(), "pending");
    // Older approvals persisted the same conflict as a failure eligible for hourly retry.
    let reason = "Review approval failed: Duplicate check needs review: track 42: duration differs";
    conflict.set_failed(&ctx.db, reason).await?;
    sqlx::query(
        "UPDATE furumusic__pending_review SET updated_at='2001-01-01T00:00:00Z' WHERE id=$1",
    )
    .bind(conflict.id_val())
    .execute(&ctx.pool)
    .await?;

    let changed_path = format!("{scope}/replaced.mp3");
    let stale = review_for_hash(&ctx.db, &changed_path, &sha).await?;
    fs::write(inbox.join(&changed_path), &replacement)?;
    let duplicate_path = format!("{scope}/another-copy.mp3");
    let duplicate = review_for_hash(&ctx.db, &duplicate_path, &sha).await?;
    fs::write(inbox.join(&duplicate_path), &first)?;
    scan(&ctx).await?;
    scan(&ctx).await?;
    let duplicate = PendingReview::get_by_id(&ctx.db, duplicate.id_val())
        .await?
        .unwrap();
    assert_eq!(duplicate.status_str(), "auto_approved");
    assert!(!inbox.join(&duplicate_path).exists());
    assert_eq!(fs::read(inbox.join(&changed_path))?, replacement);
    let queued: Vec<_> = reviews_for_path(&ctx.pool, &changed_path)
        .await?
        .into_iter()
        .filter(|(_, status, _)| status == "queued")
        .collect();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].2, hash(&replacement));
    assert_ne!(queued[0].0, stale.id_val());
    let conflict = PendingReview::get_by_id(&ctx.db, conflict.id_val())
        .await?
        .unwrap();
    assert_eq!(conflict.status_str(), "pending");
    assert!(conflict.error_message_str().contains("duration differs"));
    assert_eq!(fs::read(inbox.join(&conflict_path))?, conflict_bytes);
    Ok(())
}
