//! P2P federation for the furumusic server.
//!
//! When enabled in the admin settings, the server becomes a regular peer of
//! the furumi federation: it publishes its whole visible library (artists,
//! releases, tracks — names and small metadata, never files) into the
//! shared DHT and serves audio, track metadata, cover art and per-artist
//! catalogs to other peers (TUI clients) over the same wire protocols the
//! clients speak among themselves. The web player also searches known peers
//! for catalog metadata and, when enabled, compatible similarity embeddings;
//! local playback and the local library remain independent of the network.
//!
//! Settings are the regular admin config entries (`federation_enabled`,
//! `federation_network_id`, `federation_save_on_listen`) and apply on the fly —
//! saving the settings starts, stops or re-joins the node without a server
//! restart.

mod capabilities;
pub mod client;
pub mod devices;
mod receive;
mod serve;
mod similarity;
mod storage;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use music_dht::capabilities::CAPABILITIES_ALPN;
use music_dht::{
    ByteStream, ByteStreamConnectionStats, ItemKind, ItemSpec, MusicDhtConfig, MusicDhtService,
    NetworkId, PeerTicket, PublishStats, RendezvousConfig, SyncStats,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::Row as _;

use crate::config::AppConfig;
use storage::PostgresFederationStorage;

pub use serve::{AUDIO_ALPN, CATALOG_ALPN};
pub use similarity::SIMILARITY_ALPN;

/// How often the published library is re-synchronized with the database.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
const TRANSPORT_SAMPLE_LIMIT: usize = 16;

struct Running {
    service: Arc<MusicDhtService>,
    network_name: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct ContentHashJob {
    media_file_id: i64,
    sha256_hash: String,
    file_path: String,
}

#[derive(Clone)]
struct CachedArtwork {
    bytes: Vec<u8>,
    mime: String,
    fetched_at: std::time::Instant,
}

#[derive(Debug, Clone)]
struct TransportSample {
    at: String,
    protocol: &'static str,
    direction: &'static str,
    phase: &'static str,
    peer_id: String,
    selected_path: String,
    open_paths: usize,
    direct_paths: usize,
    relay_paths: usize,
    custom_paths: usize,
    selected_rtt_ms: Option<u64>,
    selected_tx_bytes: u64,
    selected_rx_bytes: u64,
    total_tx_bytes: u64,
    total_rx_bytes: u64,
    lost_packets: u64,
    lost_bytes: u64,
}

impl TransportSample {
    fn from_stats(
        protocol: &'static str,
        direction: &'static str,
        phase: &'static str,
        stats: ByteStreamConnectionStats,
    ) -> Self {
        Self {
            at: now_iso(),
            protocol,
            direction,
            phase,
            peer_id: stats.peer_id.to_string(),
            selected_path: stats.selected_path.as_str().to_string(),
            open_paths: stats.open_paths,
            direct_paths: stats.direct_paths,
            relay_paths: stats.relay_paths,
            custom_paths: stats.custom_paths,
            selected_rtt_ms: stats
                .selected_rtt
                .map(|duration| duration.as_millis() as u64),
            selected_tx_bytes: stats.selected_tx_bytes,
            selected_rx_bytes: stats.selected_rx_bytes,
            total_tx_bytes: stats.total_tx_bytes,
            total_rx_bytes: stats.total_rx_bytes,
            lost_packets: stats.lost_packets,
            lost_bytes: stats.lost_bytes,
        }
    }
}

#[derive(Debug, Default)]
struct TransportStatsState {
    total_samples: u64,
    direct_samples: u64,
    relay_samples: u64,
    custom_samples: u64,
    unknown_samples: u64,
    audio_samples: u64,
    catalog_samples: u64,
    sync_samples: u64,
    similarity_samples: u64,
    last: VecDeque<TransportSample>,
}

#[derive(Debug, Default)]
pub struct TransportStats {
    inner: std::sync::Mutex<TransportStatsState>,
}

impl TransportStats {
    fn reset(&self) {
        *lock(&self.inner) = TransportStatsState::default();
    }

    fn record(
        &self,
        protocol: &'static str,
        direction: &'static str,
        phase: &'static str,
        stats: ByteStreamConnectionStats,
    ) {
        let sample = TransportSample::from_stats(protocol, direction, phase, stats);
        let mut state = lock(&self.inner);
        state.total_samples += 1;
        match sample.selected_path.as_str() {
            "direct" => state.direct_samples += 1,
            "relay" => state.relay_samples += 1,
            "custom" => state.custom_samples += 1,
            _ => state.unknown_samples += 1,
        }
        match protocol {
            "audio" => state.audio_samples += 1,
            "catalog" => state.catalog_samples += 1,
            "device-sync" => state.sync_samples += 1,
            "similarity" => state.similarity_samples += 1,
            _ => {}
        }
        state.last.push_front(sample);
        while state.last.len() > TRANSPORT_SAMPLE_LIMIT {
            state.last.pop_back();
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.inner);
        let latest = state.last.front();
        json!({
            "total_samples": state.total_samples,
            "direct_samples": state.direct_samples,
            "relay_samples": state.relay_samples,
            "custom_samples": state.custom_samples,
            "unknown_samples": state.unknown_samples,
            "audio_samples": state.audio_samples,
            "catalog_samples": state.catalog_samples,
            "sync_samples": state.sync_samples,
            "similarity_samples": state.similarity_samples,
            "last_path": latest.map(|sample| sample.selected_path.clone()),
            "last_rtt_ms": latest.and_then(|sample| sample.selected_rtt_ms),
            "last_peer": latest.map(|sample| sample.peer_id.clone()),
            "last": state.last.iter().map(|sample| json!({
                "at": sample.at,
                "protocol": sample.protocol,
                "direction": sample.direction,
                "phase": sample.phase,
                "peer_id": sample.peer_id,
                "selected_path": sample.selected_path,
                "open_paths": sample.open_paths,
                "direct_paths": sample.direct_paths,
                "relay_paths": sample.relay_paths,
                "custom_paths": sample.custom_paths,
                "selected_rtt_ms": sample.selected_rtt_ms,
                "selected_tx_bytes": sample.selected_tx_bytes,
                "selected_rx_bytes": sample.selected_rx_bytes,
                "total_tx_bytes": sample.total_tx_bytes,
                "total_rx_bytes": sample.total_rx_bytes,
                "lost_packets": sample.lost_packets,
                "lost_bytes": sample.lost_bytes,
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn record_stream_transport(
    stats: &Arc<TransportStats>,
    protocol: &'static str,
    direction: &'static str,
    phase: &'static str,
    stream: &ByteStream,
) {
    stats.record(protocol, direction, phase, stream.connection_stats());
}

pub struct Federation {
    /// Transport data directory; server-side DHT state and identity live in PostgreSQL.
    data_dir: PathBuf,
    database_url: std::sync::Mutex<String>,
    storage_dir: std::sync::Mutex<String>,
    save_on_listen: std::sync::atomic::AtomicBool,
    content_cache: std::sync::Mutex<HashMap<i64, (String, String)>>,
    content_pending: std::sync::Mutex<HashSet<i64>>,
    prepared_cache: std::sync::Mutex<HashMap<String, (PathBuf, String)>>,
    artwork_cache: std::sync::Mutex<HashMap<String, CachedArtwork>>,
    download_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pool: tokio::sync::OnceCell<PgPool>,
    running: tokio::sync::Mutex<Option<Running>>,
    last_sync: std::sync::Mutex<Option<String>>,
    last_error: std::sync::Mutex<Option<String>>,
    transport_stats: Arc<TransportStats>,
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The process-wide federation handle.
pub fn handle() -> Arc<Federation> {
    static HANDLE: OnceLock<Arc<Federation>> = OnceLock::new();
    Arc::clone(HANDLE.get_or_init(|| {
        Arc::new(Federation {
            data_dir: PathBuf::from(crate::media_paths::resolve_config_path("federation")),
            database_url: std::sync::Mutex::new(String::new()),
            storage_dir: std::sync::Mutex::new(String::new()),
            save_on_listen: std::sync::atomic::AtomicBool::new(false),
            content_cache: std::sync::Mutex::new(Default::default()),
            content_pending: std::sync::Mutex::new(Default::default()),
            prepared_cache: std::sync::Mutex::new(Default::default()),
            artwork_cache: std::sync::Mutex::new(Default::default()),
            download_locks: std::sync::Mutex::new(Default::default()),
            pool: tokio::sync::OnceCell::new(),
            running: tokio::sync::Mutex::new(None),
            last_sync: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            transport_stats: Arc::new(TransportStats::default()),
        })
    }))
}

impl Federation {
    fn set_error(&self, message: Option<String>) {
        *lock(&self.last_error) = message;
    }

    async fn pool(&self) -> Result<PgPool> {
        let url = lock(&self.database_url).clone();
        anyhow::ensure!(!url.is_empty(), "database is not configured");
        let pool = self
            .pool
            .get_or_try_init(|| async {
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&url)
                    .await
            })
            .await?;
        Ok(pool.clone())
    }

    /// Starts the node at boot when federation was left enabled. The
    /// settings live in the config KV table, so this waits for the database
    /// and resolves the same default → DB → env precedence the config uses.
    pub async fn boot(self: &Arc<Self>, config: &AppConfig) {
        *lock(&self.database_url) = config.database_url.clone();
        if config.database_url.is_empty() {
            return;
        }
        let pool = match self.pool().await {
            Ok(pool) => pool,
            Err(err) => {
                tracing::warn!("federation boot: database unavailable: {err:#}");
                return;
            }
        };
        // `config` carries defaults + env; overlay the DB rows for fields
        // that have no env override (env > DB > default).
        let mut effective = config.clone();
        let rows = sqlx::query(
            "SELECT key, value FROM furumusic__config_entry
             WHERE key IN ('federation_enabled', 'federation_network_id',
                           'federation_save_on_listen', 'agent_storage_dir')",
        )
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
        for row in rows {
            let key: String = row.get(0);
            let value: String = row.get(1);
            let env_key = format!("FURU_{}", key.to_ascii_uppercase());
            if std::env::var(&env_key).is_ok() {
                continue;
            }
            match key.as_str() {
                "federation_enabled" => {
                    if let Ok(parsed) = value.parse() {
                        effective.federation_enabled = parsed;
                    }
                }
                "federation_network_id" => effective.federation_network_id = value,
                "federation_save_on_listen" => {
                    if let Ok(parsed) = value.parse() {
                        effective.federation_save_on_listen = parsed;
                    }
                }
                "agent_storage_dir" => {
                    effective.agent_storage_dir = crate::media_paths::resolve_config_path(&value);
                }
                _ => {}
            }
        }
        self.apply(&effective).await;
    }

    /// Applies the effective configuration: starts, stops or re-joins the
    /// node. Called at boot and every time the admin settings are saved.
    pub async fn apply(self: &Arc<Self>, config: &AppConfig) {
        *lock(&self.database_url) = config.database_url.clone();
        *lock(&self.storage_dir) = config.agent_storage_dir.clone();
        self.save_on_listen.store(
            config.federation_save_on_listen,
            std::sync::atomic::Ordering::Relaxed,
        );
        let network = config.federation_network_id.trim().to_string();
        if config.federation_enabled && !network.is_empty() {
            if let Err(err) = self.start(network, config.agent_storage_dir.clone()).await {
                tracing::error!("federation start failed: {err:#}");
                self.set_error(Some(format!("start failed: {err}")));
            }
        } else {
            self.stop().await;
        }
    }

    /// Starts the DHT node. Idempotent per network name; a node on another
    /// network is stopped and re-joined.
    async fn start(self: &Arc<Self>, network_name: String, storage_dir: String) -> Result<()> {
        let pool = self.pool().await?;
        let mut guard = self.running.lock().await;
        if let Some(running) = guard.as_ref() {
            if running.network_name == network_name {
                return Ok(());
            }
            stop_running(guard.take()).await;
        }

        let dht_storage = Arc::new(PostgresFederationStorage::new(pool.clone()).await?);
        let secret_key = dht_storage.load_or_create_secret_key().await?;
        self.transport_stats.reset();

        let config = MusicDhtConfig::builder()
            .data_dir(&self.data_dir)
            .network_id(NetworkId::from_name(&network_name))
            // Peers of the network find each other knowing only its name.
            .rendezvous(RendezvousConfig::default())
            .stream_protocol(AUDIO_ALPN)
            .stream_protocol(CATALOG_ALPN)
            .stream_protocol(devices::SYNC_ALPN)
            .stream_protocol(SIMILARITY_ALPN)
            .schema_independent_stream_protocol(CAPABILITIES_ALPN)
            .build()
            .map_err(|err| anyhow::anyhow!("invalid federation config: {err}"))?;
        let (service, mut events) =
            MusicDhtService::start_with_storage_and_secret_key(config, dht_storage, secret_key)
                .await
                .map_err(|err| anyhow::anyhow!("failed to start the DHT node: {err}"))?;
        let service = Arc::new(service);
        tracing::info!(
            endpoint_id = %service.endpoint_id(),
            network = %network_name,
            "federation started"
        );

        // Drain DHT events into the log; the channel is bounded.
        let event_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                tracing::debug!("federation event: {event:?}");
            }
        });
        // Keep the published library in sync with the database.
        let sync_self = Arc::clone(self);
        let sync_service = Arc::clone(&service);
        let sync_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(SYNC_INTERVAL);
            loop {
                interval.tick().await;
                let _ = sync_self.sync_once(&sync_service).await;
            }
        });
        // Serve audio and catalog requests from other peers.
        let audio_acceptor = service
            .stream_acceptor(AUDIO_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the audio acceptor: {err}"))?;
        let audio_task = tokio::spawn(serve::serve_audio(
            audio_acceptor,
            pool.clone(),
            storage_dir.clone(),
            service.endpoint_id(),
            Arc::clone(&self.transport_stats),
        ));
        let catalog_acceptor = service
            .stream_acceptor(CATALOG_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the catalog acceptor: {err}"))?;
        let catalog_task = tokio::spawn(serve::serve_catalog(
            catalog_acceptor,
            pool.clone(),
            storage_dir,
            service.endpoint_id(),
            Arc::clone(&self.transport_stats),
        ));
        let device_acceptor = service
            .stream_acceptor(devices::SYNC_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the device-sync acceptor: {err}"))?;
        let device_hub = crate::player::PlayerDeviceHub::shared();
        let device_task = tokio::spawn(devices::serve_peers(
            device_acceptor,
            pool.clone(),
            Arc::clone(&service),
            Arc::clone(&device_hub),
            Arc::clone(&self.transport_stats),
        ));
        let device_sync_task = tokio::spawn(devices::sync_loop(
            pool,
            Arc::clone(&service),
            device_hub,
            Arc::clone(&self.transport_stats),
        ));
        let capabilities_acceptor = service
            .stream_acceptor(CAPABILITIES_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the capabilities acceptor: {err}"))?;
        let capabilities_task = tokio::spawn(capabilities::serve(capabilities_acceptor));
        let similarity_acceptor = service
            .stream_acceptor(SIMILARITY_ALPN)
            .map_err(|err| anyhow::anyhow!("failed to take the similarity acceptor: {err}"))?;
        let similarity_task = tokio::spawn(similarity::serve_peers(
            similarity_acceptor,
            crate::similarity::handle(),
            service.endpoint_id(),
            Arc::clone(&self.transport_stats),
        ));

        *guard = Some(Running {
            service,
            network_name,
            tasks: vec![
                event_task,
                sync_task,
                audio_task,
                catalog_task,
                device_task,
                device_sync_task,
                capabilities_task,
                similarity_task,
            ],
        });
        self.set_error(None);
        drop(guard);
        // Publish right away instead of waiting for the first timer tick.
        self.spawn_sync_soon().await;
        Ok(())
    }

    async fn stop(&self) {
        let mut guard = self.running.lock().await;
        stop_running(guard.take()).await;
    }

    async fn service(&self) -> Result<Arc<MusicDhtService>> {
        self.running
            .lock()
            .await
            .as_ref()
            .map(|running| Arc::clone(&running.service))
            .context("federation is not running")
    }

    async fn spawn_sync_soon(self: &Arc<Self>) {
        if let Ok(service) = self.service().await {
            let fed = Arc::clone(self);
            tokio::spawn(async move {
                let _ = fed.sync_once(&service).await;
            });
        }
    }

    pub async fn sync_now(self: &Arc<Self>) -> Result<()> {
        let service = self.service().await?;
        let sync_stats = self.sync_once(&service).await?;
        let publish_stats = match service.republish().await {
            Ok(stats) => stats,
            Err(err) => {
                tracing::warn!("federation republish failed: {err}");
                self.set_error(Some(format!("republish failed: {err}")));
                anyhow::bail!("republish failed: {err}");
            }
        };
        self.record_publish_success(sync_stats, publish_stats);
        Ok(())
    }

    async fn sync_once(self: &Arc<Self>, service: &MusicDhtService) -> Result<SyncStats> {
        let specs = match self.collect_specs().await {
            Ok(specs) => specs,
            Err(err) => {
                tracing::warn!("federation sync: library read failed: {err:#}");
                self.set_error(Some(format!("library read failed: {err}")));
                anyhow::bail!("library read failed: {err}");
            }
        };
        match service.sync_library(specs).await {
            Ok(stats) => {
                self.record_sync_success(stats);
                if stats.failed > 0 {
                    self.set_error(Some(format!(
                        "{} item(s) failed to publish in the last sync",
                        stats.failed
                    )));
                } else {
                    self.set_error(None);
                }
                Ok(stats)
            }
            Err(err) => {
                tracing::warn!("federation sync failed: {err}");
                self.set_error(Some(format!("sync failed: {err}")));
                Err(anyhow::anyhow!("sync failed: {err}"))
            }
        }
    }

    fn record_sync_success(&self, stats: SyncStats) {
        *lock(&self.last_sync) = Some(format!(
            "{} (+{} ~{} −{}, unchanged {}, failed {})",
            now_iso(),
            stats.added,
            stats.updated,
            stats.removed,
            stats.unchanged,
            stats.failed
        ));
    }

    fn record_publish_success(&self, sync_stats: SyncStats, publish_stats: PublishStats) {
        *lock(&self.last_sync) = Some(format!(
            "{} (+{} ~{} −{}, unchanged {}, failed {}; republished {} records, {} keys, remote nodes {})",
            now_iso(),
            sync_stats.added,
            sync_stats.updated,
            sync_stats.removed,
            sync_stats.unchanged,
            sync_stats.failed,
            publish_stats.records,
            publish_stats.keys,
            publish_stats.remote_nodes,
        ));
        self.set_error(None);
    }

    /// Everything the regular player shows, as DHT item specs: non-hidden
    /// artists, releases and tracks (a track also hides with its release).
    async fn collect_specs(self: &Arc<Self>) -> Result<Vec<ItemSpec>> {
        let pool = self.pool().await?;
        let mut specs = Vec::new();

        let artists = sqlx::query("SELECT id, name FROM furumusic__artist WHERE is_hidden = false")
            .fetch_all(&pool)
            .await?;
        for row in &artists {
            let id: i64 = row.get(0);
            specs.push(ItemSpec {
                local_key: format!("artist:{id}"),
                kind: ItemKind::Artist,
                name: row.get(1),
                artist_names: Vec::new(),
                featured_artist_names: Vec::new(),
                year: None,
                release_type: None,
                release_title: None,
                track_number: None,
                disc_number: None,
                duration_seconds: None,
                content_id: None,
            });
        }

        let release_artists = sqlx::query(
            "SELECT ra.release_id, a.name FROM furumusic__release_artist ra
             JOIN furumusic__artist a ON a.id = ra.artist_id
             ORDER BY ra.release_id, ra.position",
        )
        .fetch_all(&pool)
        .await?;
        let mut artists_of_release: std::collections::HashMap<i64, Vec<String>> =
            Default::default();
        for row in &release_artists {
            artists_of_release
                .entry(row.get(0))
                .or_default()
                .push(row.get(1));
        }
        let releases = sqlx::query(
            "SELECT id, title, year, release_type FROM furumusic__release
             WHERE is_hidden = false",
        )
        .fetch_all(&pool)
        .await?;
        for row in &releases {
            let id: i64 = row.get(0);
            specs.push(ItemSpec {
                local_key: format!("release:{id}"),
                kind: ItemKind::Release,
                name: row.get(1),
                artist_names: artists_of_release.remove(&id).unwrap_or_default(),
                featured_artist_names: Vec::new(),
                year: row.get(2),
                release_type: row.get(3),
                release_title: None,
                track_number: None,
                disc_number: None,
                duration_seconds: None,
                content_id: None,
            });
        }

        let track_artists = sqlx::query(
            "SELECT ta.track_id, a.name, ta.role FROM furumusic__track_artist ta
             JOIN furumusic__artist a ON a.id = ta.artist_id
             WHERE ta.role IN ('main', 'featuring')
             ORDER BY ta.track_id,
                      CASE ta.role WHEN 'main' THEN 0 ELSE 1 END,
                      ta.position",
        )
        .fetch_all(&pool)
        .await?;
        let mut artists_of_track: std::collections::HashMap<i64, Vec<String>> = Default::default();
        let mut featured_of_track: std::collections::HashMap<i64, Vec<String>> = Default::default();
        for row in &track_artists {
            let id: i64 = row.get(0);
            let name: String = row.get(1);
            if row.get::<String, _>(2) == "featuring" {
                featured_of_track.entry(id).or_default().push(name);
            } else {
                artists_of_track.entry(id).or_default().push(name);
            }
        }
        let tracks = sqlx::query(
            "SELECT t.id, t.title, COALESCE(t.year, r.year), t.duration_seconds,
                    r.title, r.release_type, t.track_number, t.disc_number,
                    t.audio_file_id, m.file_path, m.sha256_hash, c.content_id
             FROM furumusic__track t
             JOIN furumusic__release r ON r.id = t.release_id
             JOIN furumusic__media_file m ON m.id = t.audio_file_id
             LEFT JOIN furumusic__federation_content_id_cache c
                    ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
             WHERE t.is_hidden = false AND r.is_hidden = false",
        )
        .fetch_all(&pool)
        .await?;
        let storage_dir = lock(&self.storage_dir).clone();
        let mut content_hash_jobs = Vec::new();
        for row in &tracks {
            let id: i64 = row.get(0);
            let duration: f64 = row.get(3);
            let media_file_id: i64 = row.get(8);
            let file_path: String = row.get(9);
            let sha256_hash: String = row.get(10);
            let cached_content_id: Option<String> = row.get(11);
            let content_id = cached_content_id
                .or_else(|| self.cached_content_id_for_media(media_file_id, &sha256_hash));
            if content_id.is_none()
                && !storage_dir.trim().is_empty()
                && self.mark_content_hash_pending(media_file_id)
            {
                content_hash_jobs.push(ContentHashJob {
                    media_file_id,
                    sha256_hash,
                    file_path,
                });
            }
            specs.push(ItemSpec {
                local_key: format!("track:{id}"),
                kind: ItemKind::Track,
                name: row.get(1),
                artist_names: artists_of_track.remove(&id).unwrap_or_default(),
                featured_artist_names: featured_of_track.remove(&id).unwrap_or_default(),
                year: row.get(2),
                release_type: row.get(5),
                release_title: Some(row.get(4)),
                track_number: row.get(6),
                disc_number: row.get(7),
                duration_seconds: (duration > 0.0).then_some(duration),
                content_id,
            });
        }
        self.spawn_content_warmer(pool.clone(), storage_dir, content_hash_jobs);

        Ok(specs)
    }

    fn cached_content_id_for_media(&self, media_file_id: i64, sha256_hash: &str) -> Option<String> {
        if let Some((cached_hash, content_id)) = lock(&self.content_cache).get(&media_file_id)
            && cached_hash == sha256_hash
        {
            return Some(content_id.clone());
        }
        None
    }

    fn mark_content_hash_pending(&self, media_file_id: i64) -> bool {
        lock(&self.content_pending).insert(media_file_id)
    }

    fn spawn_content_warmer(
        self: &Arc<Self>,
        pool: PgPool,
        storage_dir: String,
        jobs: Vec<ContentHashJob>,
    ) {
        if jobs.is_empty() {
            return;
        }
        let fed = Arc::clone(self);
        tokio::spawn(async move {
            let total = jobs.len();
            let mut stored = 0usize;
            for job in jobs {
                let job_storage_dir = storage_dir.clone();
                let job_file_path = job.file_path.clone();
                let content_id = tokio::task::spawn_blocking(move || {
                    audio_content_id(&job_storage_dir, &job_file_path)
                })
                .await
                .ok()
                .flatten();
                lock(&fed.content_pending).remove(&job.media_file_id);
                if let Some(content_id) = content_id {
                    lock(&fed.content_cache).insert(
                        job.media_file_id,
                        (job.sha256_hash.clone(), content_id.clone()),
                    );
                    if let Err(err) =
                        persist_content_id(&pool, job.media_file_id, &job.sha256_hash, &content_id)
                            .await
                    {
                        tracing::warn!(
                            media_file_id = job.media_file_id,
                            "federation content-id cache write failed: {err:#}"
                        );
                    } else {
                        stored += 1;
                    }
                }
            }
            tracing::info!(total, stored, "federation content-id cache warm finished");
        });
    }

    /// Live status for the admin page.
    pub async fn status(&self) -> Value {
        let guard = self.running.lock().await;
        let node = match guard.as_ref() {
            Some(running) => {
                let service = &running.service;
                let published = service
                    .list_local_items()
                    .await
                    .map(|items| items.len())
                    .unwrap_or(0);
                let peers: Vec<String> = service
                    .connected_peers()
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                json!({
                    "running": true,
                    "network": running.network_name,
                    "endpoint_id": service.endpoint_id().to_string(),
                    "connected_peers": peers,
                    "known_contacts": service.known_peers().len(),
                    "published_items": published,
                    "transport": self.transport_stats.snapshot(),
                })
            }
            None => json!({ "running": false }),
        };
        json!({
            "node": node,
            "last_sync": lock(&self.last_sync).clone(),
            "last_error": lock(&self.last_error).clone(),
        })
    }

    pub async fn ticket(&self) -> Result<String> {
        let service = self.service().await?;
        let ticket = service
            .ticket()
            .await
            .map_err(|err| anyhow::anyhow!("cannot create a ticket: {err}"))?;
        Ok(ticket.to_string())
    }

    pub async fn connect(&self, ticket: &str) -> Result<String> {
        let service = self.service().await?;
        let ticket: PeerTicket = ticket
            .trim()
            .parse()
            .map_err(|err| anyhow::anyhow!("malformed ticket: {err}"))?;
        let peer = service
            .connect(ticket)
            .await
            .map_err(|err| anyhow::anyhow!("connect failed: {err}"))?;
        Ok(peer.to_string())
    }

    pub async fn search_similarity(
        &self,
        query: crate::similarity::QueryVector,
        limit: usize,
    ) -> Result<Vec<similarity::RemoteSimilarityTrack>> {
        anyhow::ensure!(
            crate::similarity::handle().enabled(),
            "similarity search is disabled"
        );
        let service = self.service().await?;
        similarity::search(service, query, limit, Arc::clone(&self.transport_stats)).await
    }

    pub async fn fed_device_status(
        &self,
        user_id: i64,
        user_name: &str,
    ) -> Result<devices::FedDeviceStatus> {
        let pool = self.pool().await?;
        devices::status(&pool, user_id, user_name).await
    }

    pub async fn fed_device_invite(&self, user_id: i64, user_name: &str) -> Result<String> {
        let service = self.service().await?;
        let pool = self.pool().await?;
        devices::create_invite(&pool, service, user_id, user_name).await
    }

    pub async fn fed_device_connect(
        &self,
        user_id: i64,
        user_name: &str,
        invite: &str,
    ) -> Result<String> {
        let network_id = devices::invite_network_id(invite)?;
        {
            let guard = self.running.lock().await;
            let Some(running) = guard.as_ref() else {
                anyhow::bail!("federation is not running");
            };
            let expected = NetworkId::from_name(&running.network_name);
            anyhow::ensure!(
                network_id == expected,
                "device invite belongs to a different federation network"
            );
        }
        let service = self.service().await?;
        let pool = self.pool().await?;
        devices::connect_invite(
            &pool,
            service,
            crate::player::PlayerDeviceHub::shared(),
            Arc::clone(&self.transport_stats),
            user_id,
            user_name,
            invite,
        )
        .await
    }

    pub async fn fed_device_answer_pairing(
        &self,
        user_id: i64,
        request_id: &str,
        accept: bool,
        use_requester_group: bool,
    ) -> Result<()> {
        let pool = self.pool().await?;
        devices::answer_pairing(&pool, user_id, request_id, accept, use_requester_group).await
    }

    pub async fn fed_device_revoke(&self, user_id: i64, device_id: &str) -> Result<()> {
        let pool = self.pool().await?;
        devices::revoke_device(&pool, user_id, device_id).await
    }

    pub async fn fed_device_sync_now(&self, user_id: i64) -> Result<()> {
        let service = self.service().await?;
        let pool = self.pool().await?;
        devices::sync_once(
            &pool,
            service,
            crate::player::PlayerDeviceHub::shared(),
            Arc::clone(&self.transport_stats),
            user_id,
        )
        .await
    }

    pub async fn fed_device_web_command(
        &self,
        user_id: i64,
        target_device_id: &str,
        command: &str,
        payload: serde_json::Value,
        current_state: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let pool = self.pool().await?;
        devices::record_web_playback_command(
            &pool,
            user_id,
            target_device_id,
            command,
            payload,
            current_state,
        )
        .await
    }

    pub async fn fed_device_web_active_transfer(
        &self,
        user_id: i64,
        target_device_id: &str,
        previous_device_id: Option<&str>,
        state: serde_json::Value,
    ) -> Result<()> {
        let pool = self.pool().await?;
        devices::record_web_active_transfer(
            &pool,
            user_id,
            target_device_id,
            previous_device_id,
            state,
        )
        .await
    }

    pub async fn fed_device_web_active_takeover(
        &self,
        user_id: i64,
        previous_device_id: &str,
        state: serde_json::Value,
    ) -> Result<()> {
        let pool = self.pool().await?;
        devices::record_web_active_takeover(&pool, user_id, previous_device_id, state).await
    }
}

async fn persist_content_id(
    pool: &PgPool,
    media_file_id: i64,
    sha256_hash: &str,
    content_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO furumusic__federation_content_id_cache
         (media_file_id, sha256_hash, content_id, updated_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (media_file_id) DO UPDATE SET
            sha256_hash = EXCLUDED.sha256_hash,
            content_id = EXCLUDED.content_id,
            updated_at = EXCLUDED.updated_at",
    )
    .bind(media_file_id)
    .bind(sha256_hash)
    .bind(content_id)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

fn audio_content_id(storage_dir: &str, file_path: &str) -> Option<String> {
    if storage_dir.trim().is_empty() {
        return None;
    }
    let path = crate::media_paths::resolve_media_file_path(storage_dir, file_path);
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("b3:{}", hasher.finalize().to_hex()))
}

async fn stop_running(running: Option<Running>) {
    let Some(running) = running else { return };
    for task in &running.tasks {
        task.abort();
    }
    if let Err(err) = running.service.shutdown().await {
        tracing::warn!("federation node shutdown reported an error: {err}");
    }
    tracing::info!("federation stopped");
}
