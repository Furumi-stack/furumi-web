//! Server-owned podcast catalogue and cache; subscriptions and progress are personal.
pub(crate) mod sync;
mod web;
pub use web::PodcastApp;

use crate::{
    config::AppConfig,
    scheduler::{Job, JobContext, JobLog},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use music_dht::podcast::feed::parse_feed_with_base;
use music_dht::podcast::{
    EpisodeMeta, FeedUrl, KnownEpisode, PodcastId, PodcastMeta, ShareRecord, SubscriptionRecord,
};
use music_dht::podcast_dht::{PodcastCardSpec, PodcastDht, PodcastShareState};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{net::IpAddr, path::PathBuf, time::Duration};
use tokio::io::AsyncWriteExt;

const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUDIO_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) async fn search(
    pool: &PgPool,
    user: i64,
    query: &str,
    limit: i64,
) -> Result<Vec<Value>> {
    let words: Vec<String> = query
        .split_whitespace()
        .take(20)
        .map(|word| {
            format!(
                "%{}%",
                word.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect();
    if words.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('id',p.id,'url',p.origin_url,'metadata',p.metadata,'subscribed',EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.user_id=$1 AND s.podcast_id=p.id)) FROM furumusic__podcast p WHERE EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.podcast_id=p.id AND (s.user_id=$1 OR s.shared)) AND NOT EXISTS(SELECT 1 FROM unnest($2::text[]) AS term WHERE concat_ws(' ',p.metadata->>'title',p.metadata->>'description',p.metadata->>'author') COLLATE furumusic_unicode NOT ILIKE term) ORDER BY p.metadata->>'title',p.id LIMIT $3").bind(user).bind(words).bind(limit.clamp(1,100)).fetch_all(pool).await?)
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 240
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])))
        }
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|v| public_address(IpAddr::V4(v)))
            .unwrap_or_else(|| (ip.segments()[0] & 0xe000) == 0x2000),
    }
}

/// Resolve and pin public addresses on every redirect, including enclosure requests.
pub(super) async fn fetch(
    url: &str,
    range: Option<&str>,
    validators: Option<(&str, &str)>,
) -> Result<reqwest::Response> {
    let mut url = reqwest::Url::parse(FeedUrl::parse(url)?.as_str())?;
    for _ in 0..8 {
        let host = url.host_str().context("missing host")?;
        let addresses: Vec<_> =
            tokio::net::lookup_host((host, url.port_or_known_default().context("missing port")?))
                .await?
                .collect();
        if addresses.is_empty() || addresses.iter().any(|a| !public_address(a.ip())) {
            bail!("podcast sources must use public internet addresses");
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("FuruMusic/", env!("CARGO_PKG_VERSION")))
            .resolve_to_addrs(host, &addresses)
            .build()?;
        let mut request = client.get(url.clone());
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        if let Some((etag, modified)) = validators {
            if !etag.is_empty() {
                request = request.header("If-None-Match", etag);
            }
            if !modified.is_empty() {
                request = request.header("If-Modified-Since", modified);
            }
        }
        let response = request
            .send()
            .await
            .context("podcast source request failed")?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(response);
        }
        if response.status().is_redirection() {
            url = url.join(
                response
                    .headers()
                    .get("location")
                    .context("redirect without location")?
                    .to_str()?,
            )?;
            FeedUrl::parse(url.as_str())?;
            continue;
        }
        return response
            .error_for_status()
            .context("podcast source returned an error");
    }
    bail!("too many podcast redirects")
}

async fn feed_bytes(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_FEED_BYTES as u64)
    {
        bail!("feed is too large");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > MAX_FEED_BYTES {
            bail!("feed is too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) fn cache_path(config: &AppConfig, id: &str) -> PathBuf {
    PathBuf::from(&config.agent_storage_dir)
        .join("podcasts")
        .join(blake3::hash(id.as_bytes()).to_hex().as_str())
}

pub(super) async fn subscribe(pool: &PgPool, user: i64, input: &str) -> Result<String> {
    let origin = FeedUrl::from_user_input(input)?;
    let existing: Option<String> =
        sqlx::query_scalar("SELECT podcast_id FROM furumusic__podcast_alias WHERE url=$1")
            .bind(origin.as_str())
            .fetch_optional(pool)
            .await?;
    let id = if let Some(id) = existing {
        id
    } else {
        let response = fetch(origin.as_str(), None, None).await?;
        let final_url = FeedUrl::parse(response.url().as_str())?;
        let bytes = feed_bytes(response).await?;
        let feed = parse_feed_with_base(&bytes, Some(final_url.as_str()))?;
        let mut tx = pool.begin().await?;
        // Serialize identity resolution: two users subscribing via a redirect share one catalogue.
        sqlx::query("SELECT pg_advisory_xact_lock(73829531)")
            .execute(&mut *tx)
            .await?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT podcast_id FROM furumusic__podcast_alias WHERE url=$1 OR url=$2 LIMIT 1",
        )
        .bind(origin.as_str())
        .bind(final_url.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let id = existing.unwrap_or_else(|| PodcastId::from_feed_url(&origin).as_str().to_owned());
        sqlx::query("INSERT INTO furumusic__podcast(id,origin_url,feed_url,metadata) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(origin.as_str()).bind(final_url.as_str()).bind(serde_json::to_value(feed.to_podcast_meta(final_url.clone()))?).execute(&mut *tx).await?;
        for url in [&origin, &final_url] {
            sqlx::query("INSERT INTO furumusic__podcast_alias(url,podcast_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(url.as_str()).bind(&id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        id
    };
    sqlx::query("INSERT INTO furumusic__podcast_subscription(user_id,podcast_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(user).bind(&id).execute(pool).await?;
    Ok(id)
}

pub(super) async fn refresh(pool: &PgPool, id: &str) -> Result<usize> {
    let result = tokio::time::timeout(Duration::from_secs(60), refresh_inner(pool, id))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("feed check timed out")));
    if result.is_err() {
        sqlx::query("UPDATE furumusic__podcast SET checked_at=now(),last_error='Feed check failed; retry later' WHERE id=$1").bind(id).execute(pool).await?;
    }
    result
}

async fn refresh_inner(pool: &PgPool, id: &str) -> Result<usize> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,7391))")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT * FROM furumusic__podcast WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let url: String = row.get("feed_url");
    let etag: Option<String> = row.get("etag");
    let modified: Option<String> = row.get("modified");
    let response = fetch(
        &url,
        None,
        Some((
            etag.as_deref().unwrap_or(""),
            modified.as_deref().unwrap_or(""),
        )),
    )
    .await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        sqlx::query("UPDATE furumusic__podcast SET checked_at=now(),last_error=NULL WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(0);
    }
    let final_url = FeedUrl::parse(response.url().as_str())?;
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let modified = response
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = feed_bytes(response).await?;
    let feed = parse_feed_with_base(&bytes, Some(final_url.as_str()))?;
    let podcast_id: PodcastId = serde_json::from_value(json!(id))?;
    let stored: Vec<Value> =
        sqlx::query_scalar("SELECT metadata FROM furumusic__podcast_episode WHERE podcast_id=$1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    let known: Vec<_> = stored
        .into_iter()
        .map(serde_json::from_value::<EpisodeMeta>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(|meta| KnownEpisode {
            episode_id: meta.episode_id.clone(),
            hints: meta.identity_hints(),
        })
        .collect();
    let ids = feed.resolve_episode_ids(&podcast_id, &known);
    let mut added = 0;
    for (episode, episode_id) in feed.episodes.iter().zip(ids) {
        let Some(episode_id) = episode_id else {
            continue;
        };
        let meta = episode.to_meta(&podcast_id, episode_id.clone());
        let inserted: bool = sqlx::query_scalar("INSERT INTO furumusic__podcast_episode(id,podcast_id,metadata) VALUES($1,$2,$3) ON CONFLICT(id) DO UPDATE SET metadata=EXCLUDED.metadata RETURNING (xmax=0)").bind(episode_id.as_str()).bind(id).bind(serde_json::to_value(meta)?).fetch_one(&mut *tx).await?;
        added += usize::from(inserted);
        sync::restore_episode_progress(&mut tx, episode_id.as_str()).await?;
    }
    for alias in [
        Some(final_url.clone()),
        feed.new_feed_url
            .as_deref()
            .and_then(|u| FeedUrl::parse(u).ok()),
    ]
    .into_iter()
    .flatten()
    {
        sqlx::query("INSERT INTO furumusic__podcast_alias(url,podcast_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(alias.as_str()).bind(id).execute(&mut *tx).await?;
    }
    let next_url = feed
        .new_feed_url
        .as_deref()
        .and_then(|u| FeedUrl::parse(u).ok())
        .unwrap_or(final_url.clone());
    let moved = next_url != final_url;
    sqlx::query("UPDATE furumusic__podcast SET feed_url=$2,metadata=$3,etag=$4,modified=$5,checked_at=now(),last_error=NULL WHERE id=$1")
        .bind(id).bind(next_url.as_str()).bind(serde_json::to_value(feed.to_podcast_meta(next_url.clone()))?).bind(if moved {None} else {etag}).bind(if moved {None} else {modified}).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(added)
}

pub(super) async fn download(
    pool: &PgPool,
    config: &AppConfig,
    id: &str,
    force: bool,
) -> Result<()> {
    static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);
    let _permit = DOWNLOADS.acquire().await?;
    let podcast: Option<String> =
        sqlx::query_scalar("SELECT podcast_id FROM furumusic__podcast_episode WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some(podcast) = podcast else {
        return Ok(());
    };
    let mut cache_guard = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,7421))")
        .bind(&podcast)
        .execute(&mut *cache_guard)
        .await?;
    let meta: Option<Value> = sqlx::query_scalar("UPDATE furumusic__podcast_episode SET download_until=now()+interval '15 minutes',download_error=NULL WHERE id=$1 AND cached_at IS NULL AND (download_until IS NULL OR download_until<now()) AND ($2 OR (NOT expired AND (retry_at IS NULL OR retry_at<now()))) RETURNING metadata")
        .bind(id).bind(force).fetch_optional(pool).await?;
    let Some(meta) = meta else { return Ok(()) };
    let meta: EpisodeMeta = serde_json::from_value(meta)?;
    let path = cache_path(config, id);
    let temporary = path.with_extension("part");
    let result: Result<()> = tokio::time::timeout(Duration::from_secs(12 * 60), async {
        tokio::fs::create_dir_all(path.parent().context("missing cache directory")?).await?;
        let response = fetch(&meta.audio_url, None, None).await?;
        if response.content_length().is_some_and(|len| len > MAX_AUDIO_BYTES) { bail!("episode exceeds 2 GiB limit"); }
        let expected = response.content_length();
        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(&temporary).await?;
        let mut length = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            length += chunk.len() as u64;
            if length > MAX_AUDIO_BYTES { bail!("episode exceeds 2 GiB limit"); }
            file.write_all(&chunk).await?;
        }
        if length == 0 || expected.is_some_and(|size| size != length) { bail!("incomplete episode download"); }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&temporary, &path).await?;
        sqlx::query("UPDATE furumusic__podcast_episode SET cached_at=now(),download_until=NULL,retry_at=NULL,download_error=NULL,expired=false WHERE id=$1").bind(id).execute(pool).await?;
        Ok(())
    }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("episode download timed out")));
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
        sqlx::query("UPDATE furumusic__podcast_episode SET download_until=NULL,retry_at=now()+interval '30 minutes',download_error='Download failed; will retry' WHERE id=$1").bind(id).execute(pool).await?;
    }
    result
}

pub(super) async fn download_pending(
    pool: &PgPool,
    config: &AppConfig,
    podcast: Option<&str>,
) -> Result<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT e.id FROM furumusic__podcast_episode e WHERE cached_at IS NULL AND NOT expired AND ($1::text IS NULL OR podcast_id=$1) AND EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.podcast_id=e.podcast_id) AND (retry_at IS NULL OR retry_at<now()) ORDER BY discovered_at DESC LIMIT 100").bind(podcast).fetch_all(pool).await?;
    futures_util::stream::iter(ids)
        .for_each_concurrent(3, |id| async move {
            let _ = download(pool, config, &id, false).await;
        })
        .await;
    Ok(())
}

pub async fn reconcile(pool: &PgPool, overlay: &PodcastDht) -> Result<()> {
    // Read the desired set only after acquiring the gate. An older pass must
    // never publish its snapshot after a completed share-off/unsubscribe pass.
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = GATE.lock().await;
    let rows = sqlx::query("SELECT p.*,s.confirmed_urls,s.user_id FROM furumusic__podcast p JOIN furumusic__podcast_subscription s ON s.podcast_id=p.id WHERE s.shared ORDER BY p.id").fetch_all(pool).await?;
    let mut states = std::collections::BTreeMap::new();
    for row in rows {
        if !sync::publication_current(pool, row.get("user_id")).await? {
            continue;
        }
        let id: String = row.get("id");
        let meta: PodcastMeta = serde_json::from_value(row.get("metadata"))?;
        let origin = FeedUrl::parse(row.get::<&str, _>("origin_url"))?;
        let sub = SubscriptionRecord {
            podcast_id: serde_json::from_value(json!(id))?,
            subscribed: true,
            podcast: meta,
            feed_aliases: vec![origin],
            generation: 1,
            hlc_ms: 1,
            op_id: "web-subscription".into(),
        };
        let card = PodcastCardSpec::from_subscription(&sub)?;
        let confirmed: Option<Value> = row.get("confirmed_urls");
        if !card.privacy_warnings().is_empty()
            && confirmed != Some(json!([card.feed_url, card.id_feed_url]))
        {
            continue;
        }
        let share = ShareRecord {
            podcast_id: sub.podcast_id.clone(),
            shared: true,
            generation: 2,
            hlc_ms: 2,
            op_id: "web-share".into(),
        };
        states.insert(
            id,
            PodcastShareState {
                subscription: sub,
                share: Some(share),
                card: Some(card),
                privacy_confirmed: true,
            },
        );
    }
    overlay.sync_shares(states.into_values().collect()).await?;
    Ok(())
}

pub struct PodcastRefreshJob;
#[async_trait::async_trait]
impl Job for PodcastRefreshJob {
    fn name(&self) -> &'static str {
        "podcast_refresh"
    }
    fn description(&self) -> &'static str {
        "Check subscribed podcast feeds, download episodes and enforce cache retention"
    }
    fn default_cron(&self) -> &'static str {
        "0 */30 * * * *"
    }
    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> Result<()> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT podcast_id FROM furumusic__podcast_subscription")
                .fetch_all(&ctx.pool)
                .await?;
        let mut failed = 0;
        for id in &ids {
            if refresh(&ctx.pool, id).await.is_err() {
                failed += 1;
                sqlx::query("UPDATE furumusic__podcast SET last_error='Feed check failed',checked_at=now() WHERE id=$1").bind(id).execute(&ctx.pool).await?;
            }
        }
        download_pending(&ctx.pool, &ctx.config, None).await?;
        cleanup(&ctx.pool, &ctx.config).await?;
        let _ = crate::federation::handle().sync_podcasts().await;
        log.info(&format!(
            "Checked {} podcast feeds; {} failed",
            ids.len(),
            failed
        ));
        Ok(())
    }
}

async fn cleanup(pool: &PgPool, config: &AppConfig) -> Result<()> {
    let days: Option<i32> =
        sqlx::query_scalar("SELECT retention_days FROM furumusic__podcast_settings WHERE id=1")
            .fetch_one(pool)
            .await?;
    let Some(days) = days else { return Ok(()) };
    let rows = sqlx::query(
        "SELECT id,podcast_id FROM furumusic__podcast_episode WHERE cached_at<now()-make_interval(days=>$1)",
    )
    .bind(days)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let id: String = row.get("id");
        let podcast: String = row.get("podcast_id");
        let mut tx = pool.begin().await?;
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,7421))")
                .bind(&podcast)
                .fetch_one(&mut *tx)
                .await?;
        if !locked {
            continue;
        }
        let expired: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__podcast_episode WHERE id=$1 AND cached_at<now()-make_interval(days=>$2))").bind(&id).bind(days).fetch_one(&mut *tx).await?;
        if !expired {
            continue;
        }
        match tokio::fs::remove_file(cache_path(config, &id)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => continue,
        }
        sqlx::query(
            "UPDATE furumusic__podcast_episode SET cached_at=NULL,expired=true WHERE id=$1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blocks_non_public_sources() {
        for ip in [
            "127.0.0.1",
            "10.1.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
        ] {
            assert!(!public_address(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_address("8.8.8.8".parse().unwrap()));
    }
}
