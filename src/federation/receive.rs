//! Verified federated audio download and trusted materialization.
//!
//! This module never writes inbox, processing-task, or review tables. Peer
//! metadata is the authority for this import path.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use music_dht::EndpointId;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{Federation, now_iso};

const MAX_LINE: usize = 4096;
const MAX_AUDIO_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const ARTWORK_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TrackMetadata {
    title: String,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    featured_artists: Vec<String>,
    #[serde(default)]
    album_artists: Vec<String>,
    #[serde(default)]
    release_title: String,
    release_type: Option<String>,
    year: Option<i32>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
}

#[derive(Serialize)]
struct AudioRequest<'a> {
    item_id: &'a str,
    offset: u64,
    want_cover: bool,
}

#[derive(Deserialize)]
struct AudioHeader {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    total_size: u64,
    #[serde(default)]
    metadata: Option<TrackMetadata>,
    #[serde(default)]
    cover_size: u64,
    #[serde(default)]
    cover_mime: String,
    #[serde(default)]
    artist_image_size: u64,
    #[serde(default)]
    artist_image_mime: String,
}

pub struct PreparedTrack {
    pub local_track_id: Option<i64>,
    pub stream_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub phase: &'static str,
    pub received: u64,
    pub total: u64,
}

struct Downloaded {
    path: PathBuf,
    mime: String,
    metadata: TrackMetadata,
    cover: Option<(Vec<u8>, String)>,
    artist_image: Option<(Vec<u8>, String)>,
}

impl Federation {
    pub async fn discover_catalog_artwork(
        &self,
        artist: &str,
        release: Option<&str>,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let service = self.service().await?;
        let normalized = music_dht::normalize_name(artist);
        let outcome = service
            .search_network(artist)
            .await
            .map_err(|err| anyhow::anyhow!("artwork peer discovery failed: {err}"))?;
        let mut owners = Vec::new();
        for item in outcome.local_results.iter().chain(&outcome.network_results) {
            let matches = (item.kind == music_dht::ItemKind::Artist
                && item.normalized_name == normalized)
                || item
                    .artist_names
                    .iter()
                    .chain(&item.featured_artist_names)
                    .any(|name| music_dht::normalize_name(name) == normalized);
            let owner = item.owner.to_string();
            if matches && !owners.contains(&owner) {
                owners.push(owner);
            }
        }
        for owner in owners {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.catalog_artwork(&owner, artist, release),
            )
            .await
            {
                Ok(Ok(Some(artwork))) => return Ok(Some(artwork)),
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    tracing::debug!(%owner, %artist, ?release, "catalog artwork peer failed: {err:#}");
                }
                Err(_) => {
                    tracing::debug!(%owner, %artist, ?release, "catalog artwork peer timed out");
                }
            }
        }
        Ok(None)
    }

    pub async fn catalog_artwork(
        &self,
        owner: &str,
        artist: &str,
        release: Option<&str>,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let cache_key = format!("catalog:{owner}:{artist}:{}", release.unwrap_or_default());
        if let Some(cached) = cached_artwork(self, &cache_key) {
            return Ok(Some(cached));
        }
        let owner = EndpointId::from_str(owner).context("invalid federation owner")?;
        let service = self.service().await?;
        let mut stream = service
            .open_stream(owner, super::CATALOG_ALPN)
            .await
            .map_err(|err| anyhow::anyhow!("cannot reach catalog peer: {err}"))?;
        let mut request = serde_json::to_vec(&music_dht::catalog::CatalogRequest {
            artist: artist.to_owned(),
            want: Some(if release.is_some() {
                "release_cover".to_owned()
            } else {
                "artist_image".to_owned()
            }),
            release: release.map(str::to_owned),
            ..Default::default()
        })?;
        request.push(b'\n');
        stream.send.write_all(&request).await?;
        stream.send.finish()?;
        let header: music_dht::catalog::CatalogImageHeader =
            serde_json::from_slice(&read_line(&mut stream.recv).await?)
                .context("invalid catalog artwork response")?;
        if !header.ok || header.size == 0 {
            return Ok(None);
        }
        anyhow::ensure!(
            header.size <= MAX_IMAGE_BYTES,
            "catalog artwork is too large"
        );
        anyhow::ensure!(
            header.mime_type.starts_with("image/"),
            "invalid catalog artwork mime type"
        );
        let mut bytes = vec![0; header.size as usize];
        stream.recv.read_exact(&mut bytes).await?;
        let artwork = (bytes, header.mime_type);
        cache_artwork(self, cache_key, &artwork);
        Ok(Some(artwork))
    }

    pub async fn track_artwork(
        &self,
        owner: &str,
        item_id: &str,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let cache_key = format!("{owner}:{item_id}");
        if let Some(cached) = cached_artwork(self, &cache_key) {
            return Ok(Some(cached));
        }
        let owner = EndpointId::from_str(owner).context("invalid federation owner")?;
        anyhow::ensure!(item_id.len() == 64, "invalid federation item id");
        let service = self.service().await?;
        let mut stream = service
            .open_stream(owner, super::AUDIO_ALPN)
            .await
            .map_err(|err| anyhow::anyhow!("cannot reach track owner: {err}"))?;
        write_line(
            &mut stream.send,
            &AudioRequest {
                item_id,
                offset: 0,
                want_cover: true,
            },
        )
        .await?;
        stream.send.finish()?;
        let header: AudioHeader = serde_json::from_slice(&read_line(&mut stream.recv).await?)
            .context("invalid artwork response")?;
        anyhow::ensure!(
            header.ok,
            "peer refused artwork: {}",
            header.error.unwrap_or_else(|| "unknown error".to_string())
        );
        let artwork = read_segment(
            &mut stream.recv,
            header.cover_size,
            &header.cover_mime,
            "cover",
        )
        .await?;
        if let Some((bytes, mime)) = artwork {
            let artwork = (bytes, mime);
            cache_artwork(self, cache_key, &artwork);
            Ok(Some(artwork))
        } else {
            Ok(None)
        }
    }

    pub async fn prepare_discovered_content_with_progress<F>(
        self: &std::sync::Arc<Self>,
        content_id: &str,
        progress: F,
    ) -> Result<PreparedTrack>
    where
        F: FnMut(DownloadProgress) + Send,
    {
        let content_id =
            music_dht::normalize_content_id(content_id).context("invalid content id")?;
        let service = self.service().await?;
        let outcome = service.search_content_id(&content_id).await?;
        let item = outcome
            .local_results
            .into_iter()
            .chain(outcome.network_results)
            .find(|item| {
                item.kind == music_dht::ItemKind::Track
                    && item.content_id.as_deref() == Some(content_id.as_str())
            })
            .context("no peer currently advertises this content id")?;
        self.prepare_content_with_progress(
            &content_id,
            &item.owner.to_string(),
            &hex_encode(item.id.as_bytes()),
            progress,
        )
        .await
    }

    pub async fn prepare_content_with_progress<F>(
        self: &std::sync::Arc<Self>,
        content_id: &str,
        owner: &str,
        item_id: &str,
        mut progress: F,
    ) -> Result<PreparedTrack>
    where
        F: FnMut(DownloadProgress) + Send,
    {
        progress(DownloadProgress {
            phase: "checking",
            received: 0,
            total: 0,
        });
        let content_id =
            music_dht::normalize_content_id(content_id).context("invalid content id")?;
        let pool = self.pool().await?;
        let token = content_id.trim_start_matches("b3:").to_owned();
        let download_lock = {
            let mut locks = super::lock(&self.download_locks);
            std::sync::Arc::clone(
                locks
                    .entry(content_id.clone())
                    .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _download_guard = download_lock.lock().await;
        if let Some(track_id) = local_track_id(&pool, &content_id).await? {
            progress(DownloadProgress {
                phase: "ready",
                received: 1,
                total: 1,
            });
            return Ok(PreparedTrack {
                local_track_id: Some(track_id),
                stream_url: format!("/api/player/stream/{track_id}"),
            });
        }

        let save = self
            .save_on_listen
            .load(std::sync::atomic::Ordering::Relaxed);
        if !save
            && let Some((_path, _mime)) = super::lock(&self.prepared_cache).get(&token).cloned()
        {
            return Ok(PreparedTrack {
                local_track_id: None,
                stream_url: format!("/api/player/federation/cache/{token}"),
            });
        }
        let owner = EndpointId::from_str(owner).context("invalid federation owner")?;
        anyhow::ensure!(item_id.len() == 64, "invalid federation item id");
        let service = self.service().await?;
        let storage_root = if save {
            PathBuf::from(super::lock(&self.storage_dir).clone())
        } else {
            PathBuf::from(crate::media_paths::resolve_config_path("federation-cache"))
        };
        anyhow::ensure!(
            !storage_root.as_os_str().is_empty(),
            "media storage directory is not configured"
        );
        let dir = storage_root.join("federation");
        tokio::fs::create_dir_all(&dir).await?;
        let downloaded =
            match download(&service, owner, item_id, &content_id, &dir, &mut progress).await {
                Ok(downloaded) => downloaded,
                Err(primary_error) => {
                    progress(DownloadProgress {
                        phase: "discovering",
                        received: 0,
                        total: 0,
                    });
                    let outcome = service
                        .search_content_id(&content_id)
                        .await
                        .map_err(|err| {
                            anyhow::anyhow!("{primary_error:#}; content lookup also failed: {err}")
                        })?;
                    let mut last_error = primary_error;
                    let mut downloaded = None;
                    for item in outcome
                        .local_results
                        .into_iter()
                        .chain(outcome.network_results)
                    {
                        if item.kind != music_dht::ItemKind::Track
                            || item.content_id.as_deref() != Some(content_id.as_str())
                        {
                            continue;
                        }
                        let candidate_item_id = hex_encode(item.id.as_bytes());
                        if item.owner == owner && candidate_item_id == item_id {
                            continue;
                        }
                        match download(
                            &service,
                            item.owner,
                            &candidate_item_id,
                            &content_id,
                            &dir,
                            &mut progress,
                        )
                        .await
                        {
                            Ok(candidate) => {
                                downloaded = Some(candidate);
                                break;
                            }
                            Err(err) => last_error = err,
                        }
                    }
                    downloaded.ok_or_else(|| {
                        anyhow::anyhow!(
                            "no reachable peer currently provides this track: {last_error:#}"
                        )
                    })?
                }
            };
        if !save {
            super::lock(&self.prepared_cache)
                .insert(token.clone(), (downloaded.path, downloaded.mime));
            return Ok(PreparedTrack {
                local_track_id: None,
                stream_url: format!("/api/player/federation/cache/{token}"),
            });
        }
        progress(DownloadProgress {
            phase: "saving",
            received: 0,
            total: 0,
        });
        let track_id = materialize(&pool, &storage_root, &content_id, downloaded).await?;
        // Materialization is the playback boundary: return the local track
        // immediately so the browser can replace the pending queue entry and
        // start it. Publishing must not keep the prepare stream stuck at 100%
        // when a federation peer is slow or offline.
        let federation = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = federation.sync_now().await {
                tracing::warn!(track_id, "post-import federation publish failed: {err:#}");
            }
        });
        Ok(PreparedTrack {
            local_track_id: Some(track_id),
            stream_url: format!("/api/player/stream/{track_id}"),
        })
    }

    pub fn prepared_cache_file(&self, token: &str) -> Option<(PathBuf, String)> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        super::lock(&self.prepared_cache).get(token).cloned()
    }
}

fn cached_artwork(federation: &Federation, key: &str) -> Option<(Vec<u8>, String)> {
    let mut cache = super::lock(&federation.artwork_cache);
    let cached = cache.get(key).cloned()?;
    if cached.fetched_at.elapsed() > ARTWORK_CACHE_TTL {
        cache.remove(key);
        return None;
    }
    Some((cached.bytes, cached.mime))
}

fn cache_artwork(federation: &Federation, key: String, artwork: &(Vec<u8>, String)) {
    let mut cache = super::lock(&federation.artwork_cache);
    if cache.len() >= 512 {
        cache.retain(|_, value| value.fetched_at.elapsed() <= ARTWORK_CACHE_TTL);
        if cache.len() >= 512
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, value)| value.fetched_at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        key,
        super::CachedArtwork {
            bytes: artwork.0.clone(),
            mime: artwork.1.clone(),
            fetched_at: std::time::Instant::now(),
        },
    );
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn download(
    service: &music_dht::MusicDhtService,
    owner: EndpointId,
    item_id: &str,
    content_id: &str,
    dir: &Path,
    progress: &mut (impl FnMut(DownloadProgress) + Send),
) -> Result<Downloaded> {
    progress(DownloadProgress {
        phase: "connecting",
        received: 0,
        total: 0,
    });
    let mut stream = service
        .open_stream(owner, super::AUDIO_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach track owner: {err}"))?;
    write_line(
        &mut stream.send,
        &AudioRequest {
            item_id,
            offset: 0,
            want_cover: true,
        },
    )
    .await?;
    stream.send.finish()?;
    let header: AudioHeader = serde_json::from_slice(&read_line(&mut stream.recv).await?)
        .context("invalid audio response")?;
    anyhow::ensure!(
        header.ok,
        "peer refused audio: {}",
        header.error.unwrap_or_else(|| "unknown error".to_string())
    );
    anyhow::ensure!(
        header.total_size > 0 && header.total_size <= MAX_AUDIO_BYTES,
        "invalid federated audio size"
    );
    progress(DownloadProgress {
        phase: "downloading",
        received: 0,
        total: header.total_size,
    });
    let metadata = header.metadata.context("peer returned no track metadata")?;
    let cover = read_segment(
        &mut stream.recv,
        header.cover_size,
        &header.cover_mime,
        "cover",
    )
    .await?;
    let artist_image = read_segment(
        &mut stream.recv,
        header.artist_image_size,
        &header.artist_image_mime,
        "artist image",
    )
    .await?;

    let stem = content_id.trim_start_matches("b3:");
    let extension = audio_extension(&header.mime_type);
    let final_path = dir.join(format!("{stem}.{extension}"));
    let part_path = dir.join(format!(".{stem}.{extension}.part"));
    let mut file = tokio::fs::File::create(&part_path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut received = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while received < header.total_size {
        let remaining = (header.total_size - received).min(buf.len() as u64) as usize;
        let count = stream.recv.read(&mut buf[..remaining]).await?.unwrap_or(0);
        anyhow::ensure!(count > 0, "audio stream ended early");
        file.write_all(&buf[..count]).await?;
        hasher.update(&buf[..count]);
        received += count as u64;
        progress(DownloadProgress {
            phase: "downloading",
            received,
            total: header.total_size,
        });
    }
    file.flush().await?;
    drop(file);
    let actual = format!("b3:{}", hasher.finalize().to_hex());
    progress(DownloadProgress {
        phase: "verifying",
        received,
        total: header.total_size,
    });
    if actual != content_id {
        let _ = tokio::fs::remove_file(&part_path).await;
        anyhow::bail!("downloaded audio content id mismatch");
    }
    tokio::fs::rename(&part_path, &final_path).await?;
    Ok(Downloaded {
        path: final_path,
        mime: header.mime_type,
        metadata,
        cover,
        artist_image,
    })
}

async fn materialize(
    pool: &sqlx::PgPool,
    storage_root: &Path,
    content_id: &str,
    downloaded: Downloaded,
) -> Result<i64> {
    if let Some(track_id) = local_track_id(pool, content_id).await? {
        return Ok(track_id);
    }
    let bytes = tokio::fs::read(&downloaded.path).await?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let relative = downloaded
        .path
        .strip_prefix(storage_root)
        .unwrap_or(&downloaded.path)
        .to_string_lossy()
        .into_owned();
    let mut tx = pool.begin().await?;
    let existing_media: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__media_file
          WHERE file_type = 'audio' AND sha256_hash = $1 LIMIT 1",
    )
    .bind(&sha256)
    .fetch_optional(&mut *tx)
    .await?;
    let media_id = match existing_media {
        Some(id) => id,
        None => {
            sqlx::query_scalar(
                "INSERT INTO furumusic__media_file
                    (file_type, file_path, original_filename, mime_type,
                     file_size_bytes, sha256_hash, audio_format,
                     uploaded_by_user_id, uploader_name, created_at)
                 VALUES ('audio', $1, $2, $3, $4, $5, $6, NULL, 'Federation', $7)
                 RETURNING id",
            )
            .bind(&relative)
            .bind(
                downloaded
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("federated-audio"),
            )
            .bind(&downloaded.mime)
            .bind(bytes.len() as i64)
            .bind(&sha256)
            .bind(audio_extension(&downloaded.mime))
            .bind(now_iso())
            .fetch_one(&mut *tx)
            .await?
        }
    };
    sqlx::query(
        "INSERT INTO furumusic__federation_content_id_cache
            (media_file_id, sha256_hash, content_id, updated_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (media_file_id) DO UPDATE SET
            sha256_hash = EXCLUDED.sha256_hash,
            content_id = EXCLUDED.content_id,
            updated_at = EXCLUDED.updated_at",
    )
    .bind(media_id)
    .bind(&sha256)
    .bind(content_id)
    .bind(now_iso())
    .execute(&mut *tx)
    .await?;
    if let Some(track_id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM furumusic__track WHERE audio_file_id = $1 LIMIT 1",
    )
    .bind(media_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(track_id);
    }

    let release_title = nonempty(&downloaded.metadata.release_title).unwrap_or("Unknown release");
    let release_sort = music_dht::normalize_name(release_title);
    let release_id: i64 = if let Some(id) = sqlx::query_scalar(
        "SELECT id FROM furumusic__release
          WHERE title_sort = $1 AND year IS NOT DISTINCT FROM $2
          ORDER BY id LIMIT 1",
    )
    .bind(&release_sort)
    .bind(downloaded.metadata.year)
    .fetch_optional(&mut *tx)
    .await?
    {
        id
    } else {
        sqlx::query_scalar(
            "INSERT INTO furumusic__release
                (title, title_sort, release_type, year, is_hidden, model_name,
                 created_at, updated_at)
             VALUES ($1, $2, $3, $4, false, NULL, $5, $5)
             RETURNING id",
        )
        .bind(release_title)
        .bind(&release_sort)
        .bind(
            downloaded
                .metadata
                .release_type
                .as_deref()
                .unwrap_or("album"),
        )
        .bind(downloaded.metadata.year)
        .bind(now_iso())
        .fetch_one(&mut *tx)
        .await?
    };
    let duration: f64 = sqlx::query_scalar(
        "SELECT COALESCE(duration_seconds, 0)
           FROM furumusic__track_ref WHERE content_id = $1",
    )
    .bind(content_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(0.0);
    let track_id: i64 = sqlx::query_scalar(
        "INSERT INTO furumusic__track
            (title, title_sort, release_id, track_number, disc_number,
             duration_seconds, audio_file_id, year, is_hidden, model_name,
             created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, false, NULL, $9, $9)
         RETURNING id",
    )
    .bind(&downloaded.metadata.title)
    .bind(music_dht::normalize_name(&downloaded.metadata.title))
    .bind(release_id)
    .bind(downloaded.metadata.track_number)
    .bind(downloaded.metadata.disc_number)
    .bind(duration)
    .bind(media_id)
    .bind(downloaded.metadata.year)
    .bind(now_iso())
    .fetch_one(&mut *tx)
    .await?;

    let main_artists = if downloaded.metadata.artists.is_empty() {
        &downloaded.metadata.album_artists
    } else {
        &downloaded.metadata.artists
    };
    let mut main_artist_ids = Vec::new();
    for (position, name) in main_artists.iter().enumerate() {
        let artist_id = ensure_artist(&mut tx, name).await?;
        main_artist_ids.push(artist_id);
        link_track_artist(&mut tx, track_id, artist_id, "main", position as i32).await?;
        link_release_artist(&mut tx, release_id, artist_id, position as i32).await?;
    }
    for (position, name) in downloaded.metadata.featured_artists.iter().enumerate() {
        let artist_id = ensure_artist(&mut tx, name).await?;
        link_track_artist(&mut tx, track_id, artist_id, "featuring", position as i32).await?;
    }
    sqlx::query(
        "UPDATE furumusic__track_ref
            SET local_track_id = $2, metadata_authority = 'federation', updated_at = $3
          WHERE content_id = $1",
    )
    .bind(content_id)
    .bind(track_id)
    .bind(now_iso())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE furumusic__fed_state_like
            SET local_track_id = $2 WHERE content_id = $1",
    )
    .bind(content_id)
    .bind(track_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE furumusic__fed_state_playlist_item
            SET local_track_id = $2 WHERE content_id = $1",
    )
    .bind(content_id)
    .bind(track_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    super::devices::materialize_content_state(pool, content_id, track_id).await?;

    // Artwork is non-authoritative for identity and may be installed after
    // the audio transaction. Failure does not invalidate a verified track.
    if let Some((bytes, mime)) = downloaded.cover
        && let Err(err) =
            install_release_artwork(pool, storage_root, release_id, &bytes, &mime).await
    {
        tracing::warn!(
            release_id,
            "failed to install federated release artwork: {err:#}"
        );
    }
    if let Some((bytes, mime)) = downloaded.artist_image
        && let Err(err) =
            install_artist_artwork(pool, storage_root, &main_artist_ids, &bytes, &mime).await
    {
        tracing::warn!("failed to install federated artist artwork: {err:#}");
    }
    Ok(track_id)
}

async fn install_release_artwork(
    pool: &sqlx::PgPool,
    storage_root: &Path,
    release_id: i64,
    bytes: &[u8],
    mime: &str,
) -> Result<()> {
    let media_id = persist_artwork(pool, storage_root, bytes, mime).await?;
    sqlx::query(
        "UPDATE furumusic__release
            SET cover_file_id = $1, updated_at = $3
          WHERE id = $2 AND cover_file_id IS NULL",
    )
    .bind(media_id)
    .bind(release_id)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn install_artist_artwork(
    pool: &sqlx::PgPool,
    storage_root: &Path,
    artist_ids: &[i64],
    bytes: &[u8],
    mime: &str,
) -> Result<()> {
    if artist_ids.is_empty() {
        return Ok(());
    }
    let media_id = persist_artwork(pool, storage_root, bytes, mime).await?;
    sqlx::query(
        "UPDATE furumusic__artist
            SET image_file_id = $1, updated_at = $3
          WHERE id = ANY($2) AND image_file_id IS NULL",
    )
    .bind(media_id)
    .bind(artist_ids)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn persist_artwork(
    pool: &sqlx::PgPool,
    storage_root: &Path,
    bytes: &[u8],
    mime: &str,
) -> Result<i64> {
    anyhow::ensure!(!bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES);
    anyhow::ensure!(mime.starts_with("image/"), "invalid artwork mime type");
    let hash = format!("{:x}", Sha256::digest(bytes));
    if let Some(id) = sqlx::query_scalar(
        "SELECT id FROM furumusic__media_file
          WHERE file_type = 'cover_art' AND sha256_hash = $1 LIMIT 1",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?
    {
        return Ok(id);
    }
    let extension = image_extension(mime);
    let filename = format!("federation-artwork-{}.{}", &hash[..16], extension);
    let dir = storage_root.join("federation").join("artwork");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(&filename);
    tokio::fs::write(&path, bytes).await?;
    let relative = path
        .strip_prefix(storage_root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    let id = sqlx::query_scalar(
        "INSERT INTO furumusic__media_file
            (file_type, file_path, original_filename, mime_type,
             file_size_bytes, sha256_hash, uploaded_by_user_id,
             uploader_name, created_at)
         VALUES ('cover_art', $1, $2, $3, $4, $5, NULL, 'Federation', $6)
         RETURNING id",
    )
    .bind(&relative)
    .bind(&filename)
    .bind(mime)
    .bind(bytes.len() as i64)
    .bind(&hash)
    .bind(now_iso())
    .fetch_one(pool)
    .await?;
    if let Err(err) = crate::agent::cover_variants::ensure_cover_variants(&path).await {
        tracing::warn!(
            media_id = id,
            "failed to generate federated artwork variants: {err}"
        );
    }
    Ok(id)
}

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        _ => "jpg",
    }
}

async fn ensure_artist(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, name: &str) -> Result<i64> {
    let normalized = music_dht::normalize_name(name);
    if let Some(id) = sqlx::query_scalar(
        "SELECT id FROM furumusic__artist WHERE name_sort = $1 ORDER BY id LIMIT 1",
    )
    .bind(&normalized)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO furumusic__artist
            (name, name_sort, is_hidden, model_name, created_at, updated_at)
         VALUES ($1, $2, false, NULL, $3, $3) RETURNING id",
    )
    .bind(name)
    .bind(normalized)
    .bind(now_iso())
    .fetch_one(&mut **tx)
    .await?)
}

async fn link_track_artist(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    track_id: i64,
    artist_id: i64,
    role: &str,
    position: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO furumusic__track_artist
            (track_id, artist_id, role, position) VALUES ($1, $2, $3, $4)",
    )
    .bind(track_id)
    .bind(artist_id)
    .bind(role)
    .bind(position)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn link_release_artist(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    release_id: i64,
    artist_id: i64,
    position: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO furumusic__release_artist
            (release_id, artist_id, position)
         SELECT $1, $2, $3 WHERE NOT EXISTS (
            SELECT 1 FROM furumusic__release_artist
             WHERE release_id = $1 AND artist_id = $2
         )",
    )
    .bind(release_id)
    .bind(artist_id)
    .bind(position)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn local_track_id(pool: &sqlx::PgPool, content_id: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT t.id
           FROM furumusic__federation_content_id_cache c
           JOIN furumusic__track t ON t.audio_file_id = c.media_file_id
          WHERE c.content_id = $1 AND t.is_hidden = false LIMIT 1",
    )
    .bind(content_id)
    .fetch_optional(pool)
    .await?)
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        anyhow::ensure!(
            reader.read_exact(&mut byte).await.is_ok(),
            "stream ended early"
        );
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        anyhow::ensure!(line.len() <= MAX_LINE, "protocol line too large");
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

async fn read_segment<R: AsyncRead + Unpin>(
    reader: &mut R,
    size: u64,
    mime: &str,
    label: &str,
) -> Result<Option<(Vec<u8>, String)>> {
    if size == 0 {
        return Ok(None);
    }
    anyhow::ensure!(size <= MAX_IMAGE_BYTES, "{label} is too large");
    let mut bytes = vec![0u8; size as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(Some((bytes, mime.to_owned())))
}

fn audio_extension(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" => "wav",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        _ => "bin",
    }
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value.trim())
}
