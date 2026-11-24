use super::*;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database in FURU_IMPORT_TEST_DATABASE_URL"]
async fn batches_group_before_pagination_and_hiding_preserves_imports() -> anyhow::Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("FURU_IMPORT_TEST_DATABASE_URL")?)
        .await?;
    // Isolate the complete migration (which creates permanent relations) in
    // this test's own schema on a single connection.
    let schema = format!("history_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(&format!(
        "CREATE SCHEMA {schema}; SET search_path TO {schema};"
    ))
    .execute(&pool)
    .await?;
    sqlx::raw_sql(r#"
        CREATE TABLE furumusic__local_upload (id text PRIMARY KEY,user_id bigint,filename text,
            inbox_path text,status text,error text,created_at text DEFAULT '01',updated_at text DEFAULT '01');
        CREATE TABLE furumusic__youtube_download (id text PRIMARY KEY,user_id bigint,title text,source_url text,
            total_items integer,status text,error text,created_at text DEFAULT '02',updated_at text DEFAULT '02');
        CREATE TABLE furumusic__youtube_download_item(job_id text, progress_percent double precision);
        CREATE TABLE furumusic__torrent_session(id text PRIMARY KEY,user_id bigint,name text,selected_files_json text,files_json text,
            status text, progress_percent double precision,error text,created_at text DEFAULT '03',updated_at text DEFAULT '03');
        CREATE TABLE furumusic__pending_review(input_path text,status text,error_message text,id bigserial PRIMARY KEY);
        CREATE TABLE furumusic__media_file(id bigint,uploaded_by_user_id bigint);
        CREATE TABLE furumusic__track(audio_file_id bigint,release_id bigint);
        INSERT INTO furumusic__media_file VALUES (1,1),(2,1),(3,2);
        INSERT INTO furumusic__track VALUES (1,10),(2,10),(3,20);
        INSERT INTO furumusic__youtube_download(id,user_id,title,total_items,status) VALUES ('same',1,'Mix',2,'downloading');
        INSERT INTO furumusic__youtube_download_item VALUES ('same',100),('same',20);
        INSERT INTO furumusic__torrent_session(id,user_id,name,selected_files_json,files_json,status)
            VALUES ('same',1,'Album','[1]','[1,2,3]','complete'),('other',2,'Other','[]','[5,6]','complete');
        INSERT INTO furumusic__pending_review VALUES ('user_uploads/1/torrent-same/Album/a.flac','processing',null);
        INSERT INTO furumusic__local_upload(id,user_id,filename,inbox_path,status)
            SELECT i::text,1,'Disc/'||i||'.mp3','user_uploads/1/local-12345678-1234-1234-1234-123456789abc/Disc/'||i||'.mp3',
                   CASE WHEN i=105 THEN 'failed' ELSE 'complete' END FROM generate_series(1,105) AS i;
        INSERT INTO furumusic__local_upload(id,user_id,filename,inbox_path,status)
            VALUES ('legacy',1,'Song.mp3','older/location/song.mp3','complete'),
                   ('second-user',2,'Own.mp3','user_uploads/2/local-12345678-1234-1234-1234-123456789abc/Own.mp3','complete');
    "#).execute(&pool).await?;
    sqlx::raw_sql(include_str!("download_history_schema.sql"))
        .execute(&pool)
        .await?;
    sqlx::raw_sql(include_str!("../scheduler/review_dismissal_schema.sql"))
        .execute(&pool)
        .await?;
    let batch = "12345678-1234-1234-1234-123456789abc";
    let key_list = keys(&pool, 1, 0, 100, None).await?;
    assert_eq!(key_list.len(), 4);
    assert_eq!(keys(&pool, 1, 1, 2, None).await?, key_list[1..3]);
    assert_eq!(
        keys(&pool, 1, 0, 1, Some((&Source::Youtube, "same"))).await?,
        vec![("youtube".into(), "same".into())]
    );
    let (sources, ids): (Vec<_>, Vec<_>) = key_list.into_iter().unzip();
    let rows = summaries(&pool, 1, &sources, &ids).await?;
    assert_eq!(
        (
            rows[0].source.as_str(),
            rows[0].status.as_str(),
            rows[0].item_count
        ),
        ("torrents", "ai_processing", 1)
    );
    assert_eq!(rows[1].progress_percent, Some(60.0));
    let files = rows.iter().find(|r| r.id == batch).unwrap();
    assert_eq!(
        (
            files.item_count,
            files.title.as_deref(),
            files.status.as_str()
        ),
        (105, Some("Disc"), "complete_with_errors")
    );
    let selection = vec![
        Key {
            source: Source::Files,
            id: batch.into(),
        },
        Key {
            source: Source::Youtube,
            id: "same".into(),
        },
        Key {
            source: Source::Torrents,
            id: "other".into(),
        },
    ];
    hide(&pool, 1, &selection).await?;
    hide(&pool, 1, &selection).await?;
    assert_eq!(
        keys(&pool, 1, 0, 100, None).await?,
        vec![
            ("torrents".into(), "same".into()),
            ("files".into(), "legacy".into())
        ]
    );
    assert_eq!(keys(&pool, 2, 0, 100, None).await?.len(), 2);
    assert!(
        keys(&pool, 1, 0, 1, Some((&Source::Youtube, "same")))
            .await?
            .is_empty()
    );
    let stats = stats(&pool, 1).await?;
    assert_eq!((stats.tracks, stats.releases), (2, 1));
    let counts:(i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM furumusic__local_upload),
        (SELECT count(*) FROM furumusic__youtube_download_item),(SELECT count(*) FROM furumusic__hidden_download)")
        .fetch_one(&pool).await?;
    assert_eq!(counts, (107, 2, 2));
    // A later receipt for a still-running hidden batch must not revive its history.
    sqlx::query("INSERT INTO furumusic__local_upload(id,user_id,filename,inbox_path,status)
        VALUES ('late',1,'last.flac','user_uploads/1/local-12345678-1234-1234-1234-123456789abc/last.flac','queued')")
        .execute(&pool).await?;
    assert_eq!(keys(&pool, 1, 0, 100, None).await?.len(), 2);

    // Check the boundary between a finished torrent transfer and finished import.
    sqlx::query("DELETE FROM furumusic__pending_review")
        .execute(&pool)
        .await?;
    let inbox = tempfile::tempdir()?;
    let folder = inbox.path().join("user_uploads/1/torrent-same/Album");
    tokio::fs::create_dir_all(&folder).await?;
    tokio::fs::write(folder.join("track.flac"), b"audio").await?;
    let service = TorrentService::new(Arc::new(OnceCell::new()));
    let mut entry = summaries(&pool, 1, &["torrents".into()], &["same".into()])
        .await?
        .remove(0);
    torrent_status(&pool, 1, Some(inbox.path()), &service, &mut entry).await?;
    assert_eq!(entry.status, "awaiting_ai");
    tokio::fs::remove_file(folder.join("track.flac")).await?;
    let mut entry = summaries(&pool, 1, &["torrents".into()], &["same".into()])
        .await?
        .remove(0);
    torrent_status(&pool, 1, Some(inbox.path()), &service, &mut entry).await?;
    assert_eq!(entry.status, "complete");
    assert!(summaries(&pool, 1, &["torrents".into()], &["same".into()]).await?[0].processed);
    // An unimported file dismissed in the inbox must not count as an import.
    let other_file = "user_uploads/2/torrent-other/Album/removed.mp3";
    sqlx::query("INSERT INTO furumusic__dismissed_review VALUES ($1, repeat('a',64), 70, false)")
        .bind(other_file)
        .execute(&pool)
        .await?;
    let torrent_sources = ["torrents".into()];
    let other_id = ["other".into()];
    assert_eq!(
        summaries(&pool, 2, &torrent_sources, &other_id).await?[0].status,
        "cancelled"
    );
    sqlx::query(
        "INSERT INTO furumusic__pending_review(input_path,status,id) VALUES ($1,'queued',71)",
    )
    .bind(other_file)
    .execute(&pool)
    .await?;
    assert_eq!(
        summaries(&pool, 2, &torrent_sources, &other_id).await?[0].status,
        "awaiting_ai"
    );
    sqlx::raw_sql(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await?;
    Ok(())
}
