use sqlx::{PgConnection, PgPool};

pub(crate) async fn lock(conn: &mut PgConnection, user: i64) -> sqlx::Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('furumusic-like:' || $1::bigint::text,0))",
    )
    .bind(user)
    .execute(conn)
    .await?;
    Ok(())
}

pub(super) struct Aliases {
    ids: Vec<i64>,
    visible: Vec<i64>,
}

impl Aliases {
    pub(super) async fn load(conn: &mut PgConnection, content: &str) -> sqlx::Result<Self> {
        let rows: Vec<(i64,bool)> = sqlx::query_as(
            "SELECT track_id, hidden FROM furumusic__track_content_alias WHERE content_id=$1 ORDER BY track_id"
        ).bind(content).fetch_all(conn).await?;
        Ok(Self {
            ids: rows.iter().map(|r| r.0).collect(),
            visible: rows.into_iter().filter(|r| !r.1).map(|r| r.0).collect(),
        })
    }

    pub(super) async fn existing(
        &self,
        conn: &mut PgConnection,
        user: i64,
    ) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar(
            "SELECT track_id FROM furumusic__user_liked_track WHERE user_id=$1 AND track_id=ANY($2)
             ORDER BY (track_id=ANY($3)) DESC, id LIMIT 1",
        )
        .bind(user)
        .bind(&self.ids)
        .bind(&self.visible)
        .fetch_optional(conn)
        .await
    }

    // Only one local row represents a signed, content-addressed like. Preserve
    // the existing visible edition, including the one the local listener chose.
    pub(super) async fn project(
        &self,
        conn: &mut PgConnection,
        user: i64,
        liked: bool,
        preferred: Option<i64>,
        created_at: &str,
    ) -> sqlx::Result<Option<i64>> {
        let existing = self.existing(conn, user).await?;
        let chosen = existing
            .filter(|id| self.visible.contains(id))
            .or_else(|| preferred.filter(|id| self.visible.contains(id)))
            .or_else(|| self.visible.first().copied());
        sqlx::query(
            "DELETE FROM furumusic__user_liked_track WHERE user_id=$1 AND track_id=ANY($2)
             AND track_id IS DISTINCT FROM $3",
        )
        .bind(user)
        .bind(&self.ids)
        .bind(if liked { chosen } else { None })
        .execute(&mut *conn)
        .await?;
        if let Some(id) = chosen.filter(|_| liked) {
            sqlx::query(
                "INSERT INTO furumusic__user_liked_track(user_id,track_id,created_at) VALUES($1,$2,$3)
                 ON CONFLICT(user_id,track_id) DO NOTHING"
            ).bind(user).bind(id).bind(created_at).execute(conn).await?;
        }
        Ok(chosen)
    }
}

pub(crate) async fn toggle(
    pool: &PgPool,
    user: i64,
    track: i64,
) -> sqlx::Result<Option<(bool, Option<String>, Vec<i64>)>> {
    let mut tx = pool.begin().await?;
    lock(&mut tx, user).await?;
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT a.content_id FROM furumusic__track t LEFT JOIN furumusic__track_content_alias a ON a.track_id=t.id
         WHERE t.id=$1"
    ).bind(track).fetch_optional(&mut *tx).await?;
    let Some((content,)) = row else {
        return Ok(None);
    };
    let aliases = match &content {
        Some(content) => Aliases::load(&mut tx, content).await?,
        None => Aliases {
            ids: vec![track],
            visible: vec![track],
        },
    };
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM furumusic__fed_state_like
                       WHERE user_id=$1 AND content_id=$2 AND liked AND local_track_id IS NULL)",
    )
    .bind(user)
    .bind(content.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    let liked = !pending && aliases.existing(&mut tx, user).await?.is_none();
    if liked && aliases.visible.is_empty() {
        return Ok(None);
    }
    aliases
        .project(&mut tx, user, liked, Some(track), &super::now_iso())
        .await?;
    tx.commit().await?;
    Ok(Some((liked, content, aliases.ids)))
}

pub(crate) async fn on_content_available(
    conn: &mut PgConnection,
    content: &str,
    preferred: Option<i64>,
) -> sqlx::Result<()> {
    let users = sqlx::query_scalar::<_, i64>(
        "SELECT user_id FROM furumusic__fed_state_like WHERE content_id=$1
         UNION
         SELECT l.user_id FROM furumusic__track_content_alias a
         JOIN furumusic__user_liked_track l ON l.track_id=a.track_id
         WHERE a.content_id=$1 ORDER BY user_id",
    )
    .bind(content)
    .fetch_all(&mut *conn)
    .await?;
    if users.is_empty() {
        return Ok(());
    }
    for user in users {
        lock(conn, user).await?;
        let aliases = Aliases::load(conn, content).await?;
        let state: Option<(bool, i64)> = sqlx::query_as(
            "SELECT liked, hlc_ms FROM furumusic__fed_state_like WHERE user_id=$1 AND content_id=$2"
        ).bind(user).bind(content).fetch_optional(&mut *conn).await?;
        if let Some((liked, ms)) = state {
            let chosen = aliases
                .project(conn, user, liked, preferred, &super::iso_from_ms(ms))
                .await?;
            sqlx::query("UPDATE furumusic__fed_state_like SET local_track_id=$3 WHERE user_id=$1 AND content_id=$2")
                .bind(user).bind(content).bind(chosen).execute(&mut *conn).await?;
        } else if aliases.existing(conn, user).await?.is_some() {
            aliases
                .project(conn, user, true, preferred, &super::now_iso())
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::devices::{apply_like_state, record_track_like};

    async fn local_rows(pool: &PgPool) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar(
            "SELECT track_id FROM furumusic__user_liked_track WHERE user_id=1 ORDER BY track_id",
        )
        .fetch_all(pool)
        .await
    }

    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
    async fn catalog_aliases_share_one_like_across_web_sync_and_import() -> anyhow::Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?)
            .await?;
        sqlx::raw_sql("
            CREATE TEMP TABLE furumusic__track(id bigint PRIMARY KEY,title text DEFAULT 'Taking Off',
                release_id bigint DEFAULT 1,audio_file_id bigint,is_hidden bool DEFAULT false,
                track_number int,disc_number int,duration_seconds float8 DEFAULT 220);
            CREATE TEMP TABLE furumusic__release(id bigint PRIMARY KEY,title text,year int,is_hidden bool DEFAULT false);
            CREATE TEMP TABLE furumusic__media_file(id bigint PRIMARY KEY,sha256_hash text);
            CREATE TEMP TABLE furumusic__federation_content_id_cache(media_file_id bigint PRIMARY KEY,sha256_hash text,content_id text,updated_at text);
            CREATE TEMP TABLE furumusic__track_artist(track_id bigint,artist_id bigint,role text,position int);
            CREATE TEMP TABLE furumusic__artist(id bigint,name text);
            CREATE TEMP TABLE furumusic__user_liked_track(id bigserial PRIMARY KEY,user_id bigint,track_id bigint,created_at text,UNIQUE(user_id,track_id));
            CREATE TEMP TABLE furumusic__fed_state_like(user_id bigint,content_id text,liked bool,hlc_ms bigint,op_id text,local_track_id bigint,fed_json jsonb,PRIMARY KEY(user_id,content_id));
            CREATE TEMP TABLE furumusic__fed_device_identity(user_id bigint,device_id text,group_id text,device_name text,local_seeded_at_ms bigint,local_seq bigint,last_hlc_ms bigint);
            CREATE TEMP TABLE furumusic__fed_sync_ops(user_id bigint,op_id text,origin_device_id text,seq bigint,kind text,payload_json jsonb,hlc_ms bigint,received_at_ms bigint,tombstone bool,UNIQUE(user_id,op_id));
            CREATE TEMP TABLE furumusic__fed_sync_vector(user_id bigint,device_id text,max_seq bigint,UNIQUE(user_id,device_id));
            INSERT INTO furumusic__fed_device_identity VALUES(1,'web','g','web',1,0,0);
            INSERT INTO furumusic__release(id,title) VALUES(1,'Ambitions');
            INSERT INTO furumusic__track(id,audio_file_id) VALUES(5672,1),(5686,2),(5797,3),(50,4);
            INSERT INTO furumusic__media_file VALUES(1,'one'),(2,'two'),(3,'three'),(4,'new');
            INSERT INTO furumusic__federation_content_id_cache(media_file_id,sha256_hash,content_id) VALUES
                (1,'one','b3:'||repeat('a',64)),(2,'two','b3:'||repeat('a',64)),
                (3,'three','b3:'||repeat('b',64)),(4,'stale','b3:'||repeat('a',64));
        ").execute(&pool).await?;
        sqlx::raw_sql(&include_str!("track_content_alias.sql").replacen(
            "CREATE VIEW",
            "CREATE TEMP VIEW",
            1,
        ))
        .execute(&pool)
        .await?;
        let content = format!("b3:{}", "a".repeat(64));
        let local = toggle(&pool, 1, 5686).await?.unwrap();
        assert_eq!(local, (true, Some(content.clone()), vec![5672, 5686]));
        record_track_like(&pool, 1, 5686, true).await?;
        assert_eq!(local_rows(&pool).await?, [5686]);
        assert!(toggle(&pool, 1, 5797).await?.unwrap().0);
        record_track_like(&pool, 1, 5797, true).await?;
        let uncached = toggle(&pool, 1, 50).await?.unwrap();
        assert_eq!(uncached, (true, None, vec![50]));
        let mut tx = pool.begin().await?;
        on_content_available(&mut tx, &content, Some(5672)).await?;
        tx.commit().await?;
        assert_eq!(local_rows(&pool).await?, [50, 5686, 5797]);
        assert!(!toggle(&pool, 1, 5672).await?.unwrap().0);
        record_track_like(&pool, 1, 5672, false).await?;
        record_track_like(&pool, 1, 5686, true).await?; // A delayed callback cannot restore it.
        assert_eq!(local_rows(&pool).await?, [50, 5797]);
        assert!(apply_like_state(&pool, 1, &content, true, None, i64::MAX - 1, "peer:1").await?);
        assert!(!apply_like_state(&pool, 1, &content, false, None, 1, "old").await?);
        assert_eq!(local_rows(&pool).await?, [50, 5672, 5797]);
        assert!(apply_like_state(&pool, 1, &content, false, None, i64::MAX, "peer:2").await?);
        assert_eq!(local_rows(&pool).await?, [50, 5797]);

        // Restore the legacy double insert, including the user's original, higher track ID.
        sqlx::raw_sql(
            "INSERT INTO furumusic__user_liked_track(user_id,track_id,created_at)
                       VALUES(1,5686,'first'),(1,5672,'second'),(2,5672,'other user')",
        )
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!("repair_like_aliases.sql"))
            .execute(&pool)
            .await?;
        assert_eq!(local_rows(&pool).await?, [50, 5686, 5797]);
        let other: i64 =
            sqlx::query_scalar("SELECT track_id FROM furumusic__user_liked_track WHERE user_id=2")
                .fetch_one(&pool)
                .await?;
        assert_eq!(other, 5672);
        sqlx::raw_sql(
            "INSERT INTO furumusic__media_file VALUES(5,'five');
                       INSERT INTO furumusic__track(id,audio_file_id) VALUES(60,5)",
        )
        .execute(&pool)
        .await?;
        assert!(toggle(&pool, 1, 60).await?.unwrap().0);
        let late_hash = format!("b3:{}", "c".repeat(64));
        crate::federation::persist_content_id(&pool, 4, "new", &late_hash).await?;
        crate::federation::persist_content_id(&pool, 5, "five", &late_hash).await?;
        assert_eq!(local_rows(&pool).await?, [50, 5686, 5797]);
        assert!(!toggle(&pool, 1, 60).await?.unwrap().0);
        assert_eq!(local_rows(&pool).await?, [5686, 5797]);
        Ok(())
    }
}
