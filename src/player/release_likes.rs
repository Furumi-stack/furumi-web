use sqlx::PgPool;

pub(super) async fn toggle(
    pool: &PgPool,
    user: i64,
    release: i64,
) -> anyhow::Result<(bool, Vec<i64>)> {
    let mut tx = pool.begin().await?;
    crate::federation::devices::likes::lock(&mut tx, user).await?;
    let recordings = sqlx::query_as::<_, (i64, Vec<i64>, bool)>(include_str!("release_likes.sql"))
        .bind(user)
        .bind(release)
        .fetch_all(&mut *tx)
        .await?;
    let liked = recordings.is_empty()
        || recordings
            .iter()
            .any(|(_, ids, remote)| ids.is_empty() && !remote);
    let changed: Vec<i64>;
    if liked {
        changed = recordings
            .into_iter()
            .filter(|(_, ids, remote)| ids.is_empty() && !remote)
            .map(|(id, _, _)| id)
            .collect();
        sqlx::query(
            "INSERT INTO furumusic__user_liked_track (user_id, track_id, created_at)
             SELECT $1, id, $3 FROM unnest($2::bigint[]) AS id
             ON CONFLICT (user_id, track_id) DO NOTHING",
        )
        .bind(user)
        .bind(&changed)
        .bind(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .execute(&mut *tx)
        .await?;
    } else {
        let stored: Vec<i64> = recordings
            .iter()
            .flat_map(|(_, ids, _)| ids.iter().copied())
            .collect();
        sqlx::query(
            "DELETE FROM furumusic__user_liked_track WHERE user_id=$1 AND track_id=ANY($2)",
        )
        .bind(user)
        .bind(stored)
        .execute(&mut *tx)
        .await?;
        changed = recordings.into_iter().map(|(id, _, _)| id).collect();
    }
    tx.commit().await?;
    Ok((liked, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
    async fn release_likes_keep_one_copy_and_light_up_each_alias() -> anyhow::Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?)
            .await?;
        sqlx::raw_sql("
            CREATE TEMP TABLE furumusic__track(id bigint, release_id bigint, is_hidden boolean DEFAULT false);
            CREATE TEMP TABLE furumusic__track_content_alias(track_id bigint, content_id text, hidden boolean DEFAULT false);
            CREATE TEMP TABLE furumusic__user_liked_track(user_id bigint, track_id bigint, created_at text,
                PRIMARY KEY(user_id, track_id));
            CREATE TEMP TABLE furumusic__fed_state_like(user_id bigint, content_id text, liked boolean, local_track_id bigint);
            INSERT INTO furumusic__track(id, release_id) VALUES (1,10),(2,10),(3,10),(4,10),(5,20),(6,30),(7,40);
            INSERT INTO furumusic__track_content_alias(track_id, content_id)
                VALUES (1,'album'),(2,'album'),(3,'third'),(5,'album'),(6,'single'),(7,'pending');
            INSERT INTO furumusic__user_liked_track VALUES (9,5,'original'),(9,6,'single'),(10,1,'other user');
            INSERT INTO furumusic__fed_state_like VALUES (9,'pending',true,NULL);
        ").execute(&pool).await?;

        let (liked, mut added) = toggle(&pool, 9, 10).await?;
        added.sort_unstable();
        assert!(liked);
        assert_eq!(added, [3, 4]); // Preserve the original copy from release 20.
        let mut ids = sqlx::query_scalar::<_, i64>(include_str!("liked_ids.sql"))
            .bind(9_i64)
            .fetch_all(&pool)
            .await?;
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7]);
        let (liked, mut removed) = toggle(&pool, 9, 10).await?;
        removed.sort_unstable();
        assert!(!liked);
        assert_eq!(removed, [1, 3, 4]);
        let stored: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT user_id, track_id FROM furumusic__user_liked_track ORDER BY user_id, track_id",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(stored, [(9, 6), (10, 1)]);
        assert_eq!(toggle(&pool, 9, 40).await?, (false, vec![7]));
        Ok(())
    }
}
