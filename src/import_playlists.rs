//! Durable playlist destinations for file and torrent imports.
use anyhow::{Context, bail};
use sqlx::PgPool;

pub async fn validate_target(
    pool: &PgPool,
    user_id: i64,
    playlist_id: Option<i64>,
) -> anyhow::Result<()> {
    if let Some(id) = playlist_id {
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM furumusic__playlist WHERE id=$1 AND owner_id=$2)",
        )
        .bind(id)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
        if id <= 0 || !owned {
            bail!("selected playlist does not exist or is not yours");
        }
    }
    Ok(())
}

pub async fn register(
    pool: &PgPool,
    user_id: i64,
    playlist_id: i64,
    input_path: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    validate_target(pool, user_id, Some(playlist_id)).await?;
    let result = sqlx::query(
        r#"INSERT INTO furumusic__import_playlist_target
        (user_id, playlist_id, input_path, source_sha256)
        VALUES ($1,$2,$3,$4)
        ON CONFLICT (user_id, input_path) DO UPDATE SET source_sha256=EXCLUDED.source_sha256
        WHERE furumusic__import_playlist_target.playlist_id=EXCLUDED.playlist_id
          AND furumusic__import_playlist_target.source_sha256=EXCLUDED.source_sha256"#,
    )
    .bind(user_id)
    .bind(playlist_id)
    .bind(input_path)
    .bind(sha256)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        bail!("this upload already has a different playlist destination");
    }
    Ok(())
}

/// Fingerprint deduplication may resolve a differently encoded file to an existing media ID.
pub async fn link_media(pool: &PgPool, source_sha256: &str, media_id: i64) -> anyhow::Result<()> {
    if source_sha256.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE furumusic__import_playlist_target SET media_file_id=$2 WHERE source_sha256=$1 AND fulfilled_at IS NULL")
        .bind(source_sha256).bind(media_id).execute(pool).await?;
    reconcile(pool, None).await
}

/// Runs after imports and from background discovery, including after a restart.
/// Fulfilled requests are retained as receipts so later user removals stay removed.
pub async fn reconcile(pool: &PgPool, user_id: Option<i64>) -> anyhow::Result<()> {
    let requests: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT user_id,playlist_id,input_path FROM furumusic__import_playlist_target WHERE fulfilled_at IS NULL AND ($1::bigint IS NULL OR user_id=$1) ORDER BY user_id,playlist_id,input_path"
    ).bind(user_id).fetch_all(pool).await?;
    for (user_id, playlist_id, path) in requests {
        let mut transaction = pool.begin().await?;
        let pending: Option<String> = sqlx::query_scalar(
            "SELECT input_path FROM furumusic__import_playlist_target WHERE user_id=$1 AND input_path=$2 AND fulfilled_at IS NULL FOR UPDATE"
        ).bind(user_id).bind(&path).fetch_optional(&mut *transaction).await?;
        if pending.is_none() {
            continue;
        }
        let track_ids: Vec<i64> = sqlx::query_scalar(r#"
            SELECT DISTINCT track.id FROM furumusic__import_playlist_target request
            JOIN furumusic__media_file media ON media.id=request.media_file_id
                OR (request.media_file_id IS NULL AND media.sha256_hash=request.source_sha256 AND media.file_type='audio')
            JOIN furumusic__track track ON track.audio_file_id=media.id
            WHERE request.user_id=$1 AND request.input_path=$2 ORDER BY track.id
        "#).bind(user_id).bind(&path).fetch_all(&mut *transaction).await?;
        if track_ids.is_empty() {
            continue;
        }
        let mut added = Vec::new();
        for track_id in track_ids {
            if append_in_transaction(&mut transaction, user_id, playlist_id, track_id).await? {
                added.push(track_id);
            }
        }
        sqlx::query("UPDATE furumusic__import_playlist_target SET fulfilled_at=$3 WHERE user_id=$1 AND input_path=$2")
            .bind(user_id).bind(path).bind(now_string()).execute(&mut *transaction).await?;
        transaction.commit().await?;
        record_added(pool, user_id, playlist_id, &added).await;
    }
    Ok(())
}

fn now_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) async fn append_track_to_target_playlist(
    pool: &PgPool,
    user_id: i64,
    playlist_id: i64,
    track_id: i64,
) -> anyhow::Result<bool> {
    let mut transaction = pool.begin().await?;
    let added = append_in_transaction(&mut transaction, user_id, playlist_id, track_id).await?;
    transaction.commit().await?;
    if added {
        record_added(pool, user_id, playlist_id, &[track_id]).await;
    }
    Ok(added)
}

async fn append_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    playlist_id: i64,
    track_id: i64,
) -> anyhow::Result<bool> {
    let playlist: Option<i64> = sqlx::query_scalar(
        r#"SELECT id FROM furumusic__playlist
           WHERE id = $1 AND owner_id = $2
           FOR UPDATE"#,
    )
    .bind(playlist_id)
    .bind(user_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if playlist.is_none() {
        return Ok(false);
    }

    let already_present: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1 FROM furumusic__playlist_track
               WHERE playlist_id = $1 AND track_id = $2
           )"#,
    )
    .bind(playlist_id)
    .bind(track_id)
    .fetch_one(&mut **transaction)
    .await?;
    if already_present {
        return Ok(false);
    }

    let position: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(MAX(position), -1)::bigint + 1
           FROM furumusic__playlist_track WHERE playlist_id = $1"#,
    )
    .bind(playlist_id)
    .fetch_one(&mut **transaction)
    .await?;
    let position = i32::try_from(position).context("target playlist has too many tracks")?;
    let now = now_string();
    sqlx::query(
        r#"INSERT INTO furumusic__playlist_track
              (playlist_id, track_id, position, added_at, added_by_user_id)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(playlist_id)
    .bind(track_id)
    .bind(position)
    .bind(&now)
    .bind(user_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query("UPDATE furumusic__playlist SET updated_at = $1 WHERE id = $2")
        .bind(&now)
        .bind(playlist_id)
        .execute(&mut **transaction)
        .await?;
    Ok(true)
}

async fn record_added(pool: &PgPool, user_id: i64, playlist_id: i64, track_ids: &[i64]) {
    if track_ids.is_empty() {
        return;
    }
    if let Err(error) = crate::federation::devices::record_playlist_tracks_added(
        pool,
        user_id,
        playlist_id,
        track_ids,
    )
    .await
    {
        tracing::warn!(playlist_id, %error, "federation operation for an automatic import playlist addition was not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL in FURU_IMPORT_TEST_DATABASE_URL"]
    async fn import_destinations_are_durable_owned_and_idempotent() {
        let url = std::env::var("FURU_IMPORT_TEST_DATABASE_URL").unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql(r#"
            CREATE TEMP TABLE furumusic__playlist(id bigint PRIMARY KEY,owner_id bigint,updated_at text);
            CREATE TEMP TABLE furumusic__media_file(id bigint PRIMARY KEY,sha256_hash text,file_type text);
            CREATE TEMP TABLE furumusic__track(id bigint PRIMARY KEY,audio_file_id bigint);
            CREATE TEMP TABLE furumusic__playlist_track(playlist_id bigint,track_id bigint,position int,added_at text,added_by_user_id bigint);
            CREATE TEMP TABLE furumusic__import_playlist_target(user_id bigint,playlist_id bigint REFERENCES furumusic__playlist ON DELETE CASCADE,input_path text,source_sha256 text,media_file_id bigint,fulfilled_at text,PRIMARY KEY(user_id,input_path));
            INSERT INTO furumusic__playlist VALUES (10,1,''),(20,2,'');
        "#).execute(&pool).await.unwrap();
        assert!(validate_target(&pool, 1, Some(20)).await.is_err());
        assert!(validate_target(&pool, 1, Some(999)).await.is_err());
        validate_target(&pool, 1, None).await.unwrap();
        register(&pool, 1, 10, "batch/a.flac", "hash-a")
            .await
            .unwrap();
        register(&pool, 1, 10, "batch/a.flac", "hash-a")
            .await
            .unwrap();
        assert!(
            register(&pool, 1, 10, "batch/a.flac", "changed-hash")
                .await
                .is_err()
        );
        reconcile(&pool, None).await.unwrap();
        let count = || {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM furumusic__playlist_track")
                .fetch_one(&pool)
        };
        assert_eq!(count().await.unwrap(), 0);
        sqlx::raw_sql("INSERT INTO furumusic__media_file VALUES (100,'hash-a','audio'); INSERT INTO furumusic__track VALUES (101,100),(102,100);").execute(&pool).await.unwrap();
        reconcile(&pool, Some(2)).await.unwrap();
        assert_eq!(count().await.unwrap(), 0);
        reconcile(&pool, Some(1)).await.unwrap();
        assert_eq!(count().await.unwrap(), 2);
        // A second upload of identical audio must not append duplicates.
        register(&pool, 1, 10, "torrent/b.flac", "hash-a")
            .await
            .unwrap();
        reconcile(&pool, None).await.unwrap();
        assert_eq!(count().await.unwrap(), 2);
        sqlx::query("DELETE FROM furumusic__playlist_track WHERE track_id=101")
            .execute(&pool)
            .await
            .unwrap();
        reconcile(&pool, None).await.unwrap();
        assert_eq!(count().await.unwrap(), 1);
        // Fingerprint matching can map a differently encoded upload to the same media.
        register(&pool, 2, 20, "batch/a.mp3", "different-encoding")
            .await
            .unwrap();
        link_media(&pool, "different-encoding", 100).await.unwrap();
        assert_eq!(count().await.unwrap(), 3);
        let positions: Vec<i32> = sqlx::query_scalar(
            "SELECT position FROM furumusic__playlist_track WHERE playlist_id=20 ORDER BY position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(positions, vec![0, 1]);
        // Deleting a destination removes pending work without blocking future imports.
        register(&pool, 2, 20, "batch/pending.wav", "pending")
            .await
            .unwrap();
        sqlx::query("DELETE FROM furumusic__playlist WHERE id=20")
            .execute(&pool)
            .await
            .unwrap();
        reconcile(&pool, None).await.unwrap();
        pool.close().await;
    }
}
