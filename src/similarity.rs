//! Local, server-wide music embeddings and exact cosine search.
//!
//! PostgreSQL is the durable source of truth. The active profile is mirrored
//! into a replaceable in-memory index so ordinary searches do not require a
//! vector extension or a second database.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures_util::StreamExt as _;
use rodio::{Decoder, Source as _};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use sqlx::{PgPool, Row as _};
use tokio::io::AsyncWriteExt as _;
use tract_onnx::prelude::*;
use tract_onnx::tract_core::dims;

use crate::config::AppConfig;

const SAMPLE_RATE: usize = 16_000;
const FRAME_SIZE: usize = 512;
const HOP_SIZE: usize = 256;
const MEL_BANDS: usize = 96;
const PATCH_FRAMES: usize = 128;
const PATCH_HOP: usize = 62;
const EMBEDDING_DIMENSIONS: usize = 1280;
const MODEL_BATCH: usize = 8;
const MAX_MODEL_BYTES: usize = 64 * 1024 * 1024;
const RESULT_LIMIT: usize = 50;
const MAX_PER_ARTIST: usize = 3;
const NEAR_DUPLICATE_COSINE: f32 = 0.995;
const FULL_TRACK_MAX_SECONDS: u32 = 5 * 60;
const LONG_TRACK_WINDOW_SECONDS: u32 = 60;
const PIPELINE_POLL_INTERVAL: Duration = Duration::from_secs(30);

pub const DEFAULT_MODEL_ID: &str = "discogs-effnet-bsdynamic-1";
pub const DEFAULT_PROFILE_ID: &str = "furumi-full-track-v1";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProfileSpec {
    pub id: &'static str,
    pub title: &'static str,
}

pub const PROFILES: &[ProfileSpec] = &[ProfileSpec {
    id: DEFAULT_PROFILE_ID,
    title: "Full track / balanced long track",
}];

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelSpec {
    pub id: &'static str,
    pub version: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub dimensions: usize,
    pub license: &'static str,
}

pub const MODELS: &[ModelSpec] = &[ModelSpec {
    id: DEFAULT_MODEL_ID,
    version: "1",
    filename: "discogs-effnet-bsdynamic-1.onnx",
    url: "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx",
    sha256: "a280825b334797cf677939db8cd5762c0392aedd0ca6415dbc1cd083f045e43c",
    dimensions: EMBEDDING_DIMENSIONS,
    license: "CC BY-NC-SA 4.0 (or proprietary from MTG)",
}];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Disabled,
    Downloading,
    Loading,
    Processing,
    Ready,
    Error,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SimilarityStatus {
    pub phase: Phase,
    pub active_profile: Option<String>,
    pub target_profile: Option<String>,
    pub model: String,
    pub total_tracks: usize,
    pub completed_tracks: usize,
    pub failed_tracks: usize,
    pub stored_vectors: usize,
    pub stored_bytes: u64,
    pub current_track: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueryVector {
    pub profile_id: String,
    pub vector: Vec<f32>,
    pub source_content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RankedTrack {
    pub track_id: i64,
    pub score: f32,
    pub embedding_signature: [u8; music_dht::similarity::SIMILARITY_SIGNATURE_BYTES],
}

#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub track_id: i64,
    pub title: String,
    pub artist_names: Vec<String>,
    pub featured_artist_names: Vec<String>,
    pub year: Option<i32>,
    pub duration_seconds: f64,
    pub content_id: Option<String>,
    pub release_title: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    model: String,
    profile: String,
    workers: usize,
}

impl Settings {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            enabled: config.similarity_enabled,
            model: config.similarity_model.clone(),
            profile: config.similarity_profile.clone(),
            workers: (config.similarity_workers as usize).clamp(1, 16),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_MODEL_ID.to_owned(),
            profile: DEFAULT_PROFILE_ID.to_owned(),
            workers: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct SimilarityTrack {
    id: i64,
    title: String,
    file_path: PathBuf,
    source_sha256: String,
    source_content_id: Option<String>,
    duration_seconds: f64,
}

#[derive(Debug, Clone)]
struct StoredEmbedding {
    track_id: i64,
    vector: Vec<f32>,
    artist_key: String,
    content_id: Option<String>,
}

#[derive(Default)]
struct Index {
    profile_id: Option<String>,
    entries: Vec<StoredEmbedding>,
}

#[derive(Debug, Default)]
struct StorageStats {
    total_tracks: usize,
    embedded_tracks: usize,
    stored_vectors: usize,
    stored_bytes: u64,
}

type RunnableModel = Arc<TypedRunnableModel>;

pub struct Manager {
    database_url: Mutex<String>,
    storage_dir: Mutex<String>,
    pool: tokio::sync::OnceCell<PgPool>,
    settings: Mutex<Settings>,
    workers: AtomicUsize,
    generation: AtomicU64,
    status: Mutex<SimilarityStatus>,
    index: RwLock<Index>,
    model: Mutex<Option<(String, RunnableModel)>>,
    model_dir: PathBuf,
}

pub fn handle() -> Arc<Manager> {
    static HANDLE: OnceLock<Arc<Manager>> = OnceLock::new();
    Arc::clone(HANDLE.get_or_init(|| {
        Arc::new(Manager {
            database_url: Mutex::new(String::new()),
            storage_dir: Mutex::new(String::new()),
            pool: tokio::sync::OnceCell::new(),
            settings: Mutex::new(Settings::default()),
            workers: AtomicUsize::new(1),
            generation: AtomicU64::new(0),
            status: Mutex::new(SimilarityStatus::default()),
            index: RwLock::new(Index::default()),
            model: Mutex::new(None),
            model_dir: PathBuf::from(crate::media_paths::resolve_config_path("similarity-models")),
        })
    }))
}

impl Manager {
    pub async fn boot(self: &Arc<Self>, config: &AppConfig) {
        *lock(&self.database_url) = config.database_url.clone();
        *lock(&self.storage_dir) = config.agent_storage_dir.clone();
        if config.database_url.trim().is_empty() {
            return;
        }
        let pool = match self.pool().await {
            Ok(pool) => pool,
            Err(error) => {
                tracing::warn!(%error, "similarity boot: database unavailable");
                self.update_status(|status| {
                    status.phase = Phase::Error;
                    status.last_error = Some(format!("database unavailable: {error}"));
                });
                return;
            }
        };
        let mut effective = config.clone();
        let mut rows = None;
        for attempt in 0..20 {
            match sqlx::query(
                "SELECT key, value FROM furumusic__config_entry
                 WHERE key IN ('similarity_enabled', 'similarity_model',
                               'similarity_profile', 'similarity_workers',
                               'agent_storage_dir')",
            )
            .fetch_all(&pool)
            .await
            {
                Ok(loaded) => {
                    rows = Some(loaded);
                    break;
                }
                Err(error) if attempt < 19 => {
                    tracing::debug!(attempt, %error, "similarity boot: settings table not ready");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => {
                    tracing::warn!(%error, "similarity boot: database settings unavailable");
                }
            }
        }
        for row in rows.unwrap_or_default() {
            let key: String = row.get(0);
            let value: String = row.get(1);
            let env_key = format!("FURU_{}", key.to_ascii_uppercase());
            if std::env::var(&env_key).is_ok() {
                continue;
            }
            match key.as_str() {
                "similarity_enabled" => {
                    if let Ok(parsed) = value.parse() {
                        effective.similarity_enabled = parsed;
                    }
                }
                "similarity_model" => effective.similarity_model = value,
                "similarity_profile" => effective.similarity_profile = value,
                "similarity_workers" => {
                    if let Ok(parsed) = value.parse() {
                        effective.similarity_workers = parsed;
                    }
                }
                "agent_storage_dir" => {
                    effective.agent_storage_dir = crate::media_paths::resolve_config_path(&value);
                }
                _ => {}
            }
        }
        if let Err(error) = self.restore_stored_status(&pool).await {
            tracing::warn!(%error, "similarity boot: stored status unavailable");
        }
        self.apply(&effective);
    }

    pub fn apply(self: &Arc<Self>, config: &AppConfig) {
        *lock(&self.database_url) = config.database_url.clone();
        *lock(&self.storage_dir) = config.agent_storage_dir.clone();
        let settings = Settings::from_config(config);
        self.workers.store(settings.workers, Ordering::Release);
        let previous = std::mem::replace(&mut *lock(&self.settings), settings.clone());
        self.update_status(|status| status.model = settings.model.clone());
        if !settings.enabled {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.update_status(|status| {
                status.phase = Phase::Disabled;
                status.target_profile = None;
                status.current_track = None;
                status.last_error = None;
            });
            return;
        }
        if !previous.enabled
            || previous.model != settings.model
            || previous.profile != settings.profile
        {
            self.start();
        }
    }

    pub fn enabled(&self) -> bool {
        lock(&self.settings).enabled
    }

    pub fn status(&self) -> SimilarityStatus {
        lock(&self.status).clone()
    }

    /// Loads compact routing signatures for every current visible embedding.
    /// Embeddings created before DHT routing existed are upgraded in place;
    /// the CPU-heavy projection runs outside the async runtime.
    pub async fn routing_signatures(&self, profile_id: &str) -> Result<Vec<[u8; 32]>> {
        let pool = self.pool().await?;
        let missing = sqlx::query(
            "SELECT e.track_id, e.dimensions, e.vector
               FROM furumusic__track_embedding e
               JOIN furumusic__track t ON t.id = e.track_id
               JOIN furumusic__release r ON r.id = t.release_id
               JOIN furumusic__media_file m ON m.id = t.audio_file_id
              WHERE e.profile_id = $1 AND e.source_sha256 = m.sha256_hash
                AND t.is_hidden = FALSE AND r.is_hidden = FALSE
                AND (e.routing_signature IS NULL
                     OR octet_length(e.routing_signature) != 32)
              ORDER BY e.track_id",
        )
        .bind(profile_id)
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.get::<i64, _>(0),
                row.get::<i32, _>(1),
                row.get::<Vec<u8>, _>(2),
            )
        })
        .collect::<Vec<_>>();

        let computed = tokio::task::spawn_blocking(move || {
            missing
                .into_iter()
                .map(|(track_id, dimensions, bytes)| {
                    let vector = embedding_from_bytes(dimensions, &bytes)?;
                    let signature = music_dht::similarity_lsh::routing_signature(&vector)?;
                    Ok::<_, anyhow::Error>((track_id, signature))
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .context("similarity routing backfill task failed")??;

        if !computed.is_empty() {
            let mut transaction = pool.begin().await?;
            for (track_id, signature) in computed {
                sqlx::query(
                    "UPDATE furumusic__track_embedding
                        SET routing_signature = $3
                      WHERE track_id = $1 AND profile_id = $2
                        AND (routing_signature IS NULL
                             OR octet_length(routing_signature) != 32)",
                )
                .bind(track_id)
                .bind(profile_id)
                .bind(signature.as_slice())
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
        }

        let stored = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT e.routing_signature
               FROM furumusic__track_embedding e
               JOIN furumusic__track t ON t.id = e.track_id
               JOIN furumusic__release r ON r.id = t.release_id
               JOIN furumusic__media_file m ON m.id = t.audio_file_id
              WHERE e.profile_id = $1 AND e.source_sha256 = m.sha256_hash
                AND t.is_hidden = FALSE AND r.is_hidden = FALSE
              ORDER BY e.track_id",
        )
        .bind(profile_id)
        .fetch_all(&pool)
        .await?;
        stored
            .into_iter()
            .map(|signature| {
                <[u8; 32]>::try_from(signature)
                    .map_err(|_| anyhow::anyhow!("invalid similarity routing signature length"))
            })
            .collect()
    }

    pub fn start(self: &Arc<Self>) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = manager.run_pipeline(generation).await
                && manager.generation.load(Ordering::Acquire) == generation
            {
                tracing::error!(%error, "similarity pipeline failed");
                manager.update_status(|status| {
                    status.phase = Phase::Error;
                    status.current_track = None;
                    status.last_error = Some(format!("{error:#}"));
                });
            }
        });
    }

    pub async fn clear(self: &Arc<Self>) -> Result<()> {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let pool = self.pool().await?;
        sqlx::query("DELETE FROM furumusic__similarity_profile")
            .execute(&pool)
            .await?;
        *write(&self.index) = Index::default();
        self.update_status(|status| {
            *status = SimilarityStatus {
                phase: if self.enabled() {
                    Phase::Loading
                } else {
                    Phase::Disabled
                },
                model: lock(&self.settings).model.clone(),
                ..SimilarityStatus::default()
            };
        });
        if self.enabled() {
            self.start();
        }
        Ok(())
    }

    pub async fn query_for_track(&self, track_id: i64) -> Result<QueryVector> {
        anyhow::ensure!(self.enabled(), "similarity search is disabled");
        let profile_id = read(&self.index)
            .profile_id
            .clone()
            .context("no similarity profile is ready yet")?;
        let pool = self.pool().await?;
        let row = sqlx::query(
            "SELECT e.dimensions, e.vector, c.content_id
               FROM furumusic__track_embedding e
               JOIN furumusic__track t ON t.id = e.track_id
               JOIN furumusic__release r ON r.id = t.release_id
               JOIN furumusic__media_file m ON m.id = t.audio_file_id
               LEFT JOIN furumusic__federation_content_id_cache c
                 ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
              WHERE e.track_id = $1 AND e.profile_id = $2
                AND e.source_sha256 = m.sha256_hash
                AND t.is_hidden = FALSE AND r.is_hidden = FALSE",
        )
        .bind(track_id)
        .bind(&profile_id)
        .fetch_optional(&pool)
        .await?
        .context("this track has not been processed yet")?;
        let dimensions: i32 = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        let source_content_id: Option<String> = row.get(2);
        Ok(QueryVector {
            profile_id,
            vector: embedding_from_bytes(dimensions, &bytes)?,
            source_content_id,
        })
    }

    pub fn rank_vector(
        &self,
        profile_id: &str,
        vector: &[f32],
        exclude_track_id: Option<i64>,
        exclude_content_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedTrack>> {
        anyhow::ensure!(
            !vector.is_empty() && vector.len() <= 4096,
            "wrong embedding dimensions"
        );
        anyhow::ensure!(
            vector.iter().all(|value| value.is_finite()),
            "invalid embedding"
        );
        let index = read(&self.index);
        anyhow::ensure!(
            index.profile_id.as_deref() == Some(profile_id),
            "the requested similarity profile is not active"
        );
        let mut scores: Vec<(&StoredEmbedding, f32)> = index
            .entries
            .iter()
            .filter(|entry| {
                Some(entry.track_id) != exclude_track_id
                    && entry.vector.len() == vector.len()
                    && !exclude_content_id
                        .is_some_and(|source| entry.content_id.as_deref() == Some(source))
            })
            .map(|entry| (entry, dot(vector, &entry.vector)))
            .filter(|(_, score)| score.is_finite())
            .collect();
        scores.sort_by(|left, right| right.1.total_cmp(&left.1));

        let mut artist_counts: HashMap<String, usize> = HashMap::new();
        let mut kept_vectors: Vec<&[f32]> = vec![vector];
        let mut selected = Vec::new();
        for (entry, score) in scores {
            if is_near_duplicate(&entry.vector, &kept_vectors) {
                continue;
            }
            let count = artist_counts.entry(entry.artist_key.clone()).or_default();
            if !entry.artist_key.is_empty() && *count >= MAX_PER_ARTIST {
                continue;
            }
            *count += 1;
            let embedding_signature = music_dht::similarity::embedding_signature(&entry.vector)?;
            kept_vectors.push(&entry.vector);
            selected.push(RankedTrack {
                track_id: entry.track_id,
                score,
                embedding_signature,
            });
            if selected.len() >= limit.clamp(1, RESULT_LIMIT) {
                break;
            }
        }
        Ok(selected)
    }

    pub async fn metadata_for_tracks(&self, ids: &[i64]) -> Result<Vec<TrackMetadata>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool().await?;
        let rows = sqlx::query(
            "SELECT t.id, t.title::text, COALESCE(t.year, r.year),
                    t.duration_seconds, c.content_id, r.title::text,
                    t.track_number, t.disc_number,
                    COALESCE(array_agg(a.name::text ORDER BY ta.position)
                        FILTER (WHERE ta.role = 'main'), ARRAY[]::text[]),
                    COALESCE(array_agg(a.name::text ORDER BY ta.position)
                        FILTER (WHERE ta.role = 'featuring'), ARRAY[]::text[])
               FROM furumusic__track t
               JOIN furumusic__release r ON r.id = t.release_id
               JOIN furumusic__media_file m ON m.id = t.audio_file_id
               LEFT JOIN furumusic__federation_content_id_cache c
                 ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
               LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id
               LEFT JOIN furumusic__artist a ON a.id = ta.artist_id
              WHERE t.id = ANY($1) AND t.is_hidden = FALSE AND r.is_hidden = FALSE
              GROUP BY t.id, r.id, c.content_id",
        )
        .bind(ids)
        .fetch_all(&pool)
        .await?;
        let by_id: HashMap<i64, TrackMetadata> = rows
            .into_iter()
            .map(|row| {
                let track = TrackMetadata {
                    track_id: row.get(0),
                    title: row.get(1),
                    year: row.get(2),
                    duration_seconds: row.get(3),
                    content_id: row.get(4),
                    release_title: row.get(5),
                    track_number: row.get(6),
                    disc_number: row.get(7),
                    artist_names: row.get(8),
                    featured_artist_names: row.get(9),
                };
                (track.track_id, track)
            })
            .collect();
        Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
    }

    async fn run_pipeline(self: &Arc<Self>, generation: u64) -> Result<()> {
        let settings = lock(&self.settings).clone();
        if !settings.enabled {
            return Ok(());
        }
        let spec = model_by_id(&settings.model)
            .with_context(|| format!("unknown similarity model '{}'", settings.model))?;
        anyhow::ensure!(
            profile_by_id(&settings.profile).is_some(),
            "unknown preprocessing profile '{}'",
            settings.profile
        );
        let profile_id = profile_fingerprint(spec, &settings.profile);
        let pool = self.pool().await?;
        self.restore_active_index(&pool).await?;
        ensure_similarity_profile(&pool, &profile_id, spec, &settings.profile).await?;

        let stats = storage_stats(&pool, &profile_id).await?;
        self.update_status(|status| {
            status.phase = Phase::Downloading;
            status.target_profile = Some(profile_id.clone());
            status.model = spec.id.to_owned();
            status.total_tracks = stats.total_tracks;
            status.completed_tracks = stats.embedded_tracks;
            status.failed_tracks = 0;
            status.stored_vectors = stats.stored_vectors;
            status.stored_bytes = stats.stored_bytes;
            status.current_track = None;
            status.last_error = None;
        });
        let model_path = self.ensure_model(spec, generation).await?;
        self.ensure_generation(generation)?;
        self.update_status(|status| status.phase = Phase::Loading);
        let model = self.load_model(&profile_id, &model_path).await?;
        self.ensure_generation(generation)?;

        let mut failures: HashSet<(i64, String)> = HashSet::new();
        loop {
            self.ensure_generation(generation)?;
            let storage_dir = lock(&self.storage_dir).clone();
            let mut pending = pending_tracks(&pool, &profile_id, &storage_dir).await?;
            pending.retain(|track| !failures.contains(&(track.id, track.source_sha256.clone())));
            if !pending.is_empty() {
                self.update_status(|status| status.phase = Phase::Processing);
                let mut queue: std::collections::VecDeque<_> = pending.into();
                let mut jobs = tokio::task::JoinSet::new();
                while !queue.is_empty() || !jobs.is_empty() {
                    self.ensure_generation(generation)?;
                    let workers = self.workers.load(Ordering::Acquire).clamp(1, 16);
                    while jobs.len() < workers {
                        let Some(track) = queue.pop_front() else {
                            break;
                        };
                        self.update_status(|status| {
                            status.current_track = Some(track.title.clone())
                        });
                        let model = Arc::clone(&model);
                        jobs.spawn_blocking(move || {
                            let started = Instant::now();
                            let result =
                                embed_track(&model, &track.file_path, track.duration_seconds);
                            (track, result, started.elapsed())
                        });
                    }
                    let Some(result) = jobs.join_next().await else {
                        continue;
                    };
                    let (track, result, elapsed) = result.context("embedding worker panicked")?;
                    self.ensure_generation(generation)?;
                    match result {
                        Ok(vector) => {
                            store_embedding(&pool, &track, &profile_id, &vector).await?;
                            tracing::info!(
                                track_id = track.id,
                                title = %track.title,
                                elapsed_ms = elapsed.as_millis(),
                                profile = %profile_id,
                                "track embedding calculated"
                            );
                            self.update_status(|status| status.completed_tracks += 1);
                        }
                        Err(error) => {
                            tracing::warn!(
                                track_id = track.id,
                                title = %track.title,
                                %error,
                                "track embedding failed"
                            );
                            failures.insert((track.id, track.source_sha256.clone()));
                            self.update_status(|status| {
                                status.failed_tracks += 1;
                                status.last_error = Some(format!("{}: {error:#}", track.title));
                            });
                        }
                    }
                }
            }

            self.ensure_generation(generation)?;
            let entries = load_index(&pool, &profile_id).await?;
            let stats = storage_stats(&pool, &profile_id).await?;
            anyhow::ensure!(
                stats.total_tracks == 0 || !entries.is_empty(),
                "no visible tracks could be processed"
            );
            activate_profile(&pool, &profile_id).await?;
            *write(&self.index) = Index {
                profile_id: Some(profile_id.clone()),
                entries,
            };
            self.update_status(|status| {
                status.phase = Phase::Ready;
                status.active_profile = Some(profile_id.clone());
                status.target_profile = Some(profile_id.clone());
                status.total_tracks = stats.total_tracks;
                status.completed_tracks = stats.embedded_tracks;
                status.stored_vectors = stats.stored_vectors;
                status.stored_bytes = stats.stored_bytes;
                status.current_track = None;
            });
            tokio::time::sleep(PIPELINE_POLL_INTERVAL).await;
        }
    }

    async fn restore_active_index(&self, pool: &PgPool) -> Result<()> {
        let active: Option<String> = sqlx::query_scalar(
            "SELECT profile_id FROM furumusic__similarity_profile
              WHERE active = TRUE LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        let Some(profile_id) = active else {
            return Ok(());
        };
        if read(&self.index).profile_id.as_deref() == Some(&profile_id) {
            return Ok(());
        }
        let entries = load_index(pool, &profile_id).await?;
        *write(&self.index) = Index {
            profile_id: Some(profile_id.clone()),
            entries,
        };
        self.update_status(|status| status.active_profile = Some(profile_id));
        Ok(())
    }

    async fn restore_stored_status(&self, pool: &PgPool) -> Result<()> {
        let active_profile: Option<String> = sqlx::query_scalar(
            "SELECT profile_id FROM furumusic__similarity_profile
              WHERE active = TRUE LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        let stats = storage_stats(pool, active_profile.as_deref().unwrap_or_default()).await?;
        self.update_status(|status| {
            status.active_profile = active_profile;
            status.total_tracks = stats.total_tracks;
            status.completed_tracks = stats.embedded_tracks;
            status.stored_vectors = stats.stored_vectors;
            status.stored_bytes = stats.stored_bytes;
        });
        Ok(())
    }

    fn ensure_generation(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(
            self.generation.load(Ordering::Acquire) == generation,
            "similarity processing superseded by newer settings"
        );
        Ok(())
    }

    async fn ensure_model(&self, spec: &ModelSpec, generation: u64) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.model_dir).await?;
        let path = self.model_dir.join(spec.filename);
        if path.exists() {
            let verify_path = path.clone();
            let expected = spec.sha256.to_owned();
            let valid = tokio::task::spawn_blocking(move || sha256_file(&verify_path))
                .await
                .context("model hash task failed")??
                == expected;
            if valid {
                return Ok(path);
            }
            tokio::fs::remove_file(&path).await?;
        }

        let response = reqwest::get(spec.url).await?.error_for_status()?;
        let temporary = path.with_extension(format!("part-{}-{generation}", std::process::id()));
        let mut file = tokio::fs::File::create(&temporary).await?;
        let mut hasher = Sha256::new();
        let mut received = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            self.ensure_generation(generation)?;
            let chunk = chunk?;
            received = received.saturating_add(chunk.len());
            anyhow::ensure!(
                received <= MAX_MODEL_BYTES,
                "model download exceeds size limit"
            );
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        let actual = format!("{:x}", hasher.finalize());
        if actual != spec.sha256 {
            let _ = tokio::fs::remove_file(&temporary).await;
            anyhow::bail!("downloaded model hash mismatch");
        }
        if let Err(error) = tokio::fs::rename(&temporary, &path).await {
            if path.exists() {
                let _ = tokio::fs::remove_file(&temporary).await;
            } else {
                return Err(error.into());
            }
        }
        Ok(path)
    }

    async fn load_model(&self, profile_id: &str, path: &Path) -> Result<RunnableModel> {
        if let Some((cached_profile, model)) = lock(&self.model).as_ref()
            && cached_profile == profile_id
        {
            return Ok(Arc::clone(model));
        }
        let path = path.to_path_buf();
        let model = tokio::task::spawn_blocking(move || load_onnx(&path))
            .await
            .context("model loading task failed")??;
        *lock(&self.model) = Some((profile_id.to_owned(), Arc::clone(&model)));
        Ok(model)
    }

    async fn pool(&self) -> Result<PgPool> {
        let url = lock(&self.database_url).clone();
        anyhow::ensure!(!url.trim().is_empty(), "database is not configured");
        let pool = self
            .pool
            .get_or_try_init(|| async {
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&url)
                    .await
            })
            .await?;
        Ok(pool.clone())
    }

    fn update_status(&self, update: impl FnOnce(&mut SimilarityStatus)) {
        update(&mut lock(&self.status));
    }
}

pub fn model_by_id(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn profile_by_id(id: &str) -> Option<&'static ProfileSpec> {
    PROFILES.iter().find(|profile| profile.id == id)
}

pub fn profile_details(profile_id: &str, model_id: &str) -> Option<String> {
    let profile = profile_by_id(profile_id)?;
    let dimensions = model_by_id(model_id)
        .map(|model| model.dimensions.to_string())
        .unwrap_or_else(|| "model-defined".to_owned());
    Some(format!(
        "{}\n\nTrack selection:\n• Up to {} seconds: entire track.\n• Longer: 3 × {}-second windows (start, middle, end).\n\nAudio: mono, {} Hz; 16-tap windowed-sinc resampling.\nSpectrogram: Hann window; FFT {}, hop {} samples (16 ms).\nMel: {} Slaney bands, 0–8 kHz, unit-triangle normalization.\nCompression: log10(1 + 10000 × energy).\nPatches: {} frames (~2.05 s), hop {} frames (~0.99 s).\nAggregation: mean of patch embeddings, then L2 normalization.\nOutput dimensions for selected model: {}.\n\nCompatibility includes the exact model version and SHA-256; peers compare only matching profiles.",
        profile.title,
        FULL_TRACK_MAX_SECONDS,
        LONG_TRACK_WINDOW_SECONDS,
        SAMPLE_RATE,
        FRAME_SIZE,
        HOP_SIZE,
        MEL_BANDS,
        PATCH_FRAMES,
        PATCH_HOP,
        dimensions,
    ))
}

pub fn profile_fingerprint(model: &ModelSpec, profile: &str) -> String {
    let contract = format!(
        "furumi-similarity-v1\nmodel={}\nversion={}\nsha256={}\nprofile={}\ninput={}-mono-windowed-sinc16\nselection=full-to-{}s-else-first-middle-last-{}s\nframe={}\nhop={}\nmel=slaney-{}-unit-tri\npatch={}\npatch-hop={}\naggregate=mean-l2\ndimensions={}",
        model.id,
        model.version,
        model.sha256,
        profile,
        SAMPLE_RATE,
        FULL_TRACK_MAX_SECONDS,
        LONG_TRACK_WINDOW_SECONDS,
        FRAME_SIZE,
        HOP_SIZE,
        MEL_BANDS,
        PATCH_FRAMES,
        PATCH_HOP,
        model.dimensions
    );
    format!("sim1:{}", blake3::hash(contract.as_bytes()).to_hex())
}

async fn ensure_similarity_profile(
    pool: &PgPool,
    profile_id: &str,
    model: &ModelSpec,
    preprocessing: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO furumusic__similarity_profile
            (profile_id, model_id, model_version, model_sha256,
             preprocessing, dimensions, active, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, FALSE, $7)
         ON CONFLICT (profile_id) DO NOTHING",
    )
    .bind(profile_id)
    .bind(model.id)
    .bind(model.version)
    .bind(model.sha256)
    .bind(preprocessing)
    .bind(model.dimensions as i32)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn pending_tracks(
    pool: &PgPool,
    profile_id: &str,
    storage_dir: &str,
) -> Result<Vec<SimilarityTrack>> {
    let rows = sqlx::query(
        "SELECT t.id, t.title::text, m.file_path, m.sha256_hash::text,
                c.content_id, t.duration_seconds
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file m ON m.id = t.audio_file_id
           LEFT JOIN furumusic__federation_content_id_cache c
             ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
          WHERE t.is_hidden = FALSE AND r.is_hidden = FALSE
            AND NOT EXISTS (
                SELECT 1 FROM furumusic__track_embedding e
                 WHERE e.track_id = t.id AND e.profile_id = $1
                   AND e.source_sha256 = m.sha256_hash
            )
          ORDER BY t.id",
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| SimilarityTrack {
            id: row.get(0),
            title: row.get(1),
            file_path: crate::media_paths::resolve_media_file_path(
                storage_dir,
                row.get::<String, _>(2).as_str(),
            ),
            source_sha256: row.get(3),
            source_content_id: row.get(4),
            duration_seconds: row.get(5),
        })
        .collect())
}

async fn store_embedding(
    pool: &PgPool,
    track: &SimilarityTrack,
    profile_id: &str,
    vector: &[f32],
) -> Result<()> {
    anyhow::ensure!(!vector.is_empty(), "embedding vector is empty");
    anyhow::ensure!(
        vector.iter().all(|value| value.is_finite()),
        "embedding contains a non-finite value"
    );
    let routing_signature = music_dht::similarity_lsh::routing_signature(vector)?;
    sqlx::query(
        "INSERT INTO furumusic__track_embedding
            (track_id, profile_id, dimensions, vector, routing_signature,
             source_sha256, source_content_id, computed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (track_id, profile_id) DO UPDATE SET
            dimensions = EXCLUDED.dimensions,
            vector = EXCLUDED.vector,
            routing_signature = EXCLUDED.routing_signature,
            source_sha256 = EXCLUDED.source_sha256,
            source_content_id = EXCLUDED.source_content_id,
            computed_at = EXCLUDED.computed_at",
    )
    .bind(track.id)
    .bind(profile_id)
    .bind(vector.len() as i32)
    .bind(embedding_to_bytes(vector))
    .bind(routing_signature.as_slice())
    .bind(&track.source_sha256)
    .bind(&track.source_content_id)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_index(pool: &PgPool, profile_id: &str) -> Result<Vec<StoredEmbedding>> {
    let rows = sqlx::query(
        "SELECT e.track_id, e.dimensions, e.vector,
                COALESCE((
                    SELECT a.name::text
                      FROM furumusic__track_artist ta
                      JOIN furumusic__artist a ON a.id = ta.artist_id
                     WHERE ta.track_id = e.track_id AND ta.role = 'main'
                     ORDER BY ta.position LIMIT 1
                ), ''), c.content_id
           FROM furumusic__track_embedding e
           JOIN furumusic__track t ON t.id = e.track_id
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file m ON m.id = t.audio_file_id
           LEFT JOIN furumusic__federation_content_id_cache c
             ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
          WHERE e.profile_id = $1 AND e.source_sha256 = m.sha256_hash
            AND t.is_hidden = FALSE AND r.is_hidden = FALSE
          ORDER BY e.track_id",
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let dimensions: i32 = row.get(1);
            let bytes: Vec<u8> = row.get(2);
            Ok(StoredEmbedding {
                track_id: row.get(0),
                vector: embedding_from_bytes(dimensions, &bytes)?,
                artist_key: music_dht::normalize_name(&row.get::<String, _>(3)),
                content_id: row.get(4),
            })
        })
        .collect()
}

async fn storage_stats(pool: &PgPool, profile_id: &str) -> Result<StorageStats> {
    let total_tracks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
          WHERE t.is_hidden = FALSE AND r.is_hidden = FALSE",
    )
    .fetch_one(pool)
    .await?;
    let embedded_tracks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__track_embedding e
           JOIN furumusic__track t ON t.id = e.track_id
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file m ON m.id = t.audio_file_id
          WHERE e.profile_id = $1 AND e.source_sha256 = m.sha256_hash
            AND t.is_hidden = FALSE AND r.is_hidden = FALSE",
    )
    .bind(profile_id)
    .fetch_one(pool)
    .await?;
    let row = sqlx::query(
        "SELECT COUNT(*), COALESCE(SUM(octet_length(vector)), 0)
           FROM furumusic__track_embedding",
    )
    .fetch_one(pool)
    .await?;
    let stored_vectors: i64 = row.get(0);
    let stored_bytes: i64 = row.get(1);
    Ok(StorageStats {
        total_tracks: total_tracks.max(0) as usize,
        embedded_tracks: embedded_tracks.max(0) as usize,
        stored_vectors: stored_vectors.max(0) as usize,
        stored_bytes: stored_bytes.max(0) as u64,
    })
}

async fn activate_profile(pool: &PgPool, profile_id: &str) -> Result<()> {
    let mut transaction = pool.begin().await?;
    sqlx::query("UPDATE furumusic__similarity_profile SET active = FALSE WHERE active = TRUE")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE furumusic__similarity_profile SET active = TRUE WHERE profile_id = $1")
        .bind(profile_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

fn embedding_to_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn embedding_from_bytes(dimensions: i32, bytes: &[u8]) -> Result<Vec<f32>> {
    let dimensions = usize::try_from(dimensions).context("negative embedding dimensions")?;
    anyhow::ensure!(
        dimensions > 0 && dimensions <= 4096 && bytes.len() == dimensions * 4,
        "invalid stored embedding dimensions"
    );
    Ok(bytes
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four-byte chunk")))
        .collect())
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn load_onnx(path: &Path) -> Result<RunnableModel> {
    let model = tract_onnx::onnx().model_for_path(path)?;
    let batch = model.sym("batch_size");
    let model = model
        .with_input_fact(0, f32::fact(dims!(batch, PATCH_FRAMES, MEL_BANDS)).into())?
        .into_optimized()?
        .into_runnable()?;
    Ok(model)
}

fn embed_track(model: &RunnableModel, path: &Path, duration_seconds: f64) -> Result<Vec<f32>> {
    let signal = decode_mono_16k(path, duration_seconds)?;
    let mel = mel_spectrogram(&signal)?;
    anyhow::ensure!(
        mel.len() >= PATCH_FRAMES,
        "track is too short for the model"
    );
    let starts: Vec<usize> = (0..=mel.len() - PATCH_FRAMES).step_by(PATCH_HOP).collect();
    let mut sum = vec![0.0f32; EMBEDDING_DIMENSIONS];
    let mut count = 0usize;
    for batch in starts.chunks(MODEL_BATCH) {
        let mut input = vec![0.0f32; MODEL_BATCH * PATCH_FRAMES * MEL_BANDS];
        for (batch_index, &start) in batch.iter().enumerate() {
            let offset = batch_index * PATCH_FRAMES * MEL_BANDS;
            for frame in 0..PATCH_FRAMES {
                let destination = offset + frame * MEL_BANDS;
                input[destination..destination + MEL_BANDS].copy_from_slice(&mel[start + frame]);
            }
        }
        let tensor = Tensor::from_shape(&[MODEL_BATCH, PATCH_FRAMES, MEL_BANDS], &input)?;
        let outputs = model.run(tvec!(tensor.into_tvalue()))?;
        let embedding = outputs
            .iter()
            .find(|output| output.len() == MODEL_BATCH * EMBEDDING_DIMENSIONS)
            .context("model did not return its 1280-dimensional embedding output")?
            .to_plain_array_view::<f32>()?;
        let values = embedding
            .as_slice()
            .context("model embedding output is not contiguous")?;
        for batch_index in 0..batch.len() {
            let row = &values
                [batch_index * EMBEDDING_DIMENSIONS..(batch_index + 1) * EMBEDDING_DIMENSIONS];
            for (total, value) in sum.iter_mut().zip(row) {
                *total += *value;
            }
            count += 1;
        }
    }
    anyhow::ensure!(count > 0, "model produced no patches");
    for value in &mut sum {
        *value /= count as f32;
    }
    normalize(&mut sum)?;
    Ok(sum)
}

fn decode_mono_16k(path: &Path, duration_seconds: f64) -> Result<Vec<f32>> {
    if duration_seconds.is_finite() && duration_seconds > f64::from(FULL_TRACK_MAX_SECONDS) {
        let window = f64::from(LONG_TRACK_WINDOW_SECONDS);
        let starts = [
            0.0,
            (duration_seconds / 2.0 - window / 2.0).max(0.0),
            (duration_seconds - window).max(0.0),
        ];
        let mut selected = Vec::new();
        for start in starts {
            selected.extend(decode_mono_window(path, start, Some(window))?);
        }
        anyhow::ensure!(!selected.is_empty(), "decoded track is empty");
        return Ok(selected);
    }
    decode_mono_window(path, 0.0, None)
}

fn decode_mono_window(
    path: &Path,
    start_seconds: f64,
    length_seconds: Option<f64>,
) -> Result<Vec<f32>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder =
        Decoder::try_from(file).with_context(|| format!("decoding {}", path.display()))?;
    let channels = decoder.channels().get() as usize;
    let source_rate = decoder.sample_rate().get() as usize;
    if start_seconds > 0.0 {
        decoder
            .try_seek(Duration::from_secs_f64(start_seconds))
            .with_context(|| format!("seeking {}", path.display()))?;
    }
    let max_samples =
        length_seconds.map(|seconds| (seconds * source_rate as f64).ceil() as usize * channels);
    let mut mono = Vec::new();
    let mut channel_sum = 0.0f32;
    let mut channel_index = 0usize;
    for (sample_index, sample) in decoder.enumerate() {
        if max_samples.is_some_and(|limit| sample_index >= limit) {
            break;
        }
        channel_sum += sample;
        channel_index += 1;
        if channel_index == channels {
            mono.push(channel_sum / channels as f32);
            channel_sum = 0.0;
            channel_index = 0;
        }
    }
    anyhow::ensure!(!mono.is_empty(), "decoded track is empty");
    if source_rate == SAMPLE_RATE {
        return Ok(mono);
    }
    Ok(resample_sinc(&mono, source_rate, SAMPLE_RATE))
}

fn resample_sinc(input: &[f32], source_rate: usize, target_rate: usize) -> Vec<f32> {
    if input.len() < 2 || source_rate == 0 {
        return input.to_vec();
    }
    let output_len = input
        .len()
        .saturating_mul(target_rate)
        .checked_div(source_rate)
        .unwrap_or(0)
        .max(1);
    let ratio = source_rate as f64 / target_rate as f64;
    let cutoff = (target_rate as f64 / source_rate as f64).min(1.0) * 0.95;
    const HALF_TAPS: isize = 8;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * ratio;
            let center = position.floor() as isize;
            let mut value = 0.0f64;
            let mut weight_sum = 0.0f64;
            for sample_index in center - HALF_TAPS + 1..=center + HALF_TAPS {
                if sample_index < 0 || sample_index >= input.len() as isize {
                    continue;
                }
                let distance = position - sample_index as f64;
                let phase = std::f64::consts::PI * distance * cutoff;
                let sinc = if phase.abs() < 1e-12 {
                    1.0
                } else {
                    phase.sin() / phase
                };
                let window_position = distance / HALF_TAPS as f64;
                let window = if window_position.abs() <= 1.0 {
                    0.5 + 0.5 * (std::f64::consts::PI * window_position).cos()
                } else {
                    0.0
                };
                let weight = cutoff * sinc * window;
                value += input[sample_index as usize] as f64 * weight;
                weight_sum += weight;
            }
            if weight_sum.abs() < 1e-12 {
                input[center.clamp(0, input.len() as isize - 1) as usize]
            } else {
                (value / weight_sum) as f32
            }
        })
        .collect()
}

fn mel_spectrogram(signal: &[f32]) -> Result<Vec<[f32; MEL_BANDS]>> {
    let frame_count = 1 + signal
        .len()
        .saturating_sub(FRAME_SIZE / 2)
        .div_ceil(HOP_SIZE);
    let filters = mel_filters();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);
    let mut mel = Vec::with_capacity(frame_count);
    let mut spectrum = vec![Complex::new(0.0f32, 0.0); FRAME_SIZE];
    for frame_index in 0..frame_count {
        let start = frame_index as isize * HOP_SIZE as isize - (FRAME_SIZE / 2) as isize;
        for (index, value) in spectrum.iter_mut().enumerate() {
            let source = start + index as isize;
            let sample = if source >= 0 {
                signal.get(source as usize).copied().unwrap_or(0.0)
            } else {
                0.0
            };
            let window = 0.5
                - 0.5 * (2.0 * std::f32::consts::PI * index as f32 / (FRAME_SIZE - 1) as f32).cos();
            *value = Complex::new(sample * window, 0.0);
        }
        fft.process(&mut spectrum);
        let powers: Vec<f32> = spectrum[..=FRAME_SIZE / 2]
            .iter()
            .map(|value| value.norm_sqr())
            .collect();
        let mut bands = [0.0f32; MEL_BANDS];
        for (band, weights) in filters.iter().enumerate() {
            let energy: f32 = powers
                .iter()
                .zip(weights)
                .map(|(power, weight)| power * weight)
                .sum();
            bands[band] = (1.0 + 10_000.0 * energy.max(0.0)).log10();
        }
        mel.push(bands);
    }
    Ok(mel)
}

fn mel_filters() -> Vec<Vec<f32>> {
    let low = hz_to_mel_slaney(0.0);
    let high = hz_to_mel_slaney((SAMPLE_RATE / 2) as f32);
    let points: Vec<f32> = (0..MEL_BANDS + 2)
        .map(|index| mel_to_hz_slaney(low + (high - low) * index as f32 / (MEL_BANDS + 1) as f32))
        .collect();
    let frequency_scale = (SAMPLE_RATE as f32 / 2.0) / (FRAME_SIZE / 2) as f32;
    (0..MEL_BANDS)
        .map(|band| {
            let left = points[band];
            let center = points[band + 1];
            let right = points[band + 2];
            let area = ((center - left) + (right - center)) / 2.0;
            (0..=FRAME_SIZE / 2)
                .map(|bin| {
                    let frequency = bin as f32 * frequency_scale;
                    let triangle = if frequency < left || frequency > right {
                        0.0
                    } else if frequency < center {
                        (frequency - left) / (center - left)
                    } else {
                        (right - frequency) / (right - center)
                    };
                    triangle.max(0.0) / area
                })
                .collect()
        })
        .collect()
}

fn hz_to_mel_slaney(hz: f32) -> f32 {
    if hz < 1000.0 {
        hz / (200.0 / 3.0)
    } else {
        15.0 + 27.0 * (hz / 1000.0).ln() / 6.4f32.ln()
    }
}

fn mel_to_hz_slaney(mel: f32) -> f32 {
    if mel < 15.0 {
        mel * (200.0 / 3.0)
    } else {
        1000.0 * (6.4f32.ln() * (mel - 15.0) / 27.0).exp()
    }
}

fn normalize(vector: &mut [f32]) -> Result<()> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    anyhow::ensure!(
        norm.is_finite() && norm > f32::EPSILON,
        "zero or invalid embedding"
    );
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn is_near_duplicate(candidate: &[f32], kept: &[&[f32]]) -> bool {
    kept.iter()
        .any(|existing| dot(candidate, existing) >= NEAR_DUPLICATE_COSINE)
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_fingerprint_matches_the_tui_contract() {
        assert_eq!(
            profile_fingerprint(&MODELS[0], DEFAULT_PROFILE_ID),
            "sim1:9293527b186f2f7e8b3dc2d6b05ce57721299840e80e0e7aaaa922f85c37b0e3"
        );
        assert_ne!(
            profile_fingerprint(&MODELS[0], DEFAULT_PROFILE_ID),
            profile_fingerprint(&MODELS[0], "another-profile")
        );
    }

    #[test]
    fn embedding_bytes_round_trip() {
        let vector = vec![0.1, -0.2, 0.3];
        let bytes = embedding_to_bytes(&vector);
        assert_eq!(embedding_from_bytes(3, &bytes).unwrap(), vector);
        assert!(embedding_from_bytes(4, &bytes).is_err());
    }

    #[test]
    fn near_duplicates_are_filtered() {
        let query = [1.0, 0.0, 0.0];
        let near_duplicate = [0.99995, 0.01, 0.0];
        let distinct = [0.0, 1.0, 0.0];
        assert!(is_near_duplicate(&near_duplicate, &[&query]));
        assert!(!is_near_duplicate(&distinct, &[&query]));
    }

    #[test]
    fn resampling_keeps_a_constant_signal() {
        let output = resample_sinc(&vec![0.25; 441], 44_100, 16_000);
        assert_eq!(output.len(), 160);
        assert!(output.iter().all(|value| (*value - 0.25).abs() < 1e-6));
    }
}
