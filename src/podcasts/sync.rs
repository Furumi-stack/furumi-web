//! Durable Frid podcast registers. Projections are user-scoped; revocations
//! remain here after a subscription is removed from the visible catalogue.
use super::*;
use music_dht::device_sync::{SyncOpPayload, SyncOpWire, SyncSnapshot};
use music_dht::podcast::{
    ListenSession, ProgressRecord, ProgressStatus, effective_share, merge_listen_session,
    merge_progress, merge_share, merge_subscription, next_podcast_generation,
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Postgres, Transaction};

fn current_users() -> &'static std::sync::Mutex<std::collections::HashSet<i64>> {
    static USERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i64>>> =
        std::sync::OnceLock::new();
    USERS.get_or_init(Default::default)
}
pub async fn publication_current(pool: &PgPool, user: i64) -> Result<bool> {
    if current_users()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&user)
    {
        return Ok(true);
    }
    let peers:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__fed_device d WHERE user_id=$1 AND trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL AND protocol_version>=4 AND device_id<>(SELECT device_id FROM furumusic__fed_device_identity WHERE user_id=$1))").bind(user).fetch_one(pool).await?;
    Ok(!peers)
}
pub async fn exchange_complete(pool: &PgPool, user: i64, peer: &str) -> Result<()> {
    let version: Option<i32> = sqlx::query_scalar(
        "SELECT protocol_version FROM furumusic__fed_device WHERE user_id=$1 AND device_id=$2",
    )
    .bind(user)
    .bind(peer)
    .fetch_optional(pool)
    .await?;
    if version.is_some_and(|v| v >= 4) {
        current_users()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(user);
        refresh_received(pool.clone());
        tokio::spawn(async {
            let _ = crate::federation::handle().sync_podcasts().await;
        });
    }
    Ok(())
}

async fn get<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    kind: &str,
    id: &str,
) -> Result<Option<T>> {
    let value: Option<Value> = sqlx::query_scalar("SELECT value FROM furumusic__podcast_register WHERE user_id=$1 AND kind=$2 AND entity_id=$3").bind(user).bind(kind).bind(id).fetch_optional(&mut **tx).await?;
    Ok(value.map(serde_json::from_value).transpose()?)
}
async fn put<T: Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    kind: &str,
    id: &str,
    value: &T,
) -> Result<()> {
    sqlx::query("INSERT INTO furumusic__podcast_register(user_id,kind,entity_id,value) VALUES($1,$2,$3,$4) ON CONFLICT(user_id,kind,entity_id) DO UPDATE SET value=EXCLUDED.value").bind(user).bind(kind).bind(id).bind(serde_json::to_value(value)?).execute(&mut **tx).await?;
    Ok(())
}
async fn begin(pool: &PgPool, user: i64) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,7512))")
        .bind(format!("podcast-sync:{user}"))
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn project_subscription(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    id: &str,
) -> Result<()> {
    let Some(sub) = get::<SubscriptionRecord>(tx, user, "subscription", id).await? else {
        return Ok(());
    };
    let share = get::<ShareRecord>(tx, user, "share", id).await?;
    let origin = sub
        .feed_urls()
        .into_iter()
        .find(|url| PodcastId::from_feed_url(url) == sub.podcast_id)
        .unwrap_or(sub.podcast.feed_url.clone());
    sqlx::query("INSERT INTO furumusic__podcast(id,origin_url,feed_url,metadata) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET origin_url=EXCLUDED.origin_url").bind(id).bind(origin.as_str()).bind(sub.podcast.feed_url.as_str()).bind(json!(sub.podcast)).execute(&mut **tx).await?;
    for alias in sub.feed_urls() {
        sqlx::query("INSERT INTO furumusic__podcast_alias(url,podcast_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(alias.as_str()).bind(id).execute(&mut **tx).await?;
    }
    if sub.subscribed {
        let shared = effective_share(Some(&sub), share.as_ref());
        sqlx::query("INSERT INTO furumusic__podcast_subscription(user_id,podcast_id,shared) VALUES($1,$2,$3) ON CONFLICT(user_id,podcast_id) DO UPDATE SET shared=EXCLUDED.shared,confirmed_urls=CASE WHEN EXCLUDED.shared THEN furumusic__podcast_subscription.confirmed_urls ELSE NULL END").bind(user).bind(id).bind(shared).execute(&mut **tx).await?;
    } else {
        sqlx::query(
            "DELETE FROM furumusic__podcast_subscription WHERE user_id=$1 AND podcast_id=$2",
        )
        .bind(user)
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn merge_sub(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    incoming: SubscriptionRecord,
) -> Result<()> {
    incoming.validate()?;
    let id = incoming.podcast_id.as_str();
    let mut merged = match get::<SubscriptionRecord>(tx, user, "subscription", id).await? {
        Some(current) => merge_subscription(&current, &incoming)?,
        None => incoming.clone(),
    };
    if let Some(aliases) = get::<Vec<FeedUrl>>(tx, user, "aliases", id).await? {
        merged.add_feed_aliases(&aliases);
    }
    put(tx, user, "subscription", id, &merged).await?;
    project_subscription(tx, user, id).await
}
async fn merge_shared(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    incoming: ShareRecord,
    local: bool,
) -> Result<()> {
    let id = incoming.podcast_id.as_str();
    let current = get::<ShareRecord>(tx, user, "share", id).await?;
    let merged = match &current {
        Some(current) => merge_share(current, &incoming)?,
        None => incoming.clone(),
    };
    if !local && current.as_ref().map(|r| r.op_id.as_str()) != Some(merged.op_id.as_str()) {
        sqlx::query("UPDATE furumusic__podcast_subscription SET confirmed_urls=NULL WHERE user_id=$1 AND podcast_id=$2").bind(user).bind(id).execute(&mut **tx).await?;
    }
    put(tx, user, "share", id, &merged).await?;
    project_subscription(tx, user, id).await
}
async fn merge_resume(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    incoming: ProgressRecord,
) -> Result<()> {
    incoming.validate()?;
    let id = incoming.episode_id.as_str();
    let merged = match get::<ProgressRecord>(tx, user, "progress", id).await? {
        Some(current) => merge_progress(&current, &incoming)?,
        None => incoming.clone(),
    };
    put(tx, user, "progress", id, &merged).await?;
    project_resume(tx, user, &merged).await
}
async fn project_resume(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    record: &ProgressRecord,
) -> Result<()> {
    sqlx::query("INSERT INTO furumusic__podcast_progress(user_id,episode_id,position_ms,completed,generation,checkpoint) SELECT $1,id,$3,$4,$5,0 FROM furumusic__podcast_episode WHERE id=$2 ON CONFLICT(user_id,episode_id) DO UPDATE SET position_ms=EXCLUDED.position_ms,completed=EXCLUDED.completed,generation=EXCLUDED.generation").bind(user).bind(record.episode_id.as_str()).bind(record.position_ms).bind(record.status==ProgressStatus::Completed).bind(record.generation.min(i64::MAX as u64) as i64).execute(&mut **tx).await?;
    Ok(())
}
async fn merge_session(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    incoming: ListenSession,
) -> Result<()> {
    incoming.validate()?;
    let id = &incoming.session_id;
    let merged = match get::<ListenSession>(tx, user, "session", id).await? {
        Some(current) => merge_listen_session(&current, &incoming)?,
        None => incoming.clone(),
    };
    put(tx, user, "session", id, &merged).await
}

/// Applying the same operation again is safe, including after a crash between
/// persisting its log entry and updating the projection.
pub async fn apply_op(pool: &PgPool, user: i64, op: &SyncOpWire) -> Result<bool> {
    let mut tx = begin(pool, user).await?;
    apply_op_tx(&mut tx, user, op).await?;
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn apply_op_tx(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    op: &SyncOpWire,
) -> Result<bool> {
    op.validate_podcast()?;
    let reference = match &op.payload {
        SyncOpPayload::PodcastProgressSet { episode, .. }
        | SyncOpPayload::PodcastListenRecorded { episode, .. } => episode.as_ref(),
        _ => None,
    };
    if let Some(reference) = reference {
        materialize_reference(tx, reference).await?;
    }
    if let Some(sub) = op.podcast_subscription() {
        merge_sub(tx, user, sub).await?;
    } else if let Some(share) = op.podcast_share() {
        let local:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__fed_device_identity WHERE user_id=$1 AND device_id=$2)").bind(user).bind(&op.origin_device_id).fetch_one(&mut **tx).await?;
        merge_shared(tx, user, share, local).await?;
    } else if let Some((id, aliases)) = op.podcast_feed_aliases() {
        let current = get::<Vec<FeedUrl>>(tx, user, "aliases", id.as_str())
            .await?
            .unwrap_or_default();
        let merged = music_dht::podcast::merge_feed_aliases(id, &current, aliases);
        put(tx, user, "aliases", id.as_str(), &merged).await?;
        if let Some(mut sub) =
            get::<SubscriptionRecord>(tx, user, "subscription", id.as_str()).await?
        {
            sub.add_feed_aliases(aliases);
            put(tx, user, "subscription", id.as_str(), &sub).await?;
            project_subscription(tx, user, id.as_str()).await?;
        }
    } else if let Some(progress) = op.podcast_progress() {
        merge_resume(tx, user, progress).await?;
    } else if let Some(session) = op.podcast_listen_session() {
        merge_session(tx, user, session.clone()).await?;
    }
    Ok(true)
}

async fn materialize_reference(
    tx: &mut Transaction<'_, Postgres>,
    reference: &music_dht::podcast::PodcastEpisodeRef,
) -> Result<()> {
    reference.validate()?;
    let podcast = PodcastMeta {
        feed_url: reference.feed_url.clone(),
        title: reference.podcast_title.clone(),
        author: None,
        description: None,
        link: None,
        image_url: reference.image_url.clone(),
    };
    sqlx::query("INSERT INTO furumusic__podcast(id,origin_url,feed_url,metadata) VALUES($1,$2,$2,$3) ON CONFLICT DO NOTHING").bind(reference.podcast_id.as_str()).bind(reference.feed_url.as_str()).bind(json!(podcast)).execute(&mut **tx).await?;
    let episode = EpisodeMeta {
        episode_id: reference.episode_id.clone(),
        podcast_id: reference.podcast_id.clone(),
        guid: None,
        title: reference.episode_title.clone(),
        description: None,
        season: reference.season,
        episode_number: reference.episode_number,
        episode_type: music_dht::podcast::EpisodeType::Full,
        published_at_ms: reference.published_at_ms,
        duration_ms: reference.duration_ms,
        audio_url: reference.audio_url.clone(),
        mime: reference.mime.clone(),
        byte_len: None,
        image_url: reference.image_url.clone(),
        link: None,
    };
    sqlx::query("INSERT INTO furumusic__podcast_episode(id,podcast_id,metadata) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(reference.episode_id.as_str()).bind(reference.podcast_id.as_str()).bind(json!(episode)).execute(&mut **tx).await?;
    Ok(())
}

pub async fn apply_snapshot(pool: &PgPool, user: i64, snapshot: &SyncSnapshot) -> Result<()> {
    if !snapshot.podcast_state_within_limits() {
        bail!("podcast snapshot exceeds contract limits");
    }
    let mut tx = begin(pool, user).await?;
    for sub in &snapshot.podcast_subscriptions {
        merge_sub(&mut tx, user, sub.clone()).await?;
    }
    for share in &snapshot.podcast_shares {
        merge_shared(&mut tx, user, share.clone(), false).await?;
    }
    for progress in &snapshot.podcast_progress {
        merge_resume(&mut tx, user, progress.clone()).await?;
    }
    for session in &snapshot.podcast_listen_sessions {
        merge_session(&mut tx, user, session.clone()).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn append_snapshot(pool: &PgPool, user: i64, snapshot: &mut SyncSnapshot) -> Result<()> {
    seed_user(pool, user).await?;
    let rows = sqlx::query(
        "SELECT kind,value FROM furumusic__podcast_register WHERE user_id=$1 ORDER BY entity_id",
    )
    .bind(user)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let value: Value = row.get("value");
        match row.get::<&str, _>("kind") {
            "subscription" => snapshot
                .podcast_subscriptions
                .push(serde_json::from_value(value)?),
            "share" => snapshot.podcast_shares.push(serde_json::from_value(value)?),
            "progress" => snapshot
                .podcast_progress
                .push(serde_json::from_value(value)?),
            "session" => snapshot
                .podcast_listen_sessions
                .push(serde_json::from_value(value)?),
            _ => {}
        }
    }
    snapshot.bound_podcast_state();
    Ok(())
}

pub async fn local_subscription(
    pool: &PgPool,
    user: i64,
    id: &str,
    subscribed: bool,
) -> Result<()> {
    let mut tx = begin(pool, user).await?;
    let sub = get::<SubscriptionRecord>(&mut tx, user, "subscription", id).await?;
    let share = get::<ShareRecord>(&mut tx, user, "share", id).await?;
    let generation = next_podcast_generation(sub.as_ref(), share.as_ref());
    let row = sqlx::query("SELECT metadata,origin_url FROM furumusic__podcast WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let podcast: PodcastMeta = serde_json::from_value(row.get("metadata"))?;
    let origin = FeedUrl::parse(row.get::<&str, _>("origin_url"))?;
    let mut aliases = sub.as_ref().map(|s| s.feed_urls()).unwrap_or_default();
    aliases.push(origin);
    let podcast_id: PodcastId = serde_json::from_value(json!(id))?;
    let aliases = music_dht::podcast::merge_feed_aliases(&podcast_id, &[], &aliases);
    tx.commit().await?;
    crate::federation::devices::record_podcast_op(
        pool,
        user,
        SyncOpPayload::PodcastSubscriptionSet {
            podcast_id,
            subscribed,
            podcast,
            feed_aliases: aliases,
            generation,
        },
    )
    .await?;
    if !subscribed {
        local_share(pool, user, id, false).await?;
    }
    Ok(())
}
pub async fn local_share(pool: &PgPool, user: i64, id: &str, shared: bool) -> Result<()> {
    let mut tx = begin(pool, user).await?;
    let sub = get::<SubscriptionRecord>(&mut tx, user, "subscription", id).await?;
    let share = get::<ShareRecord>(&mut tx, user, "share", id).await?;
    let generation = next_podcast_generation(sub.as_ref(), share.as_ref());
    tx.commit().await?;
    crate::federation::devices::record_podcast_op(
        pool,
        user,
        SyncOpPayload::PodcastShareSet {
            podcast_id: serde_json::from_value(json!(id))?,
            shared,
            generation,
        },
    )
    .await
}
pub async fn seed_user(pool: &PgPool, user: i64) -> Result<()> {
    let rows=sqlx::query("SELECT s.podcast_id,s.shared FROM furumusic__podcast_subscription s WHERE user_id=$1 AND NOT EXISTS(SELECT 1 FROM furumusic__podcast_register r WHERE r.user_id=s.user_id AND r.entity_id=s.podcast_id AND r.kind='subscription')").bind(user).fetch_all(pool).await?;
    for row in rows {
        let id: String = row.get("podcast_id");
        let shared: bool = row.get("shared");
        local_subscription(pool, user, &id, true).await?;
        if shared {
            local_share(pool, user, &id, true).await?;
        }
    }
    Ok(())
}

pub async fn ensure_subscribed(pool: &PgPool, user: i64, id: &str) -> Result<()> {
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__podcast_register WHERE user_id=$1 AND kind='subscription' AND entity_id=$2 AND value->>'subscribed'='true')").bind(user).bind(id).fetch_one(pool).await?;
    if !active {
        local_subscription(pool, user, id, true).await?;
    }
    Ok(())
}

pub async fn local_progress(
    pool: &PgPool,
    user: i64,
    id: &str,
    generation: u64,
    position: i64,
    completed: bool,
) -> Result<()> {
    let row=sqlx::query("SELECT g.*,e.metadata AS episode,p.metadata AS podcast FROM furumusic__podcast_progress g JOIN furumusic__podcast_episode e ON e.id=g.episode_id JOIN furumusic__podcast p ON p.id=e.podcast_id WHERE g.user_id=$1 AND g.episode_id=$2").bind(user).bind(id).fetch_one(pool).await?;
    let episode: EpisodeMeta = serde_json::from_value(row.get("episode"))?;
    let podcast: PodcastMeta = serde_json::from_value(row.get("podcast"))?;
    crate::federation::devices::record_podcast_op(
        pool,
        user,
        SyncOpPayload::PodcastProgressSet {
            episode_id: episode.episode_id.clone(),
            podcast_id: episode.podcast_id.clone(),
            position_ms: position,
            status: if completed {
                ProgressStatus::Completed
            } else if position > 0 {
                ProgressStatus::InProgress
            } else {
                ProgressStatus::NotStarted
            },
            generation,
            session_id: format!("web:{user}:{id}:{generation}"),
            episode: Some(episode.to_ref(&podcast)),
        },
    )
    .await
}

pub async fn restore_episode_progress(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<()> {
    let rows=sqlx::query("SELECT user_id,value FROM furumusic__podcast_register WHERE kind='progress' AND entity_id=$1").bind(id).fetch_all(&mut **tx).await?;
    for row in rows {
        project_resume(
            tx,
            row.get("user_id"),
            &serde_json::from_value(row.get("value"))?,
        )
        .await?;
    }
    Ok(())
}

pub fn refresh_received(pool: PgPool) {
    static WORKER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(permit) = WORKER.try_acquire() else {
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        let ids:Vec<String>=sqlx::query_scalar("SELECT DISTINCT p.id FROM furumusic__podcast p JOIN furumusic__podcast_subscription s ON s.podcast_id=p.id WHERE p.checked_at IS NULL LIMIT 20").fetch_all(&pool).await.unwrap_or_default();
        for id in ids {
            let _ = refresh(&pool, &id).await;
        }
    });
}
