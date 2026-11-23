use super::*;
use sqlx::Connection;

#[tokio::test]
async fn http_misses_throttling_and_invalid_bodies_are_distinguished() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (status, body, retry, success) in [
        ("200 OK", "[]", None, true),
        ("404 Not Found", "{}", None, false),
        ("429 Too Many Requests", "{}", Some(7200), false),
        ("503 Service Unavailable", "{}", Some(30), false),
        ("200 OK", "not JSON", None, false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{body}",
            body.len(),
            retry
                .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                .unwrap_or_default()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            stream.read(&mut bytes).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = get_json_url::<Vec<Record>>(&Client::new(), &url, &[]).await;
        assert_eq!(result.is_ok(), success, "{status}");
        if let Err(error) = result {
            assert_eq!(error.retry_after, retry);
        }
        server.await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires FURU_LYRICS_TEST_DATABASE_URL pointing at disposable PostgreSQL"]
async fn postgres_cache_retry_content_binding_and_manual_versions() {
    let url = std::env::var("FURU_LYRICS_TEST_DATABASE_URL").expect("disposable database URL");
    let mut admin = sqlx::PgConnection::connect(&url).await.unwrap();
    let schema = format!(
        "lyrics_test_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    );
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let search_path = format!("SET search_path TO {schema}");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let sql = search_path.clone();
            Box::pin(async move {
                sqlx::query(&sql).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(r#"
        CREATE TABLE furumusic__release(id BIGINT PRIMARY KEY,title TEXT,is_hidden BOOLEAN DEFAULT false);
        CREATE TABLE furumusic__artist(id BIGINT PRIMARY KEY,name TEXT);
        CREATE TABLE furumusic__media_file(id BIGINT PRIMARY KEY,sha256_hash TEXT);
        CREATE TABLE furumusic__track(id BIGINT PRIMARY KEY,title TEXT,release_id BIGINT,audio_file_id BIGINT,duration_seconds DOUBLE PRECISION,is_hidden BOOLEAN DEFAULT false);
        CREATE TABLE furumusic__track_artist(track_id BIGINT,artist_id BIGINT,role TEXT,position INTEGER);
        CREATE TABLE furumusic__federation_content_id_cache(media_file_id BIGINT,sha256_hash TEXT,content_id TEXT);
        INSERT INTO furumusic__release VALUES(1,'Album',false);
        INSERT INTO furumusic__artist VALUES(1,'Artist');
        INSERT INTO furumusic__media_file VALUES(1,'audio-a'),(2,'audio-b'),(3,'audio-a');
        INSERT INTO furumusic__track VALUES(1,'Song',1,1,100,false),(2,'Song',1,2,100,false),(3,'Song',1,3,100,false);
        INSERT INTO furumusic__track_artist VALUES(1,1,'main',0),(2,1,'main',0),(3,1,'main',0);
    "#).execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!("../lyrics/schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let candidates = || {
        sqlx::query_as::<_, Candidate>(CANDIDATES)
            .bind(200)
            .fetch_all(&pool)
    };
    let rows = candidates().await.unwrap();
    assert_eq!(rows.len(), 2, "identical bytes have one lookup binding");
    assert_eq!(
        rows[0].signature, rows[1].signature,
        "same metadata shares durable API cache"
    );
    let signature = rows[0].signature.clone();
    // Reproduce production's JSONB rejection of provider U+0000, through the
    // actual normalization, cache write and text attachment paths.
    let rejected = sqlx::query("SELECT $1::jsonb")
        .bind(serde_json::json!({"text":"Words\0here"}))
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        rejected.as_database_error().unwrap().code().as_deref(),
        Some("22P05")
    );
    let record:Record=serde_json::from_str(r#"{"id":9,"trackName":"Song","artistName":"Artist","albumName":"Al\u0000bum","duration":100,"plainLyrics":"Wo\u0000rds 🎵","syncedLyrics":"\u0000[00:01]Words 🎵"}"#).unwrap();
    let cleaned = version(record).unwrap();
    sqlx::query("INSERT INTO furumusic__lyrics_lookup(signature,status,versions) VALUES('_nul_fixture','found',$1)")
        .bind(serde_json::to_value(vec![&cleaned]).unwrap()).execute(&pool).await.unwrap();
    attach(&pool, &rows[0], std::slice::from_ref(&cleaned))
        .await
        .unwrap();
    assert_eq!(
        crate::lyrics::versions(&pool, &rows[0].hash).await.unwrap(),
        vec![cleaned]
    );
    sqlx::query("INSERT INTO furumusic__lyrics_lookup(signature,status) VALUES($1,'not_found')")
        .bind(&signature)
        .execute(&pool)
        .await
        .unwrap();
    for track in &rows {
        attach(&pool, track, &[]).await.unwrap();
    }
    assert!(
        candidates().await.unwrap().is_empty(),
        "a miss is never looked up again"
    );
    save_retry(&pool, &signature, 120_000).await.unwrap();
    save_retry(&pool, &signature, 120_000).await.unwrap();
    let seconds:f64=sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM retry_at-now())::float8 FROM furumusic__lyrics_lookup WHERE signature=$1").bind(&signature).fetch_one(&pool).await.unwrap();
    assert!(
        seconds >= 120_000.0,
        "Retry-After must not be capped by exponential backoff"
    );
    sqlx::query(
        "UPDATE furumusic__lyrics_lookup SET status='retry',retry_at=now()+interval '1 hour'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        candidates().await.unwrap().is_empty(),
        "temporary failure honors cooldown"
    );
    sqlx::query("UPDATE furumusic__lyrics_lookup SET retry_at=now()-interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        candidates().await.unwrap().len(),
        2,
        "expired failures are retried"
    );
    let automatic = LyricsVersion {
        id: "lrclib:1".into(),
        label: "Original".into(),
        source: "lrclib".into(),
        source_id: Some("1".into()),
        language: None,
        plain_text: Some("Original words\n".into()),
        synced_lrc: Some("[00:01]Original words".into()),
        instrumental: false,
    };
    attach(&pool, &rows[0], std::slice::from_ref(&automatic))
        .await
        .unwrap();
    let duplicate = crate::lyrics::for_track(&pool, 3, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(duplicate.versions, vec![automatic.clone()]);
    assert_eq!(
        crate::lyrics::available_track_ids(&pool, &[1, 2, 3])
            .await
            .unwrap()
            .len(),
        2
    );
    // Saving unrelated metadata keeps external provenance; an edited version becomes manual.
    crate::lyrics::edit(
        &pool,
        1,
        &crate::lyrics::LyricsEdit {
            versions: vec![automatic.clone()],
            retry_lookup: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        crate::lyrics::versions(&pool, "audio-a").await.unwrap()[0].source,
        "lrclib"
    );
    let mut manual = automatic.clone();
    manual.plain_text = Some("Edited locally".into());
    crate::lyrics::edit(
        &pool,
        1,
        &crate::lyrics::LyricsEdit {
            versions: vec![manual],
            retry_lookup: false,
        },
    )
    .await
    .unwrap();
    attach(&pool, &rows[0], std::slice::from_ref(&automatic))
        .await
        .unwrap();
    let stored = crate::lyrics::versions(&pool, "audio-a").await.unwrap();
    assert_eq!(stored[0].source, "manual");
    assert_eq!(stored[0].plain_text.as_deref(), Some("Edited locally"));
    sqlx::query("UPDATE furumusic__lyrics_lookup SET status='found',retry_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert!(candidates().await.unwrap().is_empty());
    sqlx::query("UPDATE furumusic__artist SET name='Renamed'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        candidates().await.unwrap().len(),
        2,
        "metadata edits invalidate previous lookup signatures"
    );
    // Manual repeat invalidates the shared lookup, without deleting saved lyrics.
    crate::lyrics::edit(
        &pool,
        1,
        &crate::lyrics::LyricsEdit {
            versions: stored,
            retry_lookup: true,
        },
    )
    .await
    .unwrap();
    assert!(
        !crate::lyrics::versions(&pool, "audio-a")
            .await
            .unwrap()
            .is_empty()
    );
    // Visibility is enforced independently of whether lyrics exist.
    sqlx::query("UPDATE furumusic__track SET is_hidden=true WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        crate::lyrics::for_track(&pool, 1, true)
            .await
            .unwrap()
            .is_none()
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut admin)
        .await
        .unwrap();
}
