use super::finalize_review_approval;
use crate::agent::dto::NormalizedFields;
use crate::config::AppConfig;
use crate::scheduler::{JobRegistry, PendingReview};
use cot::App;
use cot::db::Database;
use cot::db::migrations::MigrationEngine;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

#[tokio::test]
#[ignore = "requires disposable PostgreSQL in FURU_IMPORT_TEST_DATABASE_URL"]
async fn review_approval_requires_audio_and_moves_relative_uploads() -> anyhow::Result<()> {
    let url = std::env::var("FURU_IMPORT_TEST_DATABASE_URL")?;
    let db = Database::new(&url).await?;
    let app = crate::admin::AdminApp::new(
        Default::default(),
        JobRegistry::new().into(),
        Default::default(),
    );
    MigrationEngine::new(app.migrations())?.run(&db).await?;
    let pool = PgPool::connect(&url).await?;
    let root = tempfile::tempdir()?;
    let inbox = root.path().join("inbox");
    let library = root.path().join("library");
    let config = AppConfig {
        agent_inbox_dir: inbox.to_str().unwrap().into(),
        agent_storage_dir: library.to_str().unwrap().into(),
        ..Default::default()
    };
    let artist = format!("Artist-{}", uuid::Uuid::new_v4());
    let fields = NormalizedFields {
        artist: Some(artist.clone()),
        album: Some("Альбом".into()),
        title: Some("Мутанты".into()),
        ..Default::default()
    };
    let relative_source = "user_uploads/3/Папка/data.mp3";
    let source = inbox.join(relative_source);
    let relative_destination = format!("{artist}/Альбом/Мутанты.mp3");
    let destination = library.join(&relative_destination);
    let bytes = artist.as_bytes();
    let sha = format!("{:x}", Sha256::digest(bytes));
    let context = serde_json::json!({ "sha256": sha, "file_size": bytes.len() });
    let mut review = PendingReview::create(
        &db,
        0,
        "file",
        Some(relative_source),
        Some(&context.to_string()),
        None,
    )
    .await?;

    std::fs::create_dir_all(destination.parent().unwrap())?;
    for existing in [None, Some(b"another recording")] {
        if let Some(other_bytes) = existing {
            std::fs::write(&destination, other_bytes)?;
        }
        assert!(
            finalize_review_approval(&db, &pool, &config, &mut review, &fields, false)
                .await
                .is_err()
        );
        assert_eq!(
            PendingReview::get_by_id(&db, review.id_val())
                .await?
                .unwrap()
                .status_str(),
            "pending"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__media_file WHERE sha256_hash = $1")
                .bind(&sha)
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0);
    }
    assert_eq!(std::fs::read(&destination)?, b"another recording");

    std::fs::create_dir_all(source.parent().unwrap())?;
    std::fs::write(&source, bytes)?;
    let mut stale_review = review.clone();
    let result = serde_json::to_string(&fields)?;
    assert!(
        review
            .claim_for_processing(&pool, Some(result.clone()))
            .await?
    );
    assert!(!stale_review.claim_for_processing(&pool, None).await?);

    // Fail just this upload's catalog insert after the mover publishes its
    // collision-suffixed file. The next approval starts with an empty inbox.
    let constraint = format!("fail_insert_{}", &sha[..16]);
    sqlx::query(&format!(
        "ALTER TABLE furumusic__media_file ADD CONSTRAINT {constraint}
         CHECK (sha256_hash <> '{sha}') NOT VALID"
    ))
    .execute(&pool)
    .await?;
    let first_attempt =
        finalize_review_approval(&db, &pool, &config, &mut review, &fields, false).await;
    sqlx::query(&format!(
        "ALTER TABLE furumusic__media_file DROP CONSTRAINT {constraint}"
    ))
    .execute(&pool)
    .await?;
    let error = first_attempt.unwrap_err();
    assert!(format!("{error:#}").contains(&constraint), "{error:#}");
    assert!(!source.exists());
    let moved = std::fs::read_dir(destination.parent().unwrap())?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .find(|path| path != &destination)
        .unwrap();
    assert_eq!(std::fs::read(&moved)?, bytes);
    finalize_review_approval(&db, &pool, &config, &mut review, &fields, false).await?;
    let mut approved = PendingReview::get_by_id(&db, review.id_val())
        .await?
        .unwrap();
    assert_eq!(approved.status_str(), "approved");
    assert_eq!(approved.result_json_str(), result);
    assert!(!approved.claim_for_processing(&pool, None).await?);
    // A retry after the inbox file was consumed must keep a single playable track.
    finalize_review_approval(&db, &pool, &config, &mut review, &fields, false).await?;
    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT m.file_path FROM furumusic__track t JOIN furumusic__media_file m
         ON m.id = t.audio_file_id WHERE m.sha256_hash = $1",
    )
    .bind(&sha)
    .fetch_all(&pool)
    .await?;
    assert_eq!(paths, [moved.strip_prefix(&library)?.to_str().unwrap()]);
    assert_eq!(std::fs::read(&destination)?, b"another recording");
    assert_eq!(std::fs::read(&moved)?, bytes);
    assert!(!source.exists());

    // A recording with the same credits needs an explicit decision. Check
    // that the queue can retain both files when the admin chooses to do so.
    let second_input = format!("user_uploads/3/{artist}/second.mp3");
    let second = inbox.join(&second_input);
    std::fs::create_dir_all(second.parent().unwrap())?;
    let generated = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=1",
            "-metadata",
            &format!("title={artist}"),
        ])
        .arg(&second)
        .output()?;
    anyhow::ensure!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let second_bytes = std::fs::read(&second)?;
    let second_sha = format!("{:x}", Sha256::digest(&second_bytes));
    let second_context = serde_json::json!({"sha256": second_sha, "file_size": second_bytes.len()});
    let mut different = PendingReview::create(
        &db,
        0,
        "new_file",
        Some(&second_input),
        Some(&second_context.to_string()),
        Some(&result),
    )
    .await?;
    let error = finalize_review_approval(&db, &pool, &config, &mut different, &fields, false)
        .await
        .unwrap_err();
    assert!(error.is::<crate::agent::dedup::DuplicateReview>());
    different.set_approval_error(&db, &error).await?;
    let page = super::load_review_page(
        &pool,
        super::ReviewsQuery {
            status: Some("conflict".to_owned()),
            search: Some(second_input.clone()),
            limit: None,
            offset: None,
        },
    )
    .await?;
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].status, "conflict");
    assert_eq!(std::fs::read(&second)?, second_bytes);
    assert!(different.claim_for_processing(&pool, None).await?);
    finalize_review_approval(&db, &pool, &config, &mut different, &fields, true).await?;
    let second_path: String = sqlx::query_scalar(
        "SELECT m.file_path FROM furumusic__track t
        JOIN furumusic__media_file m ON m.id = t.audio_file_id WHERE m.sha256_hash=$1",
    )
    .bind(second_sha)
    .fetch_one(&pool)
    .await?;
    assert_eq!(std::fs::read(library.join(second_path))?, second_bytes);
    assert_eq!(std::fs::read(&moved)?, bytes);
    assert!(!second.exists());
    Ok(())
}
