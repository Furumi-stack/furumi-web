//! Serve side of the federation wire protocols (audio + catalog), backed by
//! the PostgreSQL library and the media storage directory. Wire compatible
//! with the furumi TUI client and any other furumi peer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
pub use music_dht::catalog::CATALOG_ALPN;
use music_dht::catalog::{
    CatalogArtist, CatalogArtistPreview, CatalogImageHeader as ImageHeader, CatalogRelease,
    CatalogRequest, CatalogResponse, CatalogTrack,
};
use music_dht::{ByteStream, EndpointId, ItemId, ItemKind, StreamAcceptor, normalize_name};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{TransportStats, record_stream_transport};

/// ALPN of the peer-to-peer audio streaming protocol.
pub const AUDIO_ALPN: &[u8] = b"furumi-fd/audio/1";

/// Maximum size of a JSON protocol line (request or response header).
const MAX_PROTOCOL_LINE: usize = 4096;
/// Images above this size are skipped rather than transferred.
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire shapes (shared with the furumi TUI client)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AudioRequest {
    item_id: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    want_cover: bool,
}

#[derive(Debug, Default, Serialize)]
struct AudioResponseHeader {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    mime_type: String,
    total_size: u64,
    offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<TrackMetadata>,
    cover_size: u64,
    cover_mime: String,
    artist_image_size: u64,
    artist_image_mime: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TrackMetadata {
    title: String,
    artists: Vec<String>,
    featured_artists: Vec<String>,
    album_artists: Vec<String>,
    release_title: String,
    release_type: Option<String>,
    year: Option<i32>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
}

// ---------------------------------------------------------------------------
// Framing helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode_item_id(value: &str) -> Option<ItemId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(ItemId::from_bytes(bytes))
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("stream ended before the protocol line was complete");
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > MAX_PROTOCOL_LINE {
            anyhow::bail!("protocol line exceeds {MAX_PROTOCOL_LINE} bytes");
        }
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

fn item_id_of(own: &EndpointId, track_id: i64) -> String {
    hex_encode(ItemId::derive(own, ItemKind::Track, &format!("track:{track_id}")).as_bytes())
}

fn resolve_media_path(storage_dir: &str, file_path: &str) -> PathBuf {
    crate::media_paths::resolve_media_file_path(storage_dir, file_path)
}

fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "m4a" | "mp4" | "alac" => "audio/mp4",
        "aac" => "audio/aac",
        "aiff" | "aif" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

/// Reads an image media file from disk, bounded by [`MAX_IMAGE_BYTES`].
async fn read_image(
    storage_dir: &str,
    media: Option<(String, String)>,
) -> Option<(Vec<u8>, String)> {
    let (file_path, mime) = media?;
    let path = resolve_media_path(storage_dir, &file_path);
    let size = tokio::fs::metadata(&path).await.ok()?.len();
    if size == 0 || size > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let mime = if mime.trim().is_empty() {
        "image/jpeg".to_string()
    } else {
        mime
    };
    Some((bytes, mime))
}

// ---------------------------------------------------------------------------
// Library lookups (PostgreSQL)
// ---------------------------------------------------------------------------

/// Finds the visible track whose derived DHT item id matches `item_id`.
async fn resolve_track_id(pool: &PgPool, own: &EndpointId, item_id: ItemId) -> Result<Option<i64>> {
    let rows = sqlx::query(
        "SELECT t.id FROM furumusic__track t
         JOIN furumusic__release r ON r.id = t.release_id
         WHERE t.is_hidden = false AND r.is_hidden = false",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let track_id: i64 = row.get(0);
        if ItemId::derive(own, ItemKind::Track, &format!("track:{track_id}")) == item_id {
            return Ok(Some(track_id));
        }
    }
    Ok(None)
}

/// (file_path, mime_type) of the track's audio media file.
async fn track_audio_file(pool: &PgPool, track_id: i64) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.file_path, m.mime_type FROM furumusic__track t
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         WHERE t.id = $1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

/// Track cover (falling back to the release cover) as (file_path, mime).
async fn track_cover_file(pool: &PgPool, track_id: i64) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.file_path, m.mime_type FROM furumusic__track t
         JOIN furumusic__release r ON r.id = t.release_id
         JOIN furumusic__media_file m ON m.id = COALESCE(t.cover_file_id, r.cover_file_id)
         WHERE t.id = $1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

/// The main artist's image of a track as (file_path, mime).
async fn track_artist_image_file(pool: &PgPool, track_id: i64) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.file_path, m.mime_type FROM furumusic__track_artist ta
         JOIN furumusic__artist a ON a.id = ta.artist_id
         JOIN furumusic__media_file m ON m.id = a.image_file_id
         WHERE ta.track_id = $1 AND ta.role = 'main'
         ORDER BY ta.position LIMIT 1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

async fn track_catalog_artist_names(
    pool: &PgPool,
    track_id: i64,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut artists = Vec::new();
    let mut featured = Vec::new();
    let rows = sqlx::query(
        "SELECT a.name, ta.role FROM furumusic__track_artist ta
         JOIN furumusic__artist a ON a.id = ta.artist_id
         WHERE ta.track_id = $1 ORDER BY ta.position",
    )
    .bind(track_id)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let name: String = row.get(0);
        match row.get::<String, _>(1).as_str() {
            "featuring" => featured.push(name),
            "main" => artists.push(name),
            _ => {}
        }
    }
    Ok((artists, featured))
}

async fn track_metadata(pool: &PgPool, track_id: i64) -> Result<Option<TrackMetadata>> {
    let Some(track) = sqlx::query(
        "SELECT t.title, t.track_number, t.disc_number, COALESCE(t.year, r.year),
                t.release_id, r.title, r.release_type
         FROM furumusic__track t
         JOIN furumusic__release r ON r.id = t.release_id
         WHERE t.id = $1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let release_id: i64 = track.get(4);

    let mut artists = Vec::new();
    let mut featured = Vec::new();
    let artist_rows = sqlx::query(
        "SELECT a.name, ta.role FROM furumusic__track_artist ta
         JOIN furumusic__artist a ON a.id = ta.artist_id
         WHERE ta.track_id = $1 ORDER BY ta.position",
    )
    .bind(track_id)
    .fetch_all(pool)
    .await?;
    for row in artist_rows {
        let name: String = row.get(0);
        match row.get::<String, _>(1).as_str() {
            "featuring" => featured.push(name),
            "main" => artists.push(name),
            _ => {}
        }
    }
    let album_artists: Vec<String> = sqlx::query(
        "SELECT a.name FROM furumusic__release_artist ra
         JOIN furumusic__artist a ON a.id = ra.artist_id
         WHERE ra.release_id = $1 ORDER BY ra.position",
    )
    .bind(release_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.get(0))
    .collect();

    Ok(Some(TrackMetadata {
        title: track.get(0),
        artists,
        featured_artists: featured,
        album_artists,
        release_title: track.get(5),
        release_type: Some(track.get(6)),
        year: track.get(3),
        track_number: track.get(1),
        disc_number: track.get(2),
    }))
}

// ---------------------------------------------------------------------------
// Audio protocol
// ---------------------------------------------------------------------------

/// Runs the audio accept loop until the acceptor closes. Every visible
/// track of the library is streamable by every peer of the network.
pub async fn serve_audio(
    mut acceptor: StreamAcceptor,
    pool: PgPool,
    storage_dir: String,
    own: EndpointId,
    transport_stats: Arc<TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let pool = pool.clone();
        let storage_dir = storage_dir.clone();
        let transport_stats = Arc::clone(&transport_stats);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_audio_one(stream, pool, storage_dir, own, transport_stats).await
            {
                tracing::warn!(peer = %peer, "federation audio stream failed: {err:#}");
            }
        });
    }
}

async fn serve_audio_one(
    mut stream: ByteStream,
    pool: PgPool,
    storage_dir: String,
    own: EndpointId,
    transport_stats: Arc<TransportStats>,
) -> Result<()> {
    record_stream_transport(&transport_stats, "audio", "inbound", "open", &stream);
    let request: AudioRequest = serde_json::from_slice(&read_line(&mut stream.recv).await?)?;
    tracing::info!(
        peer = %stream.peer_id,
        item = %request.item_id,
        offset = request.offset,
        "federation peer requested audio"
    );

    let track_id = match hex_decode_item_id(&request.item_id) {
        Some(item_id) => match resolve_track_id(&pool, &own, item_id).await {
            Ok(Some(track_id)) => track_id,
            Ok(None) => return refuse_audio(stream, "track not found in the library").await,
            Err(err) => {
                return refuse_audio(stream, &format!("library lookup failed: {err:#}")).await;
            }
        },
        None => return refuse_audio(stream, "malformed item_id").await,
    };

    let Some((file_path, mime_type)) = track_audio_file(&pool, track_id).await? else {
        return refuse_audio(stream, "audio file record is missing").await;
    };
    let path = resolve_media_path(&storage_dir, &file_path);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => {
            return refuse_audio(stream, &format!("audio file is not readable: {err}")).await;
        }
    };
    let total_size = file.metadata().await?.len();
    let offset = request.offset.min(total_size);
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
    }

    let metadata = match track_metadata(&pool, track_id).await {
        Ok(metadata) => metadata,
        Err(err) => {
            tracing::warn!(track_id, "federation metadata lookup failed: {err:#}");
            None
        }
    };
    let (cover, artist_image) = if request.want_cover {
        (
            read_image(
                &storage_dir,
                track_cover_file(&pool, track_id).await.ok().flatten(),
            )
            .await,
            read_image(
                &storage_dir,
                track_artist_image_file(&pool, track_id)
                    .await
                    .ok()
                    .flatten(),
            )
            .await,
        )
    } else {
        (None, None)
    };

    let mime_type = if mime_type.trim().is_empty() {
        guess_mime(&path).to_string()
    } else {
        mime_type
    };
    write_line(
        &mut stream.send,
        &AudioResponseHeader {
            ok: true,
            error: None,
            mime_type,
            total_size,
            offset,
            metadata,
            cover_size: cover.as_ref().map_or(0, |(bytes, _)| bytes.len() as u64),
            cover_mime: cover
                .as_ref()
                .map(|(_, mime)| mime.clone())
                .unwrap_or_default(),
            artist_image_size: artist_image
                .as_ref()
                .map_or(0, |(bytes, _)| bytes.len() as u64),
            artist_image_mime: artist_image
                .as_ref()
                .map(|(_, mime)| mime.clone())
                .unwrap_or_default(),
        },
    )
    .await?;
    if let Some((bytes, _)) = &cover {
        stream.send.write_all(bytes).await?;
    }
    if let Some((bytes, _)) = &artist_image {
        stream.send.write_all(bytes).await?;
    }
    tokio::io::copy(&mut file, &mut stream.send).await?;
    stream.send.finish()?;
    // Wait until the peer read everything before dropping the stream,
    // otherwise the tail of the file is lost.
    let _ = stream.send.stopped().await;
    record_stream_transport(&transport_stats, "audio", "inbound", "done", &stream);
    Ok(())
}

async fn refuse_audio(mut stream: ByteStream, message: &str) -> Result<()> {
    write_line(
        &mut stream.send,
        &AudioResponseHeader {
            ok: false,
            error: Some(message.to_string()),
            ..AudioResponseHeader::default()
        },
    )
    .await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    anyhow::bail!("refused audio request: {message}");
}

// ---------------------------------------------------------------------------
// Catalog protocol
// ---------------------------------------------------------------------------

/// Runs the catalog accept loop until the acceptor closes.
pub async fn serve_catalog(
    mut acceptor: StreamAcceptor,
    pool: PgPool,
    storage_dir: String,
    own: EndpointId,
    transport_stats: Arc<TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let pool = pool.clone();
        let storage_dir = storage_dir.clone();
        let transport_stats = Arc::clone(&transport_stats);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) =
                serve_catalog_one(stream, pool, storage_dir, own, transport_stats).await
            {
                tracing::warn!(peer = %peer, "federation catalog request failed: {err:#}");
            }
        });
    }
}

async fn serve_catalog_one(
    mut stream: ByteStream,
    pool: PgPool,
    storage_dir: String,
    own: EndpointId,
    transport_stats: Arc<TransportStats>,
) -> Result<()> {
    record_stream_transport(&transport_stats, "catalog", "inbound", "open", &stream);
    let request: CatalogRequest = serde_json::from_slice(&read_line(&mut stream.recv).await?)?;
    tracing::info!(
        peer = %stream.peer_id,
        artist = %request.artist,
        want = request.want.as_deref().unwrap_or("catalog"),
        "federation peer requested a catalog"
    );

    match request.want.as_deref() {
        None | Some("catalog") => {
            let response = match build_catalog(&pool, &own, &request.artist).await {
                Ok(response) => response,
                Err(err) => CatalogResponse {
                    ok: false,
                    error: Some(format!("catalog lookup failed: {err:#}")),
                    ..CatalogResponse::default()
                },
            };
            stream
                .send
                .write_all(&serde_json::to_vec(&response)?)
                .await?;
        }
        Some("artists") => {
            let cursor = request.cursor.clone();
            let limit = request.limit.unwrap_or(64).clamp(1, 200);
            let response = match build_artist_slice(&pool, cursor, limit).await {
                Ok(response) => response,
                Err(err) => CatalogResponse {
                    ok: false,
                    error: Some(format!("artist slice lookup failed: {err:#}")),
                    ..CatalogResponse::default()
                },
            };
            stream
                .send
                .write_all(&serde_json::to_vec(&response)?)
                .await?;
        }
        Some(want @ ("artist_image" | "release_cover")) => {
            let media = if want == "release_cover" {
                release_cover_by_names(
                    &pool,
                    &request.artist,
                    request.release.as_deref().unwrap_or_default(),
                )
                .await?
            } else {
                artist_image_by_name(&pool, &request.artist).await?
            };
            let image = read_image(&storage_dir, media).await;
            let header = match &image {
                Some((bytes, mime)) => ImageHeader {
                    ok: true,
                    error: None,
                    mime_type: mime.clone(),
                    size: bytes.len() as u64,
                },
                None => ImageHeader {
                    ok: false,
                    error: Some("no image".to_string()),
                    ..ImageHeader::default()
                },
            };
            write_line(&mut stream.send, &header).await?;
            if let Some((bytes, _)) = &image {
                stream.send.write_all(bytes).await?;
            }
        }
        Some(other) => {
            let response = CatalogResponse {
                ok: false,
                error: Some(format!("unknown request kind '{other}'")),
                ..CatalogResponse::default()
            };
            stream
                .send
                .write_all(&serde_json::to_vec(&response)?)
                .await?;
        }
    }
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    record_stream_transport(&transport_stats, "catalog", "inbound", "done", &stream);
    Ok(())
}

async fn build_catalog(pool: &PgPool, own: &EndpointId, artist: &str) -> Result<CatalogResponse> {
    let Some(artist_row) = sqlx::query(
        "SELECT id, name FROM furumusic__artist
         WHERE LOWER(name) = LOWER($1) AND is_hidden = false
         LIMIT 1",
    )
    .bind(artist)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(CatalogResponse {
            ok: false,
            error: Some("artist not found in the library".to_string()),
            ..CatalogResponse::default()
        });
    };
    let artist_id: i64 = artist_row.get(0);

    let release_rows = sqlx::query(
        "SELECT r.id, r.title, r.release_type, r.year
         FROM furumusic__release r
         JOIN furumusic__release_artist ra ON ra.release_id = r.id
         WHERE ra.artist_id = $1 AND r.is_hidden = false
         ORDER BY r.year NULLS LAST, r.title",
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await?;
    let mut releases = Vec::new();
    for release_row in release_rows {
        let release_id: i64 = release_row.get(0);
        let track_rows = sqlx::query(
            "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                    c.content_id
             FROM furumusic__track t
             JOIN furumusic__media_file m ON m.id = t.audio_file_id
             LEFT JOIN furumusic__federation_content_id_cache c
                    ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
             WHERE t.release_id = $1 AND t.is_hidden = false
             ORDER BY t.disc_number NULLS FIRST, t.track_number NULLS LAST, t.title",
        )
        .bind(release_id)
        .fetch_all(pool)
        .await?;
        let mut tracks = Vec::with_capacity(track_rows.len());
        for row in track_rows {
            let track_id: i64 = row.get(0);
            let duration: f64 = row.get(4);
            let (artists, featured_artists) = track_catalog_artist_names(pool, track_id).await?;
            tracks.push(CatalogTrack {
                title: row.get(1),
                artists,
                featured_artists,
                track_number: row.get(2),
                disc_number: row.get(3),
                duration_seconds: (duration > 0.0).then_some(duration),
                content_id: row.get(5),
                item_id: item_id_of(own, track_id),
            });
        }
        releases.push(CatalogRelease {
            title: release_row.get(1),
            release_type: release_row.get(2),
            year: release_row.get(3),
            tracks,
        });
    }

    Ok(CatalogResponse {
        ok: true,
        artist: Some(CatalogArtist {
            name: artist_row.get(1),
            releases,
            appears_on: Vec::new(),
        }),
        ..CatalogResponse::default()
    })
}

async fn build_artist_slice(
    pool: &PgPool,
    cursor: Option<String>,
    limit: usize,
) -> Result<CatalogResponse> {
    let offset = cursor
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let rows = sqlx::query(
        r#"SELECT a.name::text AS name,
                  mf.file_path::text AS image_path,
                  COALESCE(s.release_count, 0)::bigint AS release_count,
                  COALESCE(s.track_count, 0)::bigint AS track_count
           FROM furumusic__artist a
           LEFT JOIN furumusic__media_file mf ON mf.id = a.image_file_id
           LEFT JOIN (
               SELECT appearance.artist_id,
                      COUNT(DISTINCT appearance.release_id) FILTER (WHERE appearance.is_primary_release_artist) AS release_count,
                      COUNT(DISTINCT appearance.track_id) AS track_count
                 FROM (
                        SELECT ta.artist_id,
                               t.id AS track_id,
                               r.id AS release_id,
                               primary_release.artist_id IS NOT NULL AS is_primary_release_artist
                          FROM furumusic__track_artist ta
                          JOIN furumusic__track t ON t.id = ta.track_id AND t.is_hidden = false
                          JOIN furumusic__release r ON r.id = t.release_id AND r.is_hidden = false
                          LEFT JOIN furumusic__release_artist primary_release
                            ON primary_release.release_id = r.id
                           AND primary_release.artist_id = ta.artist_id
                           AND primary_release.position = 0
                      ) appearance
                GROUP BY appearance.artist_id
           ) s ON s.artist_id = a.id
           WHERE a.is_hidden = false
             AND COALESCE(s.track_count, 0) > 0
           ORDER BY (COALESCE(s.release_count, 0) > 0) DESC,
                    COALESCE(s.release_count, 0) DESC,
                    COALESCE(s.track_count, 0) DESC,
                    a.name_sort
           LIMIT $1 OFFSET $2"#,
    )
    .bind(limit as i64 + 1)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let mut artists = Vec::with_capacity(rows.len().min(limit));
    let has_more = rows.len() > limit;
    for row in rows.into_iter().take(limit) {
        let name: String = row.get(0);
        artists.push(CatalogArtistPreview {
            artist_key: normalize_name(&name),
            name,
            image_path: row.get(1),
            release_count: row.get(2),
            track_count: row.get(3),
        });
    }
    let next_cursor = has_more.then(|| (offset + artists.len() as i64).to_string());
    Ok(CatalogResponse {
        ok: true,
        artists,
        next_cursor,
        ..CatalogResponse::default()
    })
}

async fn artist_image_by_name(pool: &PgPool, artist: &str) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.file_path, m.mime_type FROM furumusic__artist a
         JOIN furumusic__media_file m ON m.id = a.image_file_id
         WHERE LOWER(a.name) = LOWER($1) AND a.is_hidden = false
         LIMIT 1",
    )
    .bind(artist)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

async fn release_cover_by_names(
    pool: &PgPool,
    artist: &str,
    release: &str,
) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.file_path, m.mime_type FROM furumusic__release r
         JOIN furumusic__release_artist ra ON ra.release_id = r.id
         JOIN furumusic__artist a ON a.id = ra.artist_id
         JOIN furumusic__media_file m ON m.id = r.cover_file_id
         WHERE LOWER(a.name) = LOWER($1) AND LOWER(r.title) = LOWER($2)
         LIMIT 1",
    )
    .bind(artist)
    .bind(release)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}
