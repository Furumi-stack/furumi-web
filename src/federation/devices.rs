//! Personal Fed Clients for the web player.
//!
//! The web service has one iroh endpoint, but each user owns one virtual
//! fed-device inside that endpoint. This module speaks the same JSON-lines
//! protocol as the TUI clients on `furumi/sync/1` and maps operations into
//! user-scoped Postgres state.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use music_dht::{ByteStream, MusicDhtService, NetworkId, PeerTicket, SecretKey, StreamAcceptor};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::player::PlayerDeviceHub;

use super::{TransportStats, record_stream_transport};

pub const SYNC_ALPN: &[u8] = b"furumi/sync/1";

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u16 = 1;
const INVITE_TTL_MS: i64 = 10 * 60 * 1000;
const PAIRING_WAIT_MS: i64 = 5 * 60 * 1000;
const PAIRING_RETRY_DELAY: Duration = Duration::from_secs(1);
const RESPONSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DEVICE_SYNC_INTERVAL: Duration = Duration::from_secs(2);
const LOCAL_SEED_RECHECK_MS: i64 = 60 * 1000;
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_OPS_PER_BATCH: i64 = 1000;

#[derive(Debug, Clone, Serialize)]
pub struct FedDeviceStatus {
    pub this_device_id: String,
    pub this_device_name: String,
    pub group_id: String,
    pub devices: Vec<FedDeviceRow>,
    pub pending: Vec<FedPairingRow>,
    pub active_devices: i64,
    pub revoked_devices: i64,
    pub ops_total: i64,
    pub outbox_ops: i64,
    pub snapshot_likes: i64,
    pub snapshot_playlists: i64,
    pub snapshot_items: i64,
    pub unresolved_items: i64,
    pub last_sync: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FedDeviceRow {
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub endpoint_id: String,
    pub last_seen_ms: Option<i64>,
    pub revoked: bool,
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FedPairingRow {
    pub request_id: String,
    pub device_id: String,
    pub name: String,
    pub client_version: String,
    pub requester_group_id: Option<String>,
    pub requester_group_active_devices: i64,
}

#[derive(Debug, Clone)]
struct Identity {
    device_id: String,
    group_id: String,
    name: String,
}

#[derive(Debug, Clone)]
struct StoredDevice {
    device_id: String,
    endpoint_ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InviteWire {
    v: u16,
    #[serde(rename = "t")]
    ticket: String,
    #[serde(rename = "d")]
    device_id: String,
    #[serde(rename = "i")]
    invite_id: String,
    #[serde(rename = "s")]
    secret: String,
    #[serde(rename = "e")]
    expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceProfileWire {
    device_id: String,
    name: String,
    client_version: String,
    protocol_version: u16,
    endpoint_id: String,
    endpoint_ticket: String,
    #[serde(default)]
    revoked: bool,
    #[serde(default)]
    revoke_cutoff_seq: Option<i64>,
    #[serde(default)]
    updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncedFedTrack {
    item_id: String,
    owner: String,
    title: String,
    #[serde(default)]
    artist_names: Vec<String>,
    #[serde(default)]
    featured_artist_names: Vec<String>,
    year: Option<i32>,
    duration_seconds: Option<i64>,
    content_id: String,
    release_title: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlaybackTrack {
    id: i64,
    title: String,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    duration_seconds: f64,
    #[serde(default)]
    artist_names: Vec<String>,
    #[serde(default)]
    featured_artist_names: Vec<String>,
    release_id: i64,
    release_title: String,
    release_year: Option<i32>,
    #[serde(default)]
    file_path: String,
    content_id: Option<String>,
    audio_format: Option<String>,
    audio_bitrate: Option<i32>,
    audio_sample_rate: Option<i32>,
    audio_bit_depth: Option<i32>,
    file_size_bytes: Option<i64>,
    #[serde(default)]
    play_count: i64,
    #[serde(default)]
    fed: Option<SyncedFedTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PlaybackRepeat {
    #[default]
    Off,
    One,
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlaybackStateWire {
    #[serde(default)]
    queue: Vec<PlaybackTrack>,
    #[serde(default)]
    queue_pos: usize,
    playing: bool,
    paused: bool,
    #[serde(default)]
    idle_since_ms: Option<i64>,
    position_secs: f64,
    #[serde(default)]
    volume: u8,
    shuffle: bool,
    repeat: PlaybackRepeat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlaybackSnapshot {
    device_id: String,
    device_name: String,
    active: bool,
    updated_at_ms: i64,
    state: PlaybackStateWire,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlaybackCommand {
    SetState {
        state: PlaybackStateWire,
        #[serde(default)]
        seek: bool,
    },
    ActiveChanged {
        active_device_id: String,
        active_device_name: String,
        state: PlaybackStateWire,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncOpWire {
    op_id: String,
    origin_device_id: String,
    seq: i64,
    hlc_ms: i64,
    payload: SyncOpPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SyncOpPayload {
    TrackLikeSet {
        content_id: String,
        liked: bool,
        #[serde(default)]
        fed: Option<SyncedFedTrack>,
    },
    PlaylistCreated {
        playlist_id: String,
        title: String,
    },
    PlaylistRenamed {
        playlist_id: String,
        title: String,
    },
    PlaylistDeleted {
        playlist_id: String,
    },
    PlaylistTrackAdded {
        playlist_id: String,
        content_id: String,
        position: i64,
        #[serde(default)]
        fed: Option<SyncedFedTrack>,
    },
    PlaylistTrackRemoved {
        playlist_id: String,
        content_id: String,
    },
    DeviceProfileSet {
        name: String,
        client_version: String,
        endpoint_ticket: String,
        endpoint_id: String,
    },
    DeviceTrusted {
        target_device_id: String,
    },
    DeviceRevoked {
        target_device_id: String,
        target_max_seq_seen: i64,
    },
    PlaybackCommand {
        target_device_id: String,
        command: PlaybackCommand,
    },
}

impl SyncOpPayload {
    fn is_tombstone(&self) -> bool {
        matches!(
            self,
            SyncOpPayload::TrackLikeSet { liked: false, .. }
                | SyncOpPayload::PlaylistDeleted { .. }
                | SyncOpPayload::PlaylistTrackRemoved { .. }
                | SyncOpPayload::DeviceRevoked { .. }
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncSnapshot {
    #[serde(default)]
    likes: Vec<SnapshotLike>,
    #[serde(default)]
    unlikes: Vec<SnapshotLikeTombstone>,
    #[serde(default)]
    playlists: Vec<SnapshotPlaylist>,
    #[serde(default)]
    deleted_playlists: Vec<SnapshotPlaylistTombstone>,
    #[serde(default)]
    removed_playlist_items: Vec<SnapshotPlaylistItemTombstone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotLike {
    content_id: String,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    fed: Option<SyncedFedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotLikeTombstone {
    content_id: String,
    hlc_ms: i64,
    op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylist {
    playlist_id: String,
    title: String,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    items: Vec<SnapshotPlaylistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistTombstone {
    playlist_id: String,
    hlc_ms: i64,
    op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistItem {
    content_id: String,
    position: i64,
    hlc_ms: i64,
    op_id: String,
    #[serde(default)]
    fed: Option<SyncedFedTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPlaylistItemTombstone {
    playlist_id: String,
    content_id: String,
    hlc_ms: i64,
    op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireMessage {
    PairRequest {
        invite_id: String,
        secret: String,
        profile: DeviceProfileWire,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        group_active_devices: usize,
        #[serde(default)]
        devices: Vec<DeviceProfileWire>,
        vector: BTreeMap<String, i64>,
        ops: Vec<SyncOpWire>,
        snapshot: SyncSnapshot,
        #[serde(default)]
        playback: Option<PlaybackSnapshot>,
    },
    PairResponse {
        accepted: bool,
        #[serde(default)]
        pending: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        profile: Option<DeviceProfileWire>,
        #[serde(default)]
        devices: Vec<DeviceProfileWire>,
        #[serde(default)]
        vector: BTreeMap<String, i64>,
        #[serde(default)]
        ops: Vec<SyncOpWire>,
        #[serde(default)]
        snapshot: SyncSnapshot,
        #[serde(default)]
        playback: Option<PlaybackSnapshot>,
    },
    Hello {
        group_id: String,
        profile: DeviceProfileWire,
        devices: Vec<DeviceProfileWire>,
        vector: BTreeMap<String, i64>,
        ops: Vec<SyncOpWire>,
        snapshot: SyncSnapshot,
        #[serde(default)]
        playback: Option<PlaybackSnapshot>,
    },
    SyncResponse {
        accepted: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        devices: Vec<DeviceProfileWire>,
        #[serde(default)]
        vector: BTreeMap<String, i64>,
        #[serde(default)]
        ops: Vec<SyncOpWire>,
        #[serde(default)]
        snapshot: SyncSnapshot,
        #[serde(default)]
        playback: Option<PlaybackSnapshot>,
    },
}

enum PairAttempt {
    Accepted(String),
    Pending,
    Denied(String),
}

pub async fn create_invite(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    user_id: i64,
    user_name: &str,
) -> Result<String> {
    let identity = ensure_identity(pool, user_id, user_name).await?;
    let ticket = service.ticket().await?.to_string();
    let secret = random_hex(16);
    let invite_id = random_hex(8);
    let expires_at_ms = now_ms() + INVITE_TTL_MS;
    sqlx::query(
        "INSERT INTO furumusic__fed_device_invite
            (invite_id, user_id, secret_hash, expires_at_ms, created_at_ms)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&invite_id)
    .bind(user_id)
    .bind(hash_secret(&secret))
    .bind(expires_at_ms)
    .bind(now_ms())
    .execute(pool)
    .await?;
    let payload = InviteWire {
        v: 1,
        ticket,
        device_id: identity.device_id,
        invite_id,
        secret,
        expires_at_ms,
    };
    encode_invite(&payload)
}

pub async fn connect_invite(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    user_id: i64,
    user_name: &str,
    invite_link: &str,
) -> Result<String> {
    let invite = parse_invite(invite_link)?;
    anyhow::ensure!(invite.expires_at_ms >= now_ms(), "invite expired");
    let ticket: PeerTicket = invite.ticket.parse().context("malformed invite ticket")?;
    let deadline = (now_ms() + PAIRING_WAIT_MS).min(invite.expires_at_ms);
    let mut last_error: Option<String>;
    loop {
        match try_connect_invite(
            pool,
            Arc::clone(&service),
            Arc::clone(&hub),
            Arc::clone(&transport_stats),
            user_id,
            user_name,
            &invite,
            ticket.clone(),
        )
        .await
        {
            Ok(PairAttempt::Accepted(message)) => return Ok(message),
            Ok(PairAttempt::Pending) => last_error = None,
            Ok(PairAttempt::Denied(message)) => anyhow::bail!(message),
            Err(err) => {
                tracing::debug!("web fed pairing poll failed: {err:#}");
                last_error = Some(format!("{err:#}"));
            }
        }
        if now_ms() >= deadline {
            if let Some(error) = last_error {
                anyhow::bail!("pairing timed out; last error: {error}");
            }
            anyhow::bail!("pairing timed out");
        }
        tokio::time::sleep(PAIRING_RETRY_DELAY).await;
    }
}

pub async fn answer_pairing(
    pool: &sqlx::PgPool,
    user_id: i64,
    request_id: &str,
    accept: bool,
    use_requester_group: bool,
) -> Result<()> {
    let pending = sqlx::query(
        "SELECT device_id, name, client_version, endpoint_id, endpoint_ticket,
                created_at_ms, requester_group_id, requester_group_devices_json
         FROM furumusic__fed_pending_pairing
         WHERE user_id = $1 AND request_id = $2",
    )
    .bind(user_id)
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    let changed = sqlx::query(
        "UPDATE furumusic__fed_pending_pairing
         SET status = $3, answered_at_ms = $4, use_requester_group = $5
         WHERE user_id = $1 AND request_id = $2 AND status = 'pending'",
    )
    .bind(user_id)
    .bind(request_id)
    .bind(if accept { "accepted" } else { "denied" })
    .bind(now_ms())
    .bind(use_requester_group)
    .execute(pool)
    .await?
    .rows_affected();
    if !accept || changed == 0 {
        return Ok(());
    }
    if let Some(row) = pending {
        let device_id: String = row.get("device_id");
        enforce_single_user_binding(pool, user_id, &device_id).await?;
        let profile = DeviceProfileWire {
            device_id,
            name: row.get("name"),
            client_version: row.get("client_version"),
            protocol_version: PROTOCOL_VERSION,
            endpoint_id: row.get("endpoint_id"),
            endpoint_ticket: row.get("endpoint_ticket"),
            revoked: false,
            revoke_cutoff_seq: None,
            updated_at_ms: row.get("created_at_ms"),
        };
        if use_requester_group {
            let group_id: Option<String> = row.get("requester_group_id");
            if let Some(group_id) = group_id.filter(|value| !value.trim().is_empty()) {
                sqlx::query(
                    "UPDATE furumusic__fed_device_identity SET group_id = $2 WHERE user_id = $1",
                )
                .bind(user_id)
                .bind(group_id)
                .execute(pool)
                .await?;
            }
            let devices_json: String = row.get("requester_group_devices_json");
            let devices: Vec<DeviceProfileWire> = serde_json::from_str(&devices_json)?;
            apply_device_profiles(pool, user_id, &devices).await?;
        }
        apply_device_profile(pool, user_id, &profile, true).await?;
        record_local_op(
            pool,
            user_id,
            SyncOpPayload::DeviceTrusted {
                target_device_id: profile.device_id,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn revoke_device(pool: &sqlx::PgPool, user_id: i64, device_id: &str) -> Result<()> {
    let identity = ensure_identity(pool, user_id, "").await?;
    anyhow::ensure!(device_id != identity.device_id, "cannot revoke this device");
    let cutoff: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0)
         FROM furumusic__fed_sync_ops
         WHERE user_id = $1 AND origin_device_id = $2",
    )
    .bind(user_id)
    .bind(device_id)
    .fetch_one(pool)
    .await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::DeviceRevoked {
            target_device_id: device_id.to_string(),
            target_max_seq_seen: cutoff,
        },
    )
    .await?;
    Ok(())
}

pub async fn status(pool: &sqlx::PgPool, user_id: i64, user_name: &str) -> Result<FedDeviceStatus> {
    let identity = ensure_identity(pool, user_id, user_name).await?;
    maybe_seed_local_user_state(pool, user_id).await?;
    let active_devices: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_device
         WHERE user_id = $1 AND trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let revoked_devices: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_device
         WHERE user_id = $1 AND revoked_at_ms IS NOT NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let ops_total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__fed_sync_ops WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    let outbox_ops: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM furumusic__fed_sync_ops o
         WHERE o.user_id = $1
           AND EXISTS (
              SELECT 1 FROM furumusic__fed_device d
              LEFT JOIN furumusic__fed_peer_ack a
                ON a.user_id = d.user_id
               AND a.peer_device_id = d.device_id
               AND a.origin_device_id = o.origin_device_id
              WHERE d.user_id = o.user_id
                AND d.trusted_at_ms IS NOT NULL
                AND d.revoked_at_ms IS NULL
                AND d.device_id <> $2
                AND o.seq > COALESCE(a.max_seq, 0)
           )",
    )
    .bind(user_id)
    .bind(&identity.device_id)
    .fetch_one(pool)
    .await?;
    let snapshot_likes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_state_like WHERE user_id = $1 AND liked = true",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let snapshot_playlists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_state_playlist WHERE user_id = $1 AND deleted = false",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let snapshot_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_state_playlist_item WHERE user_id = $1 AND present = true",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let unresolved_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_state_playlist_item i
         WHERE i.user_id = $1 AND i.present = true AND i.local_track_id IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let last_sync: Option<String> = sqlx::query_scalar(
        "SELECT last_sync FROM furumusic__fed_device_identity WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let last_error: Option<String> = sqlx::query_scalar(
        "SELECT last_error FROM furumusic__fed_device_identity WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query(
        "SELECT device_id, name, client_version, endpoint_id, last_seen_ms,
                revoked_at_ms IS NOT NULL AS revoked
         FROM furumusic__fed_device
         WHERE user_id = $1 AND trusted_at_ms IS NOT NULL
         ORDER BY revoked_at_ms IS NOT NULL, name, device_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let devices = rows
        .into_iter()
        .filter_map(|row| {
            let device_id: String = row.get("device_id");
            let revoked: bool = row.get("revoked");
            (!revoked || device_id == identity.device_id).then(|| FedDeviceRow {
                is_self: device_id == identity.device_id,
                device_id,
                name: row.get("name"),
                client_version: row.get("client_version"),
                endpoint_id: row.get("endpoint_id"),
                last_seen_ms: row.get("last_seen_ms"),
                revoked,
            })
        })
        .collect();
    let pending = sqlx::query(
        "SELECT request_id, device_id, name, client_version,
                requester_group_id, requester_group_active_devices
         FROM furumusic__fed_pending_pairing
         WHERE user_id = $1 AND status = 'pending'
         ORDER BY created_at_ms DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| FedPairingRow {
        request_id: row.get("request_id"),
        device_id: row.get("device_id"),
        name: row.get("name"),
        client_version: row.get("client_version"),
        requester_group_id: row.get("requester_group_id"),
        requester_group_active_devices: row.get("requester_group_active_devices"),
    })
    .collect();

    Ok(FedDeviceStatus {
        this_device_id: identity.device_id,
        this_device_name: identity.name,
        group_id: identity.group_id,
        devices,
        pending,
        active_devices,
        revoked_devices,
        ops_total,
        outbox_ops,
        snapshot_likes,
        snapshot_playlists,
        snapshot_items,
        unresolved_items,
        last_sync,
        last_error,
    })
}

pub async fn record_track_like(
    pool: &sqlx::PgPool,
    user_id: i64,
    track_id: i64,
    liked: bool,
) -> Result<()> {
    if let Some(content_id) = track_content_id(pool, track_id).await? {
        let fed = if liked {
            synced_fed_track_for_track(pool, track_id, &content_id).await?
        } else {
            None
        };
        record_local_op(
            pool,
            user_id,
            SyncOpPayload::TrackLikeSet {
                content_id,
                liked,
                fed,
            },
        )
        .await?;
    }
    Ok(())
}

/// Records a like for a content-addressed track that need not be local yet.
/// The ordinary local likes table remains a projection for materialized
/// tracks; the durable user intent lives under `content_id`.
pub async fn record_content_like(
    pool: &sqlx::PgPool,
    user_id: i64,
    content_id: &str,
    liked: bool,
    fed: Option<serde_json::Value>,
) -> Result<()> {
    let content_id =
        music_dht::normalize_content_id(content_id).context("invalid track content id")?;
    let fed = fed
        .map(serde_json::from_value::<SyncedFedTrack>)
        .transpose()
        .context("invalid federated track metadata")?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::TrackLikeSet {
            content_id,
            liked,
            fed: liked.then_some(fed).flatten(),
        },
    )
    .await
}

/// Adds a content-addressed track to a user's playlist without requiring a
/// local numeric track row. Materialization later fills `local_track_id`
/// without replacing this playlist item.
pub async fn record_content_playlist_add(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    content_id: &str,
    position: i64,
    fed: Option<serde_json::Value>,
) -> Result<()> {
    let content_id =
        music_dht::normalize_content_id(content_id).context("invalid track content id")?;
    let fed = fed
        .map(serde_json::from_value::<SyncedFedTrack>)
        .transpose()
        .context("invalid federated track metadata")?;
    let title = playlist_title(pool, playlist_id)
        .await?
        .context("playlist not found")?;
    let sync_id = ensure_local_playlist_sync_id(pool, user_id, playlist_id, &title).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaylistTrackAdded {
            playlist_id: sync_id,
            content_id,
            position,
            fed,
        },
    )
    .await
}

/// Reconciles content-addressed user state after a track becomes local.
/// History is intentionally not touched: only actual web playback writes it.
pub async fn materialize_content_state(
    pool: &sqlx::PgPool,
    content_id: &str,
    track_id: i64,
) -> Result<()> {
    let content_id =
        music_dht::normalize_content_id(content_id).context("invalid track content id")?;
    let likes = sqlx::query(
        "SELECT user_id, liked, hlc_ms
           FROM furumusic__fed_state_like WHERE content_id = $1",
    )
    .bind(&content_id)
    .fetch_all(pool)
    .await?;
    for row in likes {
        let user_id: i64 = row.get("user_id");
        let liked: bool = row.get("liked");
        if liked {
            sqlx::query(
                "INSERT INTO furumusic__user_liked_track
                    (user_id, track_id, created_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (user_id, track_id) DO NOTHING",
            )
            .bind(user_id)
            .bind(track_id)
            .bind(iso_from_ms(row.get("hlc_ms")))
            .execute(pool)
            .await?;
        }
    }
    let playlist_items = sqlx::query(
        "SELECT i.user_id, i.playlist_id, i.position
           FROM furumusic__fed_state_playlist_item i
          WHERE i.content_id = $1 AND i.present = true",
    )
    .bind(&content_id)
    .fetch_all(pool)
    .await?;
    for row in playlist_items {
        let user_id: i64 = row.get("user_id");
        let sync_id: String = row.get("playlist_id");
        let playlist_id = ensure_playlist_for_item(pool, user_id, &sync_id).await?;
        sqlx::query(
            "INSERT INTO furumusic__playlist_track
                (playlist_id, track_id, position, added_at, added_by_user_id)
             SELECT $1, $2, $3, $4, $5
              WHERE NOT EXISTS (
                SELECT 1 FROM furumusic__playlist_track
                 WHERE playlist_id = $1 AND track_id = $2
              )",
        )
        .bind(playlist_id)
        .bind(track_id)
        .bind(row.get::<i64, _>("position") as i32)
        .bind(now_iso())
        .bind(user_id)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "UPDATE furumusic__fed_state_like
            SET local_track_id = $2 WHERE content_id = $1",
    )
    .bind(&content_id)
    .bind(track_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE furumusic__fed_state_playlist_item
            SET local_track_id = $2 WHERE content_id = $1",
    )
    .bind(&content_id)
    .bind(track_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_playlist_created(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
) -> Result<()> {
    let sync_id = ensure_local_playlist_sync_id(pool, user_id, playlist_id, title).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaylistCreated {
            playlist_id: sync_id,
            title: title.to_string(),
        },
    )
    .await?;
    Ok(())
}

pub async fn record_playlist_renamed(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
) -> Result<()> {
    let sync_id = ensure_local_playlist_sync_id(pool, user_id, playlist_id, title).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaylistRenamed {
            playlist_id: sync_id,
            title: title.to_string(),
        },
    )
    .await?;
    Ok(())
}

pub async fn record_playlist_deleted(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
) -> Result<()> {
    if let Some(sync_id) = local_playlist_sync_id(pool, user_id, playlist_id).await? {
        record_local_op(
            pool,
            user_id,
            SyncOpPayload::PlaylistDeleted {
                playlist_id: sync_id,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn record_playlist_tracks_added(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    track_ids: &[i64],
) -> Result<()> {
    if track_ids.is_empty() {
        return Ok(());
    }
    let title = playlist_title(pool, playlist_id).await?.unwrap_or_default();
    let sync_id = ensure_local_playlist_sync_id(pool, user_id, playlist_id, &title).await?;
    let rows = playlist_track_content_positions(pool, playlist_id, track_ids).await?;
    for (track_id, content_id, position) in rows {
        let fed = synced_fed_track_for_track(pool, track_id, &content_id).await?;
        record_local_op(
            pool,
            user_id,
            SyncOpPayload::PlaylistTrackAdded {
                playlist_id: sync_id.clone(),
                content_id,
                position,
                fed,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn record_playlist_tracks_removed(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    content_ids: &[String],
) -> Result<()> {
    if content_ids.is_empty() {
        return Ok(());
    }
    if let Some(sync_id) = local_playlist_sync_id(pool, user_id, playlist_id).await? {
        for raw in content_ids {
            if let Some(content_id) = music_dht::normalize_content_id(raw) {
                record_local_op(
                    pool,
                    user_id,
                    SyncOpPayload::PlaylistTrackRemoved {
                        playlist_id: sync_id.clone(),
                        content_id,
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub async fn serve_peers(
    mut acceptor: StreamAcceptor,
    pool: sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let pool = pool.clone();
        let service = Arc::clone(&service);
        let hub = Arc::clone(&hub);
        let transport_stats = Arc::clone(&transport_stats);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(err) = serve_one(stream, &pool, service, hub, transport_stats).await {
                tracing::warn!(peer = %peer, "web fed device sync stream failed: {err:#}");
            }
        });
    }
}

pub async fn sync_loop(
    pool: sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
) {
    let mut interval = tokio::time::interval(DEVICE_SYNC_INTERVAL);
    loop {
        interval.tick().await;
        if let Err(err) = sync_once_all(
            &pool,
            Arc::clone(&service),
            Arc::clone(&hub),
            Arc::clone(&transport_stats),
        )
        .await
        {
            tracing::debug!("web fed device sync tick failed: {err:#}");
        }
    }
}

pub async fn sync_once_all(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT DISTINCT user_id FROM furumusic__fed_device
         WHERE trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let user_id: i64 = row.get("user_id");
        if let Err(err) = sync_once(
            pool,
            Arc::clone(&service),
            Arc::clone(&hub),
            Arc::clone(&transport_stats),
            user_id,
        )
        .await
        {
            set_last_error(pool, user_id, Some(&format!("{err:#}"))).await?;
        }
    }
    Ok(())
}

pub async fn sync_once(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    user_id: i64,
) -> Result<()> {
    let devices = active_remote_devices(pool, user_id).await?;
    for device in devices {
        if device.endpoint_ticket.trim().is_empty() {
            continue;
        }
        if let Err(err) = sync_device(
            pool,
            Arc::clone(&service),
            Arc::clone(&hub),
            Arc::clone(&transport_stats),
            user_id,
            &device,
        )
        .await
        {
            tracing::debug!(device = %device.device_id, "web fed device sync failed: {err:#}");
            set_last_error(
                pool,
                user_id,
                Some(&format!("{}: {err:#}", short_id(&device.device_id))),
            )
            .await?;
        }
    }
    Ok(())
}

async fn try_connect_invite(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    user_id: i64,
    user_name: &str,
    invite: &InviteWire,
    ticket: PeerTicket,
) -> Result<PairAttempt> {
    let peer = service.connect(ticket).await?;
    let own_ticket = service.ticket().await?.to_string();
    let identity = ensure_identity(pool, user_id, user_name).await?;
    let group_active_devices = active_device_count(pool, user_id).await?.max(1) as usize;
    let profile = own_profile(pool, user_id, user_name, &own_ticket).await?;
    let devices = device_profiles(pool, user_id).await?;
    let vector = vector(pool, user_id).await?;
    let ops = ops_for_peer(pool, user_id, &invite.device_id).await?;
    let snapshot = snapshot(pool, user_id).await?;
    let playback = local_playback_snapshot(pool, Arc::clone(&hub), user_id, &identity).await;
    let mut stream = service.open_stream(peer, SYNC_ALPN).await?;
    record_stream_transport(&transport_stats, "device-sync", "outbound", "open", &stream);
    write_msg(
        &mut stream,
        &WireMessage::PairRequest {
            invite_id: invite.invite_id.clone(),
            secret: invite.secret.clone(),
            profile,
            group_id: Some(identity.group_id),
            group_active_devices,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_send(&mut stream).await?;
    let response = read_msg(&mut stream)
        .await
        .context("pairing response was not received")?;
    record_stream_transport(&transport_stats, "device-sync", "outbound", "done", &stream);
    match response {
        WireMessage::PairResponse {
            accepted: true,
            group_id: Some(group_id),
            profile,
            devices,
            vector,
            ops,
            snapshot,
            playback,
            ..
        } => {
            set_group_id(pool, user_id, &group_id).await?;
            if let Some(profile) = profile {
                enforce_single_user_binding(pool, user_id, &profile.device_id).await?;
                apply_device_profile(pool, user_id, &profile, true).await?;
                if let Some(playback) = playback {
                    apply_playback_snapshot(Arc::clone(&hub), pool, user_id, playback).await?;
                }
            }
            apply_device_profiles(pool, user_id, &devices).await?;
            apply_snapshot(pool, user_id, snapshot).await?;
            apply_ops(pool, Arc::clone(&hub), user_id, ops).await?;
            record_local_op(
                pool,
                user_id,
                SyncOpPayload::DeviceTrusted {
                    target_device_id: invite.device_id.clone(),
                },
            )
            .await?;
            note_peer_vector(pool, user_id, &invite.device_id, &vector).await?;
            set_last_sync(
                pool,
                user_id,
                Some(&format!("paired {}", short_id(&invite.device_id))),
            )
            .await?;
            Ok(PairAttempt::Accepted(format!(
                "connected device {}",
                short_id(&invite.device_id)
            )))
        }
        WireMessage::PairResponse {
            accepted: false,
            pending: true,
            ..
        } => Ok(PairAttempt::Pending),
        WireMessage::PairResponse {
            accepted: false,
            error,
            ..
        } => Ok(PairAttempt::Denied(
            error.unwrap_or_else(|| "pairing denied".to_string()),
        )),
        _ => anyhow::bail!("unexpected pairing response"),
    }
}

async fn serve_one(
    mut stream: ByteStream,
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
) -> Result<()> {
    record_stream_transport(&transport_stats, "device-sync", "inbound", "open", &stream);
    match read_msg(&mut stream).await? {
        WireMessage::PairRequest {
            invite_id,
            secret,
            profile,
            group_id,
            group_active_devices,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            handle_pair_request(
                stream,
                pool,
                service,
                hub,
                transport_stats,
                invite_id,
                secret,
                profile,
                group_id,
                group_active_devices,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            )
            .await
        }
        WireMessage::Hello {
            group_id,
            profile,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        } => {
            handle_hello(
                stream,
                pool,
                service,
                hub,
                transport_stats,
                group_id,
                profile,
                devices,
                vector,
                ops,
                snapshot,
                playback,
            )
            .await
        }
        _ => anyhow::bail!("unexpected first message"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_pair_request(
    mut stream: ByteStream,
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    invite_id: String,
    secret: String,
    mut profile: DeviceProfileWire,
    requester_group_id: Option<String>,
    requester_group_active_devices: usize,
    requester_group_devices: Vec<DeviceProfileWire>,
    incoming_vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    incoming_snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    profile.endpoint_id = stream.peer_id.to_string();
    let Some((user_id, secret_hash, expires_at_ms, used_at_ms)) =
        invite_row(pool, &invite_id).await?
    else {
        return write_pair_reject(stream, "invalid or expired invite").await;
    };
    let request_id = pair_request_id(&invite_id, &profile.device_id);
    if expires_at_ms < now_ms() || secret_hash != hash_secret(&secret) {
        tracing::warn!(peer = %stream.peer_id, invite_id, "ignored pairing request with invalid invite secret");
        return write_pair_reject(stream, "invalid or expired invite").await;
    }
    if let Some(owner_id) = device_owner(pool, &profile.device_id).await?
        && owner_id != user_id
    {
        return write_pair_reject(stream, "device already paired with another account").await;
    }
    if used_at_ms.is_some() && !pairing_already_accepted(pool, user_id, &request_id).await? {
        return write_pair_reject(stream, "invite already used").await;
    }
    let identity = ensure_identity(pool, user_id, "").await?;
    let requester_group_id = requester_group_id.filter(|id| !id.trim().is_empty());
    let requester_group_active_devices = requester_group_active_devices.max(1);
    let requester_group_conflict = requester_group_id
        .as_deref()
        .is_some_and(|group_id| group_id != identity.group_id)
        && requester_group_active_devices > 1;
    sqlx::query(
        "INSERT INTO furumusic__fed_pending_pairing
            (request_id, user_id, device_id, name, client_version, endpoint_id,
             endpoint_ticket, invite_id, created_at_ms, status,
             requester_group_id, requester_group_active_devices,
             requester_group_devices_json, use_requester_group)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending',
                 $10, $11, $12, false)
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(&request_id)
    .bind(user_id)
    .bind(&profile.device_id)
    .bind(&profile.name)
    .bind(&profile.client_version)
    .bind(&profile.endpoint_id)
    .bind(&profile.endpoint_ticket)
    .bind(&invite_id)
    .bind(now_ms())
    .bind(
        requester_group_conflict
            .then_some(requester_group_id.as_deref())
            .flatten(),
    )
    .bind(if requester_group_conflict {
        requester_group_active_devices as i64
    } else {
        0
    })
    .bind(serde_json::to_string(&requester_group_devices)?)
    .execute(pool)
    .await?;

    let pairing = pairing_status(pool, user_id, &request_id).await?;
    match pairing.as_deref() {
        Some("pending") => {
            write_msg(
                &mut stream,
                &WireMessage::PairResponse {
                    accepted: false,
                    pending: true,
                    error: Some("pairing pending".to_string()),
                    group_id: None,
                    profile: None,
                    devices: Vec::new(),
                    vector: BTreeMap::new(),
                    ops: Vec::new(),
                    snapshot: SyncSnapshot::default(),
                    playback: None,
                },
            )
            .await?;
            finish_response(&mut stream).await?;
            record_stream_transport(&transport_stats, "device-sync", "inbound", "done", &stream);
            return Ok(());
        }
        Some("accepted") => {}
        _ => return write_pair_reject(stream, "pairing denied").await,
    }

    let own_ticket = service.ticket().await?.to_string();
    let own_profile = own_profile(pool, user_id, "", &own_ticket).await?;
    apply_device_profile(pool, user_id, &profile, true).await?;
    if let Some(playback) = playback {
        apply_playback_snapshot(Arc::clone(&hub), pool, user_id, playback).await?;
    }
    apply_snapshot(pool, user_id, incoming_snapshot).await?;
    apply_ops(pool, Arc::clone(&hub), user_id, ops).await?;
    apply_device_profiles(pool, user_id, &requester_group_devices).await?;
    note_peer_vector(pool, user_id, &profile.device_id, &incoming_vector).await?;
    sqlx::query("UPDATE furumusic__fed_device_invite SET used_at_ms = $2 WHERE invite_id = $1")
        .bind(&invite_id)
        .bind(now_ms())
        .execute(pool)
        .await?;
    set_last_sync(
        pool,
        user_id,
        Some(&format!("paired {}", short_id(&profile.device_id))),
    )
    .await?;

    let devices = device_profiles(pool, user_id).await?;
    let vector = vector(pool, user_id).await?;
    let ops = ops_for_peer(pool, user_id, &profile.device_id).await?;
    let snapshot = snapshot(pool, user_id).await?;
    let playback = local_playback_snapshot(pool, hub, user_id, &identity).await;
    write_msg(
        &mut stream,
        &WireMessage::PairResponse {
            accepted: true,
            pending: false,
            error: None,
            group_id: Some(identity.group_id),
            profile: Some(own_profile),
            devices,
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream).await?;
    record_stream_transport(&transport_stats, "device-sync", "inbound", "done", &stream);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_hello(
    mut stream: ByteStream,
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    group_id: String,
    mut profile: DeviceProfileWire,
    devices: Vec<DeviceProfileWire>,
    incoming_vector: BTreeMap<String, i64>,
    ops: Vec<SyncOpWire>,
    incoming_snapshot: SyncSnapshot,
    playback: Option<PlaybackSnapshot>,
) -> Result<()> {
    let Some(user_id) = active_device_user(pool, &profile.device_id).await? else {
        return write_sync_reject(stream, "device is not trusted").await;
    };
    let identity = ensure_identity(pool, user_id, "").await?;
    if group_id != identity.group_id {
        return write_sync_reject(stream, "sync group mismatch").await;
    }
    profile.endpoint_id = stream.peer_id.to_string();
    apply_device_profile(pool, user_id, &profile, false).await?;
    apply_device_profiles(pool, user_id, &devices).await?;
    if let Some(playback) = playback {
        apply_playback_snapshot(Arc::clone(&hub), pool, user_id, playback).await?;
    }
    apply_snapshot(pool, user_id, incoming_snapshot).await?;
    apply_ops(pool, Arc::clone(&hub), user_id, ops).await?;
    note_peer_vector(pool, user_id, &profile.device_id, &incoming_vector).await?;
    mark_seen(
        pool,
        user_id,
        &profile.device_id,
        Some(stream.peer_id.to_string()),
    )
    .await?;
    set_last_sync(
        pool,
        user_id,
        Some(&format!("synced {}", short_id(&profile.device_id))),
    )
    .await?;

    let own_ticket = service.ticket().await?.to_string();
    let own_profile = own_profile(pool, user_id, "", &own_ticket).await?;
    let mut devices = device_profiles(pool, user_id).await?;
    devices.push(own_profile);
    let vector = vector(pool, user_id).await?;
    let ops = ops_for_peer(pool, user_id, &profile.device_id).await?;
    let snapshot = snapshot(pool, user_id).await?;
    let playback = local_playback_snapshot(pool, hub, user_id, &identity).await;
    write_msg(
        &mut stream,
        &WireMessage::SyncResponse {
            accepted: true,
            error: None,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_response(&mut stream).await?;
    record_stream_transport(&transport_stats, "device-sync", "inbound", "done", &stream);
    Ok(())
}

async fn sync_device(
    pool: &sqlx::PgPool,
    service: Arc<MusicDhtService>,
    hub: Arc<PlayerDeviceHub>,
    transport_stats: Arc<TransportStats>,
    user_id: i64,
    device: &StoredDevice,
) -> Result<()> {
    let ticket: PeerTicket = device.endpoint_ticket.parse()?;
    let peer = service.connect(ticket).await?;
    let own_ticket = service.ticket().await?.to_string();
    let identity = ensure_identity(pool, user_id, "").await?;
    let profile = own_profile(pool, user_id, "", &own_ticket).await?;
    let devices = device_profiles(pool, user_id).await?;
    let vector = vector(pool, user_id).await?;
    let ops = ops_for_peer(pool, user_id, &device.device_id).await?;
    let snapshot = snapshot(pool, user_id).await?;
    let playback = local_playback_snapshot(pool, Arc::clone(&hub), user_id, &identity).await;
    let mut stream = service.open_stream(peer, SYNC_ALPN).await?;
    record_stream_transport(&transport_stats, "device-sync", "outbound", "open", &stream);
    write_msg(
        &mut stream,
        &WireMessage::Hello {
            group_id: identity.group_id,
            profile,
            devices,
            vector,
            ops,
            snapshot,
            playback,
        },
    )
    .await?;
    finish_send(&mut stream).await?;
    let response = read_msg(&mut stream)
        .await
        .context("device sync response was not received")?;
    record_stream_transport(&transport_stats, "device-sync", "outbound", "done", &stream);
    match response {
        WireMessage::SyncResponse {
            accepted: true,
            devices,
            vector,
            ops,
            snapshot,
            playback,
            ..
        } => {
            apply_device_profiles(pool, user_id, &devices).await?;
            if let Some(playback) = playback {
                apply_playback_snapshot(Arc::clone(&hub), pool, user_id, playback).await?;
            }
            apply_snapshot(pool, user_id, snapshot).await?;
            apply_ops(pool, hub, user_id, ops).await?;
            note_peer_vector(pool, user_id, &device.device_id, &vector).await?;
            mark_seen(
                pool,
                user_id,
                &device.device_id,
                Some(stream.peer_id.to_string()),
            )
            .await?;
            set_last_sync(
                pool,
                user_id,
                Some(&format!("synced {}", short_id(&device.device_id))),
            )
            .await?;
            set_last_error(pool, user_id, None).await?;
            Ok(())
        }
        WireMessage::SyncResponse {
            accepted: false,
            error,
            ..
        } => anyhow::bail!(error.unwrap_or_else(|| "sync refused".to_string())),
        _ => anyhow::bail!("unexpected sync response"),
    }
}

async fn ensure_identity(pool: &sqlx::PgPool, user_id: i64, user_name: &str) -> Result<Identity> {
    if let Some(row) = sqlx::query(
        "SELECT device_id, group_id, device_name, local_seeded_at_ms
         FROM furumusic__fed_device_identity
         WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    {
        if row.get::<i64, _>("local_seeded_at_ms") == 0 {
            seed_local_user_state(pool, user_id).await?;
        }
        return Ok(Identity {
            device_id: row.get("device_id"),
            group_id: row.get("group_id"),
            name: row.get("device_name"),
        });
    }
    let secret = SecretKey::generate().to_bytes();
    let device_id = format!("web_{}", &blake3::hash(&secret).to_hex()[..24]);
    let group_id = format!("grp_{}", &blake3::hash(device_id.as_bytes()).to_hex()[..24]);
    let display = if user_name.trim().is_empty() {
        "WEB".to_string()
    } else {
        format!("WEB · {}", user_name.trim())
    };
    let now = now_ms();
    sqlx::query(
        "INSERT INTO furumusic__fed_device_identity
            (user_id, device_id, group_id, device_name, local_seq, last_hlc_ms)
         VALUES ($1, $2, $3, $4, 0, 0)",
    )
    .bind(user_id)
    .bind(&device_id)
    .bind(&group_id)
    .bind(&display)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO furumusic__fed_device
            (user_id, device_id, name, client_version, protocol_version, endpoint_id,
             endpoint_ticket, trusted_at_ms, last_seen_ms)
         VALUES ($1, $2, $3, $4, $5, '', '', $6, $6)",
    )
    .bind(user_id)
    .bind(&device_id)
    .bind(&display)
    .bind(CLIENT_VERSION)
    .bind(PROTOCOL_VERSION as i32)
    .bind(now)
    .execute(pool)
    .await?;
    seed_local_user_state(pool, user_id).await?;
    Ok(Identity {
        device_id,
        group_id,
        name: display,
    })
}

async fn maybe_seed_local_user_state(pool: &sqlx::PgPool, user_id: i64) -> Result<()> {
    let last_seeded_at_ms: Option<i64> = sqlx::query_scalar(
        "SELECT local_seeded_at_ms
         FROM furumusic__fed_device_identity
         WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(last_seeded_at_ms) = last_seeded_at_ms else {
        return Ok(());
    };
    if now_ms().saturating_sub(last_seeded_at_ms) < LOCAL_SEED_RECHECK_MS
        && !local_seed_needs_metadata_backfill(pool, user_id).await?
    {
        return Ok(());
    }
    seed_local_user_state(pool, user_id).await
}

async fn local_seed_needs_metadata_backfill(pool: &sqlx::PgPool, user_id: i64) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM furumusic__fed_state_like
            WHERE user_id = $1
              AND liked = true
              AND fed_json IS NULL
              AND local_track_id IS NOT NULL
         ) OR EXISTS (
            SELECT 1 FROM furumusic__fed_state_playlist_item
            WHERE user_id = $1
              AND present = true
              AND fed_json IS NULL
              AND local_track_id IS NOT NULL
         )",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn seed_local_user_state(pool: &sqlx::PgPool, user_id: i64) -> Result<()> {
    let now = now_ms();
    let like_rows = sqlx::query(
        "SELECT ult.track_id, ult.created_at, c.content_id
         FROM furumusic__user_liked_track ult
         JOIN furumusic__track t ON t.id = ult.track_id
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         WHERE ult.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    for row in like_rows {
        let content_id: String = row.get("content_id");
        let Some(content_id) = music_dht::normalize_content_id(&content_id) else {
            continue;
        };
        let track_id: i64 = row.get("track_id");
        let liked_hlc_ms = timestamp_from_iso(&row.get::<String, _>("created_at")).unwrap_or(now);
        let fed_json = synced_fed_track_for_track(pool, track_id, &content_id)
            .await?
            .map(serde_json::to_value)
            .transpose()?;
        sqlx::query(
            "INSERT INTO furumusic__fed_state_like
                (user_id, content_id, liked, hlc_ms, op_id, local_track_id, fed_json)
             VALUES ($1, $2, true, $3, $4, $5, $6)
             ON CONFLICT (user_id, content_id) DO UPDATE SET
                local_track_id = COALESCE(furumusic__fed_state_like.local_track_id, EXCLUDED.local_track_id),
                fed_json = COALESCE(furumusic__fed_state_like.fed_json, EXCLUDED.fed_json)",
        )
        .bind(user_id)
        .bind(&content_id)
        .bind(liked_hlc_ms)
        .bind(format!("local_seed:like:{track_id}"))
        .bind(track_id)
        .bind(fed_json)
        .execute(pool)
        .await?;
    }

    let playlists = sqlx::query(
        "SELECT id, title::text AS title
         FROM furumusic__playlist
         WHERE owner_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    for playlist in playlists {
        let playlist_id: i64 = playlist.get("id");
        let title: String = playlist.get("title");
        let sync_id = ensure_seed_playlist_sync_id(pool, user_id, playlist_id, &title, now).await?;

        let items = sqlx::query(
            "SELECT pt.track_id, pt.position, c.content_id
             FROM furumusic__playlist_track pt
             JOIN furumusic__track t ON t.id = pt.track_id
             JOIN furumusic__media_file m ON m.id = t.audio_file_id
             JOIN furumusic__federation_content_id_cache c
               ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
             WHERE pt.playlist_id = $1
             ORDER BY pt.position",
        )
        .bind(playlist_id)
        .fetch_all(pool)
        .await?;
        for item in items {
            let content_id: String = item.get("content_id");
            let Some(content_id) = music_dht::normalize_content_id(&content_id) else {
                continue;
            };
            let track_id: i64 = item.get("track_id");
            let position: i32 = item.get("position");
            let fed_json = synced_fed_track_for_track(pool, track_id, &content_id)
                .await?
                .map(serde_json::to_value)
                .transpose()?;
            sqlx::query(
                "INSERT INTO furumusic__fed_state_playlist_item
                    (user_id, playlist_id, content_id, present, position, hlc_ms, op_id,
                     local_track_id, fed_json)
                 VALUES ($1, $2, $3, true, $4, $5, $6, $7, $8)
                 ON CONFLICT (user_id, playlist_id, content_id) DO UPDATE SET
                    local_track_id = COALESCE(furumusic__fed_state_playlist_item.local_track_id, EXCLUDED.local_track_id),
                    fed_json = COALESCE(furumusic__fed_state_playlist_item.fed_json, EXCLUDED.fed_json)",
            )
            .bind(user_id)
            .bind(&sync_id)
            .bind(&content_id)
            .bind(position as i64)
            .bind(now)
            .bind(format!("local_seed:playlist_item:{playlist_id}:{track_id}"))
            .bind(track_id)
            .bind(fed_json)
            .execute(pool)
            .await?;
        }
    }

    let missing_likes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM furumusic__user_liked_track ult
         JOIN furumusic__track t ON t.id = ult.track_id
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         LEFT JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         WHERE ult.user_id = $1 AND c.content_id IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    let missing_playlist_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM furumusic__playlist p
         JOIN furumusic__playlist_track pt ON pt.playlist_id = p.id
         JOIN furumusic__track t ON t.id = pt.track_id
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         LEFT JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         WHERE p.owner_id = $1 AND c.content_id IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "UPDATE furumusic__fed_device_identity
         SET local_seeded_at_ms = $2
         WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await?;
    if missing_likes + missing_playlist_items != 0 {
        tracing::debug!(
            user_id,
            missing_likes,
            missing_playlist_items,
            "web fed local seed will retry after content-id cache warms"
        );
    }
    Ok(())
}

async fn set_group_id(pool: &sqlx::PgPool, user_id: i64, group_id: &str) -> Result<()> {
    sqlx::query("UPDATE furumusic__fed_device_identity SET group_id = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(group_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn own_profile(
    pool: &sqlx::PgPool,
    user_id: i64,
    user_name: &str,
    endpoint_ticket: &str,
) -> Result<DeviceProfileWire> {
    let identity = ensure_identity(pool, user_id, user_name).await?;
    let endpoint_id = ticket_endpoint_id(endpoint_ticket).unwrap_or_default();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO furumusic__fed_device
            (user_id, device_id, name, client_version, protocol_version, endpoint_id,
             endpoint_ticket, trusted_at_ms, last_seen_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
            name = EXCLUDED.name,
            client_version = EXCLUDED.client_version,
            protocol_version = EXCLUDED.protocol_version,
            endpoint_id = EXCLUDED.endpoint_id,
            endpoint_ticket = EXCLUDED.endpoint_ticket,
            last_seen_ms = EXCLUDED.last_seen_ms",
    )
    .bind(user_id)
    .bind(&identity.device_id)
    .bind(&identity.name)
    .bind(CLIENT_VERSION)
    .bind(PROTOCOL_VERSION as i32)
    .bind(&endpoint_id)
    .bind(endpoint_ticket)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(DeviceProfileWire {
        device_id: identity.device_id,
        name: identity.name,
        client_version: CLIENT_VERSION.to_string(),
        protocol_version: PROTOCOL_VERSION,
        endpoint_id,
        endpoint_ticket: endpoint_ticket.to_string(),
        revoked: false,
        revoke_cutoff_seq: None,
        updated_at_ms: now,
    })
}

async fn record_local_op(pool: &sqlx::PgPool, user_id: i64, payload: SyncOpPayload) -> Result<()> {
    let identity = ensure_identity(pool, user_id, "").await?;
    let now = now_ms();
    let row = sqlx::query(
        "UPDATE furumusic__fed_device_identity
         SET local_seq = local_seq + 1,
             last_hlc_ms = GREATEST($2, last_hlc_ms + 1)
         WHERE user_id = $1
         RETURNING local_seq, last_hlc_ms",
    )
    .bind(user_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    let seq: i64 = row.get("local_seq");
    let hlc_ms: i64 = row.get("last_hlc_ms");
    let op_id = format!("{}:{seq}", identity.device_id);
    let op = SyncOpWire {
        op_id,
        origin_device_id: identity.device_id,
        seq,
        hlc_ms,
        payload,
    };
    sqlx::query(
        "INSERT INTO furumusic__fed_sync_ops
            (user_id, op_id, origin_device_id, seq, kind, payload_json, hlc_ms,
             received_at_ms, tombstone)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id, op_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(&op.op_id)
    .bind(&op.origin_device_id)
    .bind(op.seq)
    .bind(payload_kind(&op.payload))
    .bind(serde_json::to_value(&op.payload)?)
    .bind(op.hlc_ms)
    .bind(now_ms())
    .bind(op.payload.is_tombstone())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO furumusic__fed_sync_vector (user_id, device_id, max_seq)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
            max_seq = GREATEST(furumusic__fed_sync_vector.max_seq, EXCLUDED.max_seq)",
    )
    .bind(user_id)
    .bind(&op.origin_device_id)
    .bind(op.seq)
    .execute(pool)
    .await?;
    let _ = apply_op(pool, Arc::new(PlayerDeviceHub::default()), user_id, &op).await?;
    Ok(())
}

async fn apply_ops(
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    user_id: i64,
    ops: Vec<SyncOpWire>,
) -> Result<()> {
    for op in ops {
        if !should_accept_op(pool, user_id, &op).await? {
            continue;
        }
        let inserted = sqlx::query(
            "INSERT INTO furumusic__fed_sync_ops
                (user_id, op_id, origin_device_id, seq, kind, payload_json, hlc_ms,
                 received_at_ms, tombstone)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (user_id, op_id) DO NOTHING",
        )
        .bind(user_id)
        .bind(&op.op_id)
        .bind(&op.origin_device_id)
        .bind(op.seq)
        .bind(payload_kind(&op.payload))
        .bind(serde_json::to_value(&op.payload)?)
        .bind(op.hlc_ms)
        .bind(now_ms())
        .bind(op.payload.is_tombstone())
        .execute(pool)
        .await?
        .rows_affected();
        if inserted > 0 {
            let _ = apply_op(pool, Arc::clone(&hub), user_id, &op).await?;
        }
        sqlx::query(
            "INSERT INTO furumusic__fed_sync_vector (user_id, device_id, max_seq)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id, device_id) DO UPDATE SET
                max_seq = GREATEST(furumusic__fed_sync_vector.max_seq, EXCLUDED.max_seq)",
        )
        .bind(user_id)
        .bind(&op.origin_device_id)
        .bind(op.seq)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn should_accept_op(pool: &sqlx::PgPool, user_id: i64, op: &SyncOpWire) -> Result<bool> {
    let identity = ensure_identity(pool, user_id, "").await?;
    if op.origin_device_id == identity.device_id {
        return Ok(true);
    }
    let row = sqlx::query(
        "SELECT trusted_at_ms IS NOT NULL AS trusted,
                revoked_at_ms IS NOT NULL AS revoked,
                revoke_cutoff_seq
         FROM furumusic__fed_device
         WHERE user_id = $1 AND device_id = $2",
    )
    .bind(user_id)
    .bind(&op.origin_device_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let trusted: bool = row.get("trusted");
    let revoked: bool = row.get("revoked");
    let cutoff: Option<i64> = row.get("revoke_cutoff_seq");
    Ok(trusted && (!revoked || op.seq <= cutoff.unwrap_or(0)))
}

async fn apply_op(
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    user_id: i64,
    op: &SyncOpWire,
) -> Result<bool> {
    match &op.payload {
        SyncOpPayload::TrackLikeSet {
            content_id,
            liked,
            fed,
        } => {
            apply_like_state(
                pool,
                user_id,
                content_id,
                *liked,
                fed.as_ref(),
                op.hlc_ms,
                &op.op_id,
            )
            .await
        }
        SyncOpPayload::PlaylistCreated { playlist_id, title }
        | SyncOpPayload::PlaylistRenamed { playlist_id, title } => {
            apply_playlist_state(
                pool,
                user_id,
                playlist_id,
                title,
                false,
                op.hlc_ms,
                &op.op_id,
            )
            .await
        }
        SyncOpPayload::PlaylistDeleted { playlist_id } => {
            apply_playlist_state(pool, user_id, playlist_id, "", true, op.hlc_ms, &op.op_id).await
        }
        SyncOpPayload::PlaylistTrackAdded {
            playlist_id,
            content_id,
            position,
            fed,
        } => {
            apply_playlist_item_state(
                pool,
                user_id,
                playlist_id,
                content_id,
                true,
                *position,
                fed.as_ref(),
                op.hlc_ms,
                &op.op_id,
            )
            .await
        }
        SyncOpPayload::PlaylistTrackRemoved {
            playlist_id,
            content_id,
        } => {
            apply_playlist_item_state(
                pool,
                user_id,
                playlist_id,
                content_id,
                false,
                0,
                None,
                op.hlc_ms,
                &op.op_id,
            )
            .await
        }
        SyncOpPayload::DeviceProfileSet {
            name,
            client_version,
            endpoint_ticket,
            endpoint_id,
        } => {
            let profile = DeviceProfileWire {
                device_id: op.origin_device_id.clone(),
                name: name.clone(),
                client_version: client_version.clone(),
                protocol_version: PROTOCOL_VERSION,
                endpoint_id: endpoint_id.clone(),
                endpoint_ticket: endpoint_ticket.clone(),
                revoked: false,
                revoke_cutoff_seq: None,
                updated_at_ms: op.hlc_ms,
            };
            apply_device_profile(pool, user_id, &profile, false).await?;
            Ok(false)
        }
        SyncOpPayload::DeviceTrusted { target_device_id } => {
            apply_device_trusted(pool, user_id, target_device_id, op.hlc_ms).await
        }
        SyncOpPayload::DeviceRevoked {
            target_device_id,
            target_max_seq_seen,
        } => {
            apply_device_revoked(
                pool,
                user_id,
                target_device_id,
                op.hlc_ms,
                &op.origin_device_id,
                *target_max_seq_seen,
            )
            .await
        }
        SyncOpPayload::PlaybackCommand {
            target_device_id,
            command,
        } => {
            apply_playback_command(pool, hub, user_id, target_device_id, command, &op.op_id)
                .await?;
            Ok(false)
        }
    }
}

async fn apply_like_state(
    pool: &sqlx::PgPool,
    user_id: i64,
    content_id: &str,
    liked: bool,
    fed: Option<&SyncedFedTrack>,
    hlc_ms: i64,
    op_id: &str,
) -> Result<bool> {
    let Some(content_id) = music_dht::normalize_content_id(content_id) else {
        return Ok(false);
    };
    let current = sqlx::query(
        "SELECT liked, hlc_ms, op_id
         FROM furumusic__fed_state_like
         WHERE user_id = $1 AND content_id = $2",
    )
    .bind(user_id)
    .bind(&content_id)
    .fetch_optional(pool)
    .await?;
    let apply = current.as_ref().is_none_or(|row| {
        let current_hlc: i64 = row.get("hlc_ms");
        let current_op: String = row.get("op_id");
        (hlc_ms, op_id) > (current_hlc, current_op.as_str())
    });
    if !apply {
        return Ok(false);
    }
    let local_track_id = track_id_by_content_id(pool, &content_id).await?;
    let metadata = fed.cloned();
    sqlx::query(
        "INSERT INTO furumusic__fed_state_like
            (user_id, content_id, liked, hlc_ms, op_id, local_track_id, fed_json)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (user_id, content_id) DO UPDATE SET
            liked = EXCLUDED.liked,
            hlc_ms = EXCLUDED.hlc_ms,
            op_id = EXCLUDED.op_id,
            local_track_id = EXCLUDED.local_track_id,
            fed_json = COALESCE(EXCLUDED.fed_json, furumusic__fed_state_like.fed_json)",
    )
    .bind(user_id)
    .bind(&content_id)
    .bind(liked)
    .bind(hlc_ms)
    .bind(op_id)
    .bind(local_track_id)
    .bind(metadata.map(serde_json::to_value).transpose()?)
    .execute(pool)
    .await?;
    if let Some(track_id) = local_track_id {
        if liked {
            let created_at = iso_from_ms(hlc_ms);
            sqlx::query(
                "INSERT INTO furumusic__user_liked_track (user_id, track_id, created_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (user_id, track_id) DO NOTHING",
            )
            .bind(user_id)
            .bind(track_id)
            .bind(created_at)
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "DELETE FROM furumusic__user_liked_track WHERE user_id = $1 AND track_id = $2",
            )
            .bind(user_id)
            .bind(track_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}

async fn apply_playlist_state(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: &str,
    title: &str,
    deleted: bool,
    hlc_ms: i64,
    op_id: &str,
) -> Result<bool> {
    let current = sqlx::query(
        "SELECT hlc_ms, op_id, local_playlist_id
         FROM furumusic__fed_state_playlist
         WHERE user_id = $1 AND playlist_id = $2",
    )
    .bind(user_id)
    .bind(playlist_id)
    .fetch_optional(pool)
    .await?;
    let apply = current.as_ref().is_none_or(|row| {
        let current_hlc: i64 = row.get("hlc_ms");
        let current_op: String = row.get("op_id");
        (hlc_ms, op_id) > (current_hlc, current_op.as_str())
    });
    if !apply {
        return Ok(false);
    }
    let mut local_playlist_id = current
        .as_ref()
        .and_then(|row| row.get::<Option<i64>, _>("local_playlist_id"));
    if deleted {
        if let Some(local_id) = local_playlist_id {
            sqlx::query("DELETE FROM furumusic__playlist_track WHERE playlist_id = $1")
                .bind(local_id)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM furumusic__saved_playlist WHERE playlist_id = $1")
                .bind(local_id)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM furumusic__playlist WHERE id = $1 AND owner_id = $2")
                .bind(local_id)
                .bind(user_id)
                .execute(pool)
                .await?;
        }
    } else if !title.trim().is_empty() {
        local_playlist_id = Some(
            ensure_materialized_playlist(pool, user_id, playlist_id, local_playlist_id, title)
                .await?,
        );
    }
    sqlx::query(
        "INSERT INTO furumusic__fed_state_playlist
            (user_id, playlist_id, local_playlist_id, title, deleted, hlc_ms, op_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (user_id, playlist_id) DO UPDATE SET
            local_playlist_id = COALESCE(EXCLUDED.local_playlist_id, furumusic__fed_state_playlist.local_playlist_id),
            title = EXCLUDED.title,
            deleted = EXCLUDED.deleted,
            hlc_ms = EXCLUDED.hlc_ms,
            op_id = EXCLUDED.op_id",
    )
    .bind(user_id)
    .bind(playlist_id)
    .bind(local_playlist_id)
    .bind(title)
    .bind(deleted)
    .bind(hlc_ms)
    .bind(op_id)
    .execute(pool)
    .await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn apply_playlist_item_state(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: &str,
    content_id: &str,
    present: bool,
    position: i64,
    fed: Option<&SyncedFedTrack>,
    hlc_ms: i64,
    op_id: &str,
) -> Result<bool> {
    let Some(content_id) = music_dht::normalize_content_id(content_id) else {
        return Ok(false);
    };
    let current = sqlx::query(
        "SELECT present, position, hlc_ms, op_id
         FROM furumusic__fed_state_playlist_item
         WHERE user_id = $1 AND playlist_id = $2 AND content_id = $3",
    )
    .bind(user_id)
    .bind(playlist_id)
    .bind(&content_id)
    .fetch_optional(pool)
    .await?;
    let apply = current.as_ref().is_none_or(|row| {
        let current_hlc: i64 = row.get("hlc_ms");
        let current_op: String = row.get("op_id");
        (hlc_ms, op_id) > (current_hlc, current_op.as_str())
    });
    if !apply {
        return Ok(false);
    }
    let local_track_id = track_id_by_content_id(pool, &content_id).await?;
    let local_playlist_id = if present {
        Some(ensure_playlist_for_item(pool, user_id, playlist_id).await?)
    } else {
        local_playlist_id_by_sync_id(pool, user_id, playlist_id).await?
    };
    let fed_json = fed.cloned().map(serde_json::to_value).transpose()?;
    sqlx::query(
        "INSERT INTO furumusic__fed_state_playlist_item
            (user_id, playlist_id, content_id, present, position, hlc_ms, op_id,
             local_track_id, fed_json)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id, playlist_id, content_id) DO UPDATE SET
            present = EXCLUDED.present,
            position = EXCLUDED.position,
            hlc_ms = EXCLUDED.hlc_ms,
            op_id = EXCLUDED.op_id,
            local_track_id = EXCLUDED.local_track_id,
            fed_json = COALESCE(EXCLUDED.fed_json, furumusic__fed_state_playlist_item.fed_json)",
    )
    .bind(user_id)
    .bind(playlist_id)
    .bind(&content_id)
    .bind(present)
    .bind(position)
    .bind(hlc_ms)
    .bind(op_id)
    .bind(local_track_id)
    .bind(fed_json)
    .execute(pool)
    .await?;
    if let (Some(local_playlist_id), Some(track_id)) = (local_playlist_id, local_track_id) {
        if present {
            let exists: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM furumusic__playlist_track
                 WHERE playlist_id = $1 AND track_id = $2
                 LIMIT 1",
            )
            .bind(local_playlist_id)
            .bind(track_id)
            .fetch_optional(pool)
            .await?;
            if exists.is_none() {
                sqlx::query(
                    "INSERT INTO furumusic__playlist_track
                        (playlist_id, track_id, position, added_at, added_by_user_id)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(local_playlist_id)
                .bind(track_id)
                .bind(position as i32)
                .bind(iso_from_ms(hlc_ms))
                .bind(user_id)
                .execute(pool)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE furumusic__playlist_track
                     SET position = $3
                     WHERE playlist_id = $1 AND track_id = $2",
                )
                .bind(local_playlist_id)
                .bind(track_id)
                .bind(position as i32)
                .execute(pool)
                .await?;
            }
        } else {
            sqlx::query(
                "DELETE FROM furumusic__playlist_track
                 WHERE playlist_id = $1 AND track_id = $2",
            )
            .bind(local_playlist_id)
            .bind(track_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}

async fn apply_snapshot(pool: &sqlx::PgPool, user_id: i64, snapshot: SyncSnapshot) -> Result<()> {
    for like in snapshot.likes {
        apply_like_state(
            pool,
            user_id,
            &like.content_id,
            true,
            like.fed.as_ref(),
            like.hlc_ms,
            &like.op_id,
        )
        .await?;
    }
    for like in snapshot.unlikes {
        apply_like_state(
            pool,
            user_id,
            &like.content_id,
            false,
            None,
            like.hlc_ms,
            &like.op_id,
        )
        .await?;
    }
    for playlist in snapshot.playlists {
        apply_playlist_state(
            pool,
            user_id,
            &playlist.playlist_id,
            &playlist.title,
            false,
            playlist.hlc_ms,
            &playlist.op_id,
        )
        .await?;
        for item in playlist.items {
            apply_playlist_item_state(
                pool,
                user_id,
                &playlist.playlist_id,
                &item.content_id,
                true,
                item.position,
                item.fed.as_ref(),
                item.hlc_ms,
                &item.op_id,
            )
            .await?;
        }
    }
    for playlist in snapshot.deleted_playlists {
        apply_playlist_state(
            pool,
            user_id,
            &playlist.playlist_id,
            "",
            true,
            playlist.hlc_ms,
            &playlist.op_id,
        )
        .await?;
    }
    for item in snapshot.removed_playlist_items {
        apply_playlist_item_state(
            pool,
            user_id,
            &item.playlist_id,
            &item.content_id,
            false,
            0,
            None,
            item.hlc_ms,
            &item.op_id,
        )
        .await?;
    }
    Ok(())
}

async fn apply_device_profiles(
    pool: &sqlx::PgPool,
    user_id: i64,
    devices: &[DeviceProfileWire],
) -> Result<()> {
    for profile in devices {
        apply_device_profile(pool, user_id, profile, false).await?;
    }
    Ok(())
}

async fn apply_device_profile(
    pool: &sqlx::PgPool,
    user_id: i64,
    profile: &DeviceProfileWire,
    trusted: bool,
) -> Result<()> {
    let identity = ensure_identity(pool, user_id, "").await?;
    if profile.device_id == identity.device_id {
        return Ok(());
    }
    enforce_single_user_binding(pool, user_id, &profile.device_id).await?;
    let trusted_at_ms = trusted.then_some(now_ms()).or(Some(profile.updated_at_ms));
    sqlx::query(
        "INSERT INTO furumusic__fed_device
            (user_id, device_id, name, client_version, protocol_version, endpoint_id,
             endpoint_ticket, trusted_at_ms, last_seen_ms, revoked_at_ms,
             revoke_cutoff_seq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
            name = EXCLUDED.name,
            client_version = EXCLUDED.client_version,
            protocol_version = EXCLUDED.protocol_version,
            endpoint_id = EXCLUDED.endpoint_id,
            endpoint_ticket = CASE
                WHEN EXCLUDED.endpoint_ticket <> '' THEN EXCLUDED.endpoint_ticket
                ELSE furumusic__fed_device.endpoint_ticket
            END,
            trusted_at_ms = GREATEST(COALESCE(furumusic__fed_device.trusted_at_ms, 0), COALESCE(EXCLUDED.trusted_at_ms, 0)),
            last_seen_ms = COALESCE(EXCLUDED.last_seen_ms, furumusic__fed_device.last_seen_ms),
            revoked_at_ms = CASE
                WHEN EXCLUDED.revoked_at_ms IS NOT NULL
                 AND COALESCE(furumusic__fed_device.trusted_at_ms, 0) <= EXCLUDED.revoked_at_ms
                 AND COALESCE(furumusic__fed_device.revoked_at_ms, 0) <= EXCLUDED.revoked_at_ms
                THEN EXCLUDED.revoked_at_ms
                ELSE furumusic__fed_device.revoked_at_ms
            END,
            revoke_cutoff_seq = CASE
                WHEN EXCLUDED.revoked_at_ms IS NOT NULL
                 AND COALESCE(furumusic__fed_device.trusted_at_ms, 0) <= EXCLUDED.revoked_at_ms
                 AND COALESCE(furumusic__fed_device.revoked_at_ms, 0) <= EXCLUDED.revoked_at_ms
                THEN EXCLUDED.revoke_cutoff_seq
                ELSE furumusic__fed_device.revoke_cutoff_seq
            END",
    )
    .bind(user_id)
    .bind(&profile.device_id)
    .bind(&profile.name)
    .bind(&profile.client_version)
    .bind(profile.protocol_version as i32)
    .bind(&profile.endpoint_id)
    .bind(&profile.endpoint_ticket)
    .bind(trusted_at_ms)
    .bind(Some(profile.updated_at_ms))
    .bind(profile.revoked.then_some(profile.updated_at_ms))
    .bind(profile.revoke_cutoff_seq)
    .execute(pool)
    .await?;
    Ok(())
}

async fn apply_device_trusted(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
    hlc_ms: i64,
) -> Result<bool> {
    sqlx::query(
        "INSERT INTO furumusic__fed_device (user_id, device_id, trusted_at_ms, last_seen_ms)
         VALUES ($1, $2, $3, $3)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
            trusted_at_ms = GREATEST(COALESCE(furumusic__fed_device.trusted_at_ms, 0), EXCLUDED.trusted_at_ms),
            last_seen_ms = GREATEST(COALESCE(furumusic__fed_device.last_seen_ms, 0), EXCLUDED.last_seen_ms),
            revoked_at_ms = CASE
                WHEN furumusic__fed_device.revoked_at_ms IS NOT NULL
                 AND furumusic__fed_device.revoked_at_ms <= EXCLUDED.trusted_at_ms
                THEN NULL ELSE furumusic__fed_device.revoked_at_ms END,
            revoked_by = CASE
                WHEN furumusic__fed_device.revoked_at_ms IS NOT NULL
                 AND furumusic__fed_device.revoked_at_ms <= EXCLUDED.trusted_at_ms
                THEN NULL ELSE furumusic__fed_device.revoked_by END,
            revoke_cutoff_seq = CASE
                WHEN furumusic__fed_device.revoked_at_ms IS NOT NULL
                 AND furumusic__fed_device.revoked_at_ms <= EXCLUDED.trusted_at_ms
                THEN NULL ELSE furumusic__fed_device.revoke_cutoff_seq END",
    )
    .bind(user_id)
    .bind(target_device_id)
    .bind(hlc_ms)
    .execute(pool)
    .await?;
    Ok(true)
}

async fn apply_device_revoked(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
    hlc_ms: i64,
    revoked_by: &str,
    target_max_seq_seen: i64,
) -> Result<bool> {
    sqlx::query(
        "INSERT INTO furumusic__fed_device
            (user_id, device_id, revoked_at_ms, revoked_by, revoke_cutoff_seq)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (user_id, device_id) DO UPDATE SET
            revoked_at_ms = CASE
                WHEN COALESCE(furumusic__fed_device.trusted_at_ms, 0) <= EXCLUDED.revoked_at_ms
                 AND COALESCE(furumusic__fed_device.revoked_at_ms, 0) <= EXCLUDED.revoked_at_ms
                THEN EXCLUDED.revoked_at_ms
                ELSE furumusic__fed_device.revoked_at_ms
            END,
            revoked_by = CASE
                WHEN COALESCE(furumusic__fed_device.trusted_at_ms, 0) <= EXCLUDED.revoked_at_ms
                 AND COALESCE(furumusic__fed_device.revoked_at_ms, 0) <= EXCLUDED.revoked_at_ms
                THEN EXCLUDED.revoked_by
                ELSE furumusic__fed_device.revoked_by
            END,
            revoke_cutoff_seq = CASE
                WHEN COALESCE(furumusic__fed_device.trusted_at_ms, 0) <= EXCLUDED.revoked_at_ms
                 AND COALESCE(furumusic__fed_device.revoked_at_ms, 0) <= EXCLUDED.revoked_at_ms
                THEN EXCLUDED.revoke_cutoff_seq
                ELSE furumusic__fed_device.revoke_cutoff_seq
            END",
    )
    .bind(user_id)
    .bind(target_device_id)
    .bind(hlc_ms)
    .bind(revoked_by)
    .bind(target_max_seq_seen)
    .execute(pool)
    .await?;
    Ok(true)
}

async fn apply_playback_command(
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    user_id: i64,
    target_device_id: &str,
    command: &PlaybackCommand,
    op_id: &str,
) -> Result<()> {
    let identity = ensure_identity(pool, user_id, "").await?;
    if target_device_id != identity.device_id {
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO furumusic__fed_playback_applied (user_id, op_id, applied_at_ms)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id, op_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(op_id)
    .bind(now_ms())
    .execute(pool)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    match command {
        PlaybackCommand::SetState { state, .. } => {
            enqueue_web_transfer(pool, hub, user_id, state).await?;
        }
        PlaybackCommand::ActiveChanged {
            active_device_id,
            state,
            ..
        } if active_device_id == &identity.device_id => {
            enqueue_web_transfer(pool, hub, user_id, state).await?;
        }
        PlaybackCommand::ActiveChanged { .. } => {
            let _ = hub.enqueue_fed_command(user_id, "pause", serde_json::json!({}));
        }
    }
    Ok(())
}

async fn apply_playback_snapshot(
    hub: Arc<PlayerDeviceHub>,
    pool: &sqlx::PgPool,
    user_id: i64,
    snapshot: PlaybackSnapshot,
) -> Result<()> {
    let payload = web_playback_payload(pool, &snapshot.state).await?;
    hub.apply_fed_playback_state_json(
        user_id,
        &snapshot.device_id,
        &snapshot.device_name,
        snapshot.active,
        payload,
    )
    .map_err(|message| anyhow::anyhow!(message))?;
    Ok(())
}

async fn enqueue_web_transfer(
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    user_id: i64,
    state: &PlaybackStateWire,
) -> Result<()> {
    let payload = web_playback_payload(pool, state).await?;
    if payload
        .get("tracks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tracks| tracks.is_empty())
        && !state.queue.is_empty()
    {
        return Ok(());
    }
    hub.enqueue_fed_command(user_id, "transfer_state", payload)
        .map_err(|message| anyhow::anyhow!(message))?;
    Ok(())
}

pub async fn record_web_playback_command(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
    command: &str,
    payload: serde_json::Value,
    current_state: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    ensure_web_playback_target(pool, user_id, target_device_id).await?;
    let next_state = browser_state_for_command(command, payload, current_state)?;
    let wire = playback_state_from_browser_json(pool, next_state.clone()).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaybackCommand {
            target_device_id: target_device_id.to_string(),
            command: PlaybackCommand::SetState {
                state: wire,
                seek: matches!(
                    command,
                    "seek" | "play_track" | "play_from_index" | "transfer_state" | "next" | "prev"
                ),
            },
        },
    )
    .await?;
    Ok(next_state)
}

pub async fn record_web_active_transfer(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
    previous_device_id: Option<&str>,
    state: serde_json::Value,
) -> Result<()> {
    ensure_web_playback_target(pool, user_id, target_device_id).await?;
    let wire = playback_state_from_browser_json(pool, state).await?;
    let target_name = web_playback_target_name(pool, user_id, target_device_id).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaybackCommand {
            target_device_id: target_device_id.to_string(),
            command: PlaybackCommand::SetState {
                state: wire.clone(),
                seek: true,
            },
        },
    )
    .await?;
    if let Some(previous_device_id) =
        previous_device_id.filter(|previous| *previous != target_device_id)
    {
        ensure_web_playback_target(pool, user_id, previous_device_id).await?;
        let command = PlaybackCommand::ActiveChanged {
            active_device_id: target_device_id.to_string(),
            active_device_name: target_name,
            state: wire,
        };
        record_local_op(
            pool,
            user_id,
            SyncOpPayload::PlaybackCommand {
                target_device_id: previous_device_id.to_string(),
                command,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn record_web_active_takeover(
    pool: &sqlx::PgPool,
    user_id: i64,
    previous_device_id: &str,
    state: serde_json::Value,
) -> Result<()> {
    ensure_web_playback_target(pool, user_id, previous_device_id).await?;
    let identity = ensure_identity(pool, user_id, "").await?;
    let wire = playback_state_from_browser_json(pool, state).await?;
    record_local_op(
        pool,
        user_id,
        SyncOpPayload::PlaybackCommand {
            target_device_id: previous_device_id.to_string(),
            command: PlaybackCommand::ActiveChanged {
                active_device_id: identity.device_id,
                active_device_name: identity.name,
                state: wire,
            },
        },
    )
    .await?;
    Ok(())
}

async fn ensure_web_playback_target(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
) -> Result<()> {
    let trusted: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM furumusic__fed_device
            WHERE user_id = $1
              AND device_id = $2
              AND trusted_at_ms IS NOT NULL
              AND revoked_at_ms IS NULL
        )",
    )
    .bind(user_id)
    .bind(target_device_id)
    .fetch_one(pool)
    .await?;
    anyhow::ensure!(trusted, "fed playback target is not trusted");
    Ok(())
}

async fn web_playback_target_name(
    pool: &sqlx::PgPool,
    user_id: i64,
    target_device_id: &str,
) -> Result<String> {
    let row = sqlx::query(
        "SELECT name::text AS name
         FROM furumusic__fed_device
         WHERE user_id = $1 AND device_id = $2",
    )
    .bind(user_id)
    .bind(target_device_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .and_then(|row| {
            let name: String = row.get("name");
            (!name.trim().is_empty()).then_some(name)
        })
        .unwrap_or_else(|| short_id(target_device_id)))
}

fn browser_state_for_command(
    command: &str,
    payload: serde_json::Value,
    current_state: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let mut state = if matches!(command, "play_track" | "play_from_index" | "transfer_state") {
        payload.clone()
    } else {
        current_state.unwrap_or_else(empty_browser_playback_state)
    };
    normalize_browser_playback_state(&mut state);

    match command {
        "play_track" | "play_from_index" | "transfer_state" => {
            normalize_browser_playback_state(&mut state);
            set_browser_bool(
                &mut state,
                "paused",
                browser_bool(&payload, "paused").unwrap_or(false),
            );
        }
        "pause" => set_browser_bool(&mut state, "paused", true),
        "resume" => {
            if state.get("track").is_some_and(|track| !track.is_null()) {
                set_browser_bool(&mut state, "paused", false);
            }
        }
        "seek" => {
            let time = payload
                .get("time")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0);
            set_browser_number(&mut state, "position_seconds", time);
        }
        "next" => advance_browser_state(&mut state, &payload),
        "prev" => rewind_browser_state(&mut state),
        "set_volume" => {
            let volume = payload
                .get("volume")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.7)
                .clamp(0.0, 1.0);
            set_browser_number(&mut state, "volume", volume);
        }
        "set_options" => {
            if let Some(shuffle) = browser_bool(&payload, "shuffle") {
                set_browser_bool(&mut state, "shuffle", shuffle);
            }
            if let Some(repeat) = payload
                .get("repeat_mode")
                .and_then(serde_json::Value::as_str)
            {
                set_browser_string(&mut state, "repeat_mode", normalize_repeat_label(repeat));
            }
        }
        "queue_add_end" => {
            let mut tracks = browser_tracks(&state);
            tracks.extend(browser_tracks(&payload));
            let index = browser_index(&state);
            set_browser_tracks(&mut state, tracks, index);
        }
        "queue_add_next" => {
            let mut tracks = browser_tracks(&state);
            let incoming = browser_tracks(&payload);
            let insert_at = browser_index(&state).saturating_add(1).min(tracks.len());
            tracks.splice(insert_at..insert_at, incoming);
            let index = browser_index(&state);
            set_browser_tracks(&mut state, tracks, index);
        }
        "queue_remove" => {
            let mut tracks = browser_tracks(&state);
            let index = browser_index(&state);
            let remove_index = payload
                .get("index")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-1);
            if remove_index >= 0 {
                let remove_index = remove_index as usize;
                if remove_index < tracks.len() {
                    tracks.remove(remove_index);
                    let next_index = if tracks.is_empty() {
                        0
                    } else if remove_index < index {
                        index.saturating_sub(1)
                    } else if remove_index == index {
                        index.min(tracks.len().saturating_sub(1))
                    } else {
                        index
                    };
                    set_browser_tracks(&mut state, tracks, next_index);
                }
            }
        }
        "queue_move" => {
            let mut tracks = browser_tracks(&state);
            let index = browser_index(&state);
            let from_index = payload
                .get("from_index")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-1);
            let to_index = payload
                .get("to_index")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-1);
            if from_index >= 0 && to_index >= 0 {
                let from_index = from_index as usize;
                let to_index = to_index as usize;
                if from_index < tracks.len() && to_index < tracks.len() && from_index != to_index {
                    let track = tracks.remove(from_index);
                    tracks.insert(to_index, track);
                    let next_index = if index == from_index {
                        to_index
                    } else if from_index < index && to_index >= index {
                        index.saturating_sub(1)
                    } else if from_index > index && to_index <= index {
                        index.saturating_add(1)
                    } else {
                        index
                    };
                    set_browser_tracks(&mut state, tracks, next_index);
                }
            }
        }
        "queue_clear" => {
            set_browser_tracks(&mut state, Vec::new(), 0);
        }
        other => anyhow::bail!("unsupported fed playback command: {other}"),
    }

    normalize_browser_playback_state(&mut state);
    set_browser_i64(&mut state, "updated_at_ms", now_ms());
    Ok(state)
}

fn empty_browser_playback_state() -> serde_json::Value {
    serde_json::json!({
        "track": serde_json::Value::Null,
        "tracks": [],
        "index": 0,
        "position_seconds": 0.0,
        "duration_seconds": 0.0,
        "paused": true,
        "shuffle": false,
        "repeat_mode": "off",
        "volume": 0.7,
    })
}

fn normalize_browser_playback_state(state: &mut serde_json::Value) {
    if !state.is_object() {
        *state = empty_browser_playback_state();
    }
    let mut tracks = browser_tracks(state);
    let mut index = browser_index(state);
    if tracks.is_empty()
        && let Some(track) = state.get("track").filter(|track| !track.is_null()).cloned()
    {
        tracks.push(track);
        index = 0;
    }
    set_browser_tracks(state, tracks, index);
    if state
        .get("paused")
        .and_then(serde_json::Value::as_bool)
        .is_none()
    {
        set_browser_bool(state, "paused", true);
    }
    if state
        .get("shuffle")
        .and_then(serde_json::Value::as_bool)
        .is_none()
    {
        set_browser_bool(state, "shuffle", false);
    }
    if state
        .get("repeat_mode")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        set_browser_string(state, "repeat_mode", "off");
    }
    if state
        .get("volume")
        .and_then(serde_json::Value::as_f64)
        .is_none()
    {
        set_browser_number(state, "volume", 0.7);
    }
    if state
        .get("position_seconds")
        .and_then(serde_json::Value::as_f64)
        .is_none()
    {
        set_browser_number(state, "position_seconds", 0.0);
    }
}

fn advance_browser_state(state: &mut serde_json::Value, payload: &serde_json::Value) {
    let tracks = browser_tracks(state);
    if tracks.is_empty() {
        set_browser_bool(state, "paused", true);
        return;
    }
    let current = browser_index(state).min(tracks.len().saturating_sub(1));
    let repeat = payload
        .get("repeat_mode")
        .and_then(serde_json::Value::as_str)
        .or_else(|| state.get("repeat_mode").and_then(serde_json::Value::as_str))
        .map(normalize_repeat_label)
        .unwrap_or("off");
    if repeat == "one" {
        set_browser_number(state, "position_seconds", 0.0);
        set_browser_bool(state, "paused", false);
        return;
    }
    let shuffle = browser_bool(payload, "shuffle")
        .or_else(|| browser_bool(state, "shuffle"))
        .unwrap_or(false);
    let next = if shuffle && tracks.len() > 1 {
        let mut candidate = (now_ms().unsigned_abs() as usize) % tracks.len();
        if candidate == current {
            candidate = (candidate + 1) % tracks.len();
        }
        Some(candidate)
    } else if current + 1 < tracks.len() {
        Some(current + 1)
    } else if repeat == "all" {
        Some(0)
    } else {
        None
    };
    if let Some(next) = next {
        set_browser_tracks(state, tracks, next);
        set_browser_number(state, "position_seconds", 0.0);
        set_browser_bool(state, "paused", false);
    } else {
        set_browser_bool(state, "paused", true);
    }
}

fn rewind_browser_state(state: &mut serde_json::Value) {
    let tracks = browser_tracks(state);
    if tracks.is_empty() {
        set_browser_bool(state, "paused", true);
        return;
    }
    let current = browser_index(state).min(tracks.len().saturating_sub(1));
    let position = state
        .get("position_seconds")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    if position > 3.0 {
        set_browser_number(state, "position_seconds", 0.0);
        return;
    }
    let repeat = state
        .get("repeat_mode")
        .and_then(serde_json::Value::as_str)
        .map(normalize_repeat_label)
        .unwrap_or("off");
    let previous = if current > 0 {
        Some(current - 1)
    } else if repeat == "all" {
        Some(tracks.len().saturating_sub(1))
    } else {
        None
    };
    if let Some(previous) = previous {
        set_browser_tracks(state, tracks, previous);
        set_browser_bool(state, "paused", false);
    }
    set_browser_number(state, "position_seconds", 0.0);
}

fn browser_tracks(state: &serde_json::Value) -> Vec<serde_json::Value> {
    state
        .get("tracks")
        .and_then(serde_json::Value::as_array)
        .map(|tracks| {
            tracks
                .iter()
                .filter(|track| track.is_object())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn browser_index(state: &serde_json::Value) -> usize {
    state
        .get("index")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize
}

fn set_browser_tracks(state: &mut serde_json::Value, tracks: Vec<serde_json::Value>, index: usize) {
    let index = if tracks.is_empty() {
        0
    } else {
        index.min(tracks.len().saturating_sub(1))
    };
    let track = tracks
        .get(index)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let duration = track
        .get("duration_seconds")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let object = browser_object_mut(state);
    object.insert("tracks".to_string(), serde_json::Value::Array(tracks));
    object.insert("index".to_string(), serde_json::json!(index as i64));
    object.insert("track".to_string(), track);
    object.insert("duration_seconds".to_string(), json_f64(duration));
    if object
        .get("position_seconds")
        .and_then(serde_json::Value::as_f64)
        .is_none()
    {
        object.insert("position_seconds".to_string(), json_f64(0.0));
    }
    if object.get("track").is_some_and(serde_json::Value::is_null) {
        object.insert("paused".to_string(), serde_json::Value::Bool(true));
    }
}

fn browser_object_mut(
    state: &mut serde_json::Value,
) -> &mut serde_json::Map<String, serde_json::Value> {
    if !state.is_object() {
        *state = serde_json::json!({});
    }
    state.as_object_mut().expect("browser state object")
}

fn browser_bool(state: &serde_json::Value, key: &str) -> Option<bool> {
    state.get(key).and_then(serde_json::Value::as_bool)
}

fn set_browser_bool(state: &mut serde_json::Value, key: &str, value: bool) {
    browser_object_mut(state).insert(key.to_string(), serde_json::Value::Bool(value));
}

fn set_browser_number(state: &mut serde_json::Value, key: &str, value: f64) {
    browser_object_mut(state).insert(key.to_string(), json_f64(value));
}

fn set_browser_i64(state: &mut serde_json::Value, key: &str, value: i64) {
    browser_object_mut(state).insert(key.to_string(), serde_json::json!(value));
}

fn set_browser_string(state: &mut serde_json::Value, key: &str, value: &str) {
    browser_object_mut(state).insert(
        key.to_string(),
        serde_json::Value::String(value.to_string()),
    );
}

fn normalize_repeat_label(value: &str) -> &'static str {
    match value {
        "one" => "one",
        "all" => "all",
        _ => "off",
    }
}

fn json_f64(value: f64) -> serde_json::Value {
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::json!(0.0))
}

fn cover_variant_url(file_id: Option<i64>, variant: &str) -> Option<String> {
    file_id.map(|id| format!("/api/player/cover/{id}/{variant}"))
}

async fn web_playback_payload(
    pool: &sqlx::PgPool,
    state: &PlaybackStateWire,
) -> Result<serde_json::Value> {
    let tracks = resolve_playback_tracks(pool, &state.queue).await?;
    let index = state.queue_pos.min(tracks.len().saturating_sub(1));
    Ok(serde_json::json!({
        "tracks": tracks,
        "index": index,
        "track": tracks.get(index).cloned(),
        "position_seconds": state.position_secs.max(0.0),
        "duration_seconds": tracks.get(index).and_then(|track| track.get("duration_seconds")).and_then(|v| v.as_f64()).unwrap_or(0.0),
        "paused": state.paused || !state.playing,
        "shuffle": state.shuffle,
        "repeat_mode": repeat_label(state.repeat),
        "volume": f64::from(state.volume) / 100.0,
        "updated_at_ms": now_ms(),
    }))
}

async fn local_playback_snapshot(
    pool: &sqlx::PgPool,
    hub: Arc<PlayerDeviceHub>,
    user_id: i64,
    identity: &Identity,
) -> Option<PlaybackSnapshot> {
    let state = hub.current_playback_state_json(user_id)?;
    let wire = playback_state_from_browser_json(pool, state).await.ok()?;
    Some(PlaybackSnapshot {
        device_id: identity.device_id.clone(),
        device_name: identity.name.clone(),
        active: true,
        updated_at_ms: now_ms(),
        state: wire,
    })
}

async fn playback_state_from_browser_json(
    pool: &sqlx::PgPool,
    state: serde_json::Value,
) -> Result<PlaybackStateWire> {
    let tracks_value = state
        .get("tracks")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut queue = Vec::new();
    for track in tracks_value {
        if let Some(track_id) = track.get("id").and_then(serde_json::Value::as_i64) {
            queue.push(playback_track_from_track_json(pool, track_id, &track).await?);
        }
    }
    Ok(PlaybackStateWire {
        queue,
        queue_pos: state
            .get("index")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
            .max(0) as usize,
        playing: state.get("track").is_some_and(|track| !track.is_null()),
        paused: state
            .get("paused")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        idle_since_ms: None,
        position_secs: state
            .get("position_seconds")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        volume: ((state
            .get("volume")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.7)
            * 100.0)
            .round() as i64)
            .clamp(0, 100) as u8,
        shuffle: state
            .get("shuffle")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        repeat: match state
            .get("repeat_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("off")
        {
            "one" => PlaybackRepeat::One,
            "all" => PlaybackRepeat::All,
            _ => PlaybackRepeat::Off,
        },
    })
}

async fn playback_track_from_track_json(
    pool: &sqlx::PgPool,
    track_id: i64,
    track: &serde_json::Value,
) -> Result<PlaybackTrack> {
    Ok(PlaybackTrack {
        id: track_id,
        title: track
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown track")
            .to_string(),
        track_number: track
            .get("track_number")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        disc_number: track
            .get("disc_number")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        duration_seconds: track
            .get("duration_seconds")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        artist_names: names_from_track_json(track, "artists"),
        featured_artist_names: names_from_track_json(track, "featured_artists"),
        release_id: track
            .get("release_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        release_title: track
            .get("release_title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        release_year: track
            .get("release_year")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        file_path: String::new(),
        content_id: track
            .get("content_id")
            .and_then(serde_json::Value::as_str)
            .and_then(music_dht::normalize_content_id)
            .or(if track_id > 0 {
                track_content_id(pool, track_id).await?
            } else {
                None
            }),
        audio_format: track
            .get("audio_format")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
        audio_bitrate: track
            .get("audio_bitrate")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        audio_sample_rate: track
            .get("audio_sample_rate")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        audio_bit_depth: track
            .get("audio_bit_depth")
            .and_then(serde_json::Value::as_i64)
            .map(|value| value as i32),
        file_size_bytes: track
            .get("file_size_bytes")
            .and_then(serde_json::Value::as_i64),
        play_count: 0,
        fed: None,
    })
}

fn names_from_track_json(track: &serde_json::Value, field: &str) -> Vec<String> {
    track
        .get(field)
        .and_then(serde_json::Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|artist| artist.get("name").and_then(serde_json::Value::as_str))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn resolve_playback_tracks(
    pool: &sqlx::PgPool,
    tracks: &[PlaybackTrack],
) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let content_id = track
            .content_id
            .as_deref()
            .or_else(|| track.fed.as_ref().map(|fed| fed.content_id.as_str()))
            .and_then(music_dht::normalize_content_id);
        let Some(content_id) = content_id else {
            out.push(placeholder_playback_track(track, index, None));
            continue;
        };
        if let Some(track_json) = web_track_json_by_content_id(pool, &content_id).await? {
            out.push(track_json);
        } else {
            tracing::debug!(content_id, title = %track.title, "fed playback track is not available locally on web");
            out.push(placeholder_playback_track(track, index, Some(&content_id)));
        }
    }
    Ok(out)
}

async fn web_track_json_by_content_id(
    pool: &sqlx::PgPool,
    content_id: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(track_id) = track_id_by_content_id(pool, content_id).await? else {
        return Ok(None);
    };
    let Some(mut track) = web_track_json_by_id(pool, track_id).await? else {
        return Ok(None);
    };
    if let Some(object) = track.as_object_mut() {
        object.insert(
            "content_id".to_string(),
            serde_json::Value::String(content_id.to_string()),
        );
    }
    Ok(Some(track))
}

async fn web_track_json_by_id(
    pool: &sqlx::PgPool,
    track_id: i64,
) -> Result<Option<serde_json::Value>> {
    let row = sqlx::query(
        "SELECT t.id, t.title::text AS title, t.track_number, t.disc_number,
                t.duration_seconds, r.id AS release_id, r.title::text AS release_title,
                r.year AS release_year, mf.audio_format, mf.audio_bitrate,
                mf.audio_sample_rate, mf.audio_bit_depth, mf.file_size_bytes,
                COALESCE(t.cover_file_id, r.cover_file_id) AS cover_file_id
         FROM furumusic__track t
         JOIN furumusic__release r ON r.id = t.release_id
         LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
         WHERE t.id = $1 AND t.is_hidden = false AND r.is_hidden = false",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let artists = track_artist_json(pool, track_id, "main").await?;
    let featured_artists = track_artist_json(pool, track_id, "featuring").await?;
    Ok(Some(serde_json::json!({
        "id": row.get::<i64, _>("id"),
        "title": row.get::<String, _>("title"),
        "track_number": row.get::<Option<i32>, _>("track_number"),
        "disc_number": row.get::<Option<i32>, _>("disc_number"),
        "duration_seconds": row.get::<f64, _>("duration_seconds"),
        "artists": artists,
        "featured_artists": featured_artists,
        "release_id": row.get::<i64, _>("release_id"),
        "release_title": row.get::<String, _>("release_title"),
        "release_year": row.get::<Option<i32>, _>("release_year"),
        "cover_url": cover_variant_url(row.get::<Option<i64>, _>("cover_file_id"), "medium"),
        "stream_url": format!("/api/player/stream/{track_id}"),
        "uploader_name": "Fed",
        "audio_format": row.get::<Option<String>, _>("audio_format"),
        "audio_bitrate": row.get::<Option<i32>, _>("audio_bitrate"),
        "audio_sample_rate": row.get::<Option<i32>, _>("audio_sample_rate"),
        "audio_bit_depth": row.get::<Option<i32>, _>("audio_bit_depth"),
        "file_size_bytes": row.get::<Option<i64>, _>("file_size_bytes"),
    })))
}

async fn track_artist_json(
    pool: &sqlx::PgPool,
    track_id: i64,
    role: &str,
) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT a.id, a.name::text AS name
         FROM furumusic__track_artist ta
         JOIN furumusic__artist a ON a.id = ta.artist_id
         WHERE ta.track_id = $1 AND ta.role = $2
         ORDER BY ta.position",
    )
    .bind(track_id)
    .bind(role)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| serde_json::json!({"id": row.get::<i64, _>("id"), "name": row.get::<String, _>("name")}))
        .collect())
}

async fn track_artist_names(pool: &sqlx::PgPool, track_id: i64, role: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT a.name::text AS name
         FROM furumusic__track_artist ta
         JOIN furumusic__artist a ON a.id = ta.artist_id
         WHERE ta.track_id = $1 AND ta.role = $2
         ORDER BY ta.position",
    )
    .bind(track_id)
    .bind(role)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| row.get("name")).collect())
}

fn placeholder_playback_track(
    track: &PlaybackTrack,
    index: usize,
    content_id: Option<&str>,
) -> serde_json::Value {
    let artists = if !track.artist_names.is_empty() {
        track.artist_names.clone()
    } else if let Some(fed) = &track.fed {
        fed.artist_names.clone()
    } else {
        Vec::new()
    };
    let featured = if !track.featured_artist_names.is_empty() {
        track.featured_artist_names.clone()
    } else if let Some(fed) = &track.fed {
        fed.featured_artist_names.clone()
    } else {
        Vec::new()
    };
    let title = track
        .fed
        .as_ref()
        .map(|fed| fed.title.as_str())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&track.title);
    let release_title = track
        .fed
        .as_ref()
        .and_then(|fed| fed.release_title.as_deref())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&track.release_title);
    let content_id = content_id
        .map(ToString::to_string)
        .or_else(|| track.content_id.clone())
        .or_else(|| track.fed.as_ref().map(|fed| fed.content_id.clone()));
    serde_json::json!({
        "id": placeholder_track_id(content_id.as_deref(), index),
        "title": title,
        "track_number": track.track_number,
        "disc_number": track.disc_number,
        "duration_seconds": track.duration_seconds,
        "artists": artist_names_to_json(&artists),
        "featured_artists": artist_names_to_json(&featured),
        "release_id": 0,
        "release_title": release_title,
        "release_year": track.release_year,
        "cover_url": serde_json::Value::Null,
        "stream_url": "",
        "uploader_name": "Federation",
        "audio_format": track.audio_format.clone(),
        "audio_bitrate": track.audio_bitrate,
        "audio_sample_rate": track.audio_sample_rate,
        "audio_bit_depth": track.audio_bit_depth,
        "file_size_bytes": track.file_size_bytes,
        "content_id": content_id,
        "unavailable": true,
        "unavailable_reason": "Not available on this web player yet",
    })
}

fn artist_names_to_json(names: &[String]) -> Vec<serde_json::Value> {
    names
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.trim().is_empty())
        .map(|(index, name)| serde_json::json!({"id": -((index as i64) + 1), "name": name}))
        .collect()
}

fn placeholder_track_id(content_id: Option<&str>, index: usize) -> i64 {
    let mut bytes = [0u8; 8];
    let hash = blake3::hash(content_id.unwrap_or("").as_bytes());
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    let value = i64::from_le_bytes(bytes).unsigned_abs() % 9_000_000_000_000;
    -((value as i64) + index as i64 + 1)
}

async fn track_id_by_content_id(pool: &sqlx::PgPool, content_id: &str) -> Result<Option<i64>> {
    let Some(content_id) = music_dht::normalize_content_id(content_id) else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT t.id
         FROM furumusic__track t
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         JOIN furumusic__release r ON r.id = t.release_id
         WHERE c.content_id = $1 AND t.is_hidden = false AND r.is_hidden = false
         LIMIT 1",
    )
    .bind(content_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn track_content_id(pool: &sqlx::PgPool, track_id: i64) -> Result<Option<String>> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT c.content_id
         FROM furumusic__track t
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         WHERE t.id = $1
         LIMIT 1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    Ok(value.and_then(|value| music_dht::normalize_content_id(&value)))
}

async fn synced_fed_track_for_track(
    pool: &sqlx::PgPool,
    track_id: i64,
    content_id: &str,
) -> Result<Option<SyncedFedTrack>> {
    let Some(content_id) = music_dht::normalize_content_id(content_id) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT t.title::text AS title, t.track_number, t.disc_number,
                t.duration_seconds, r.title::text AS release_title, r.year AS release_year
         FROM furumusic__track t
         JOIN furumusic__release r ON r.id = t.release_id
         WHERE t.id = $1",
    )
    .bind(track_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let duration = row.get::<f64, _>("duration_seconds");
    Ok(Some(SyncedFedTrack {
        item_id: format!("web:{track_id}"),
        owner: "WEB".to_string(),
        title: row.get("title"),
        artist_names: track_artist_names(pool, track_id, "main").await?,
        featured_artist_names: track_artist_names(pool, track_id, "featuring").await?,
        year: row.get("release_year"),
        duration_seconds: duration
            .is_finite()
            .then(|| duration.round().max(0.0) as i64),
        content_id,
        release_title: row.get("release_title"),
        track_number: row.get("track_number"),
        disc_number: row.get("disc_number"),
    }))
}

async fn ensure_materialized_playlist(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_sync_id: &str,
    local_playlist_id: Option<i64>,
    title: &str,
) -> Result<i64> {
    if let Some(id) = local_playlist_id {
        sqlx::query("UPDATE furumusic__playlist SET title = $1, updated_at = $2 WHERE id = $3")
            .bind(title)
            .bind(now_iso())
            .bind(id)
            .execute(pool)
            .await?;
        return Ok(id);
    }
    let now = now_iso();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO furumusic__playlist (owner_id, title, is_public, created_at, updated_at)
         VALUES ($1, $2, false, $3, $3)
         RETURNING id",
    )
    .bind(user_id)
    .bind(title)
    .bind(&now)
    .fetch_one(pool)
    .await?;
    upsert_materialized_playlist_link(pool, user_id, playlist_sync_id, id, title).await?;
    Ok(id)
}

async fn upsert_materialized_playlist_link(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_sync_id: &str,
    local_playlist_id: i64,
    title: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO furumusic__fed_state_playlist
            (user_id, playlist_id, local_playlist_id, title, deleted, hlc_ms, op_id)
         VALUES ($1, $2, $3, $4, false, 0, $5)
         ON CONFLICT (user_id, playlist_id) DO UPDATE SET
            local_playlist_id = COALESCE(furumusic__fed_state_playlist.local_playlist_id, EXCLUDED.local_playlist_id),
            title = CASE
                WHEN furumusic__fed_state_playlist.hlc_ms = 0 THEN EXCLUDED.title
                ELSE furumusic__fed_state_playlist.title
            END",
    )
    .bind(user_id)
    .bind(playlist_sync_id)
    .bind(local_playlist_id)
    .bind(title)
    .bind(format!("local_materialized:{local_playlist_id}"))
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_playlist_for_item(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_sync_id: &str,
) -> Result<i64> {
    if let Some(id) = local_playlist_id_by_sync_id(pool, user_id, playlist_sync_id).await? {
        return Ok(id);
    }
    let title = sqlx::query_scalar::<_, Option<String>>(
        "SELECT title FROM furumusic__fed_state_playlist
         WHERE user_id = $1 AND playlist_id = $2",
    )
    .bind(user_id)
    .bind(playlist_sync_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .filter(|title| !title.trim().is_empty())
    .unwrap_or_else(|| "Synced playlist".to_string());
    ensure_materialized_playlist(pool, user_id, playlist_sync_id, None, &title).await
}

async fn ensure_seed_playlist_sync_id(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
    hlc_ms: i64,
) -> Result<String> {
    if let Some(sync_id) = local_playlist_sync_id(pool, user_id, playlist_id).await? {
        return Ok(sync_id);
    }
    let sync_id = format!("webpl_{user_id}_{playlist_id}");
    sqlx::query(
        "INSERT INTO furumusic__fed_state_playlist
            (user_id, playlist_id, local_playlist_id, title, deleted, hlc_ms, op_id)
         VALUES ($1, $2, $3, $4, false, $5, $6)
         ON CONFLICT (user_id, playlist_id) DO UPDATE SET
            local_playlist_id = COALESCE(furumusic__fed_state_playlist.local_playlist_id, EXCLUDED.local_playlist_id),
            title = CASE
                WHEN furumusic__fed_state_playlist.hlc_ms = 0 THEN EXCLUDED.title
                ELSE furumusic__fed_state_playlist.title
            END,
            deleted = false",
    )
    .bind(user_id)
    .bind(&sync_id)
    .bind(playlist_id)
    .bind(title)
    .bind(hlc_ms)
    .bind(format!("local_seed:playlist:{playlist_id}"))
    .execute(pool)
    .await?;
    Ok(sync_id)
}

async fn ensure_local_playlist_sync_id(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
    title: &str,
) -> Result<String> {
    if let Some(sync_id) = local_playlist_sync_id(pool, user_id, playlist_id).await? {
        return Ok(sync_id);
    }
    let sync_id = format!("webpl_{user_id}_{playlist_id}");
    sqlx::query(
        "INSERT INTO furumusic__fed_state_playlist
            (user_id, playlist_id, local_playlist_id, title, deleted, hlc_ms, op_id)
         VALUES ($1, $2, $3, $4, false, 0, $5)
         ON CONFLICT (user_id, playlist_id) DO UPDATE SET
            local_playlist_id = COALESCE(furumusic__fed_state_playlist.local_playlist_id, EXCLUDED.local_playlist_id),
            title = CASE
                WHEN furumusic__fed_state_playlist.hlc_ms = 0 THEN EXCLUDED.title
                ELSE furumusic__fed_state_playlist.title
            END",
    )
    .bind(user_id)
    .bind(&sync_id)
    .bind(playlist_id)
    .bind(title)
    .bind(format!("local_init:{playlist_id}"))
    .execute(pool)
    .await?;
    Ok(sync_id)
}

async fn local_playlist_sync_id(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: i64,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT playlist_id FROM furumusic__fed_state_playlist
         WHERE user_id = $1 AND local_playlist_id = $2",
    )
    .bind(user_id)
    .bind(playlist_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn local_playlist_id_by_sync_id(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: &str,
) -> Result<Option<i64>> {
    let value: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT local_playlist_id FROM furumusic__fed_state_playlist
         WHERE user_id = $1 AND playlist_id = $2",
    )
    .bind(user_id)
    .bind(playlist_id)
    .fetch_optional(pool)
    .await?;
    Ok(value.flatten())
}

async fn playlist_title(pool: &sqlx::PgPool, playlist_id: i64) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT title::text FROM furumusic__playlist WHERE id = $1")
        .bind(playlist_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

async fn playlist_track_content_positions(
    pool: &sqlx::PgPool,
    playlist_id: i64,
    track_ids: &[i64],
) -> Result<Vec<(i64, String, i64)>> {
    let rows = sqlx::query(
        "SELECT pt.track_id, c.content_id, pt.position
         FROM furumusic__playlist_track pt
         JOIN furumusic__track t ON t.id = pt.track_id
         JOIN furumusic__media_file m ON m.id = t.audio_file_id
         JOIN furumusic__federation_content_id_cache c
           ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
         WHERE pt.playlist_id = $1 AND pt.track_id = ANY($2)
         ORDER BY pt.position",
    )
    .bind(playlist_id)
    .bind(track_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let track_id: i64 = row.get("track_id");
            let content_id: String = row.get("content_id");
            music_dht::normalize_content_id(&content_id)
                .map(|content_id| (track_id, content_id, row.get::<i32, _>("position") as i64))
        })
        .collect())
}

pub async fn playlist_content_ids_for_removal(
    pool: &sqlx::PgPool,
    playlist_id: i64,
    playlist_track_id: Option<i64>,
    track_id: Option<i64>,
) -> Result<Vec<String>> {
    let rows = match (playlist_track_id, track_id) {
        (Some(playlist_track_id), _) => {
            sqlx::query(
                "SELECT c.content_id
                 FROM furumusic__playlist_track pt
                 JOIN furumusic__track t ON t.id = pt.track_id
                 JOIN furumusic__media_file m ON m.id = t.audio_file_id
                 JOIN furumusic__federation_content_id_cache c
                   ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
                 WHERE pt.playlist_id = $1 AND pt.id = $2",
            )
            .bind(playlist_id)
            .bind(playlist_track_id)
            .fetch_all(pool)
            .await?
        }
        (None, Some(track_id)) => {
            sqlx::query(
                "SELECT c.content_id
                 FROM furumusic__playlist_track pt
                 JOIN furumusic__track t ON t.id = pt.track_id
                 JOIN furumusic__media_file m ON m.id = t.audio_file_id
                 JOIN furumusic__federation_content_id_cache c
                   ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
                 WHERE pt.playlist_id = $1 AND pt.track_id = $2",
            )
            .bind(playlist_id)
            .bind(track_id)
            .fetch_all(pool)
            .await?
        }
        (None, None) => Vec::new(),
    };
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let content_id: String = row.get("content_id");
            music_dht::normalize_content_id(&content_id)
        })
        .collect())
}

async fn active_remote_devices(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<StoredDevice>> {
    let identity = ensure_identity(pool, user_id, "").await?;
    let rows = sqlx::query(
        "SELECT device_id, endpoint_ticket
         FROM furumusic__fed_device
         WHERE user_id = $1
           AND trusted_at_ms IS NOT NULL
           AND revoked_at_ms IS NULL
           AND device_id <> $2
         ORDER BY last_seen_ms DESC NULLS LAST",
    )
    .bind(user_id)
    .bind(identity.device_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| StoredDevice {
            device_id: row.get("device_id"),
            endpoint_ticket: row.get("endpoint_ticket"),
        })
        .collect())
}

async fn active_device_count(pool: &sqlx::PgPool, user_id: i64) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM furumusic__fed_device
         WHERE user_id = $1 AND trusted_at_ms IS NOT NULL AND revoked_at_ms IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn vector(pool: &sqlx::PgPool, user_id: i64) -> Result<BTreeMap<String, i64>> {
    let rows =
        sqlx::query("SELECT device_id, max_seq FROM furumusic__fed_sync_vector WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("device_id"), row.get("max_seq")))
        .collect())
}

async fn note_peer_vector(
    pool: &sqlx::PgPool,
    user_id: i64,
    peer_device_id: &str,
    vector: &BTreeMap<String, i64>,
) -> Result<()> {
    for (origin, seq) in vector {
        sqlx::query(
            "INSERT INTO furumusic__fed_peer_ack
                (user_id, peer_device_id, origin_device_id, max_seq, updated_at_ms)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, peer_device_id, origin_device_id) DO UPDATE SET
                max_seq = GREATEST(furumusic__fed_peer_ack.max_seq, EXCLUDED.max_seq),
                updated_at_ms = EXCLUDED.updated_at_ms",
        )
        .bind(user_id)
        .bind(peer_device_id)
        .bind(origin)
        .bind(*seq)
        .bind(now_ms())
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn ops_for_peer(
    pool: &sqlx::PgPool,
    user_id: i64,
    peer_device_id: &str,
) -> Result<Vec<SyncOpWire>> {
    let rows = sqlx::query(
        "SELECT o.op_id, o.origin_device_id, o.seq, o.hlc_ms, o.payload_json
         FROM furumusic__fed_sync_ops o
         LEFT JOIN furumusic__fed_peer_ack a
           ON a.user_id = o.user_id
          AND a.peer_device_id = $2
          AND a.origin_device_id = o.origin_device_id
         WHERE o.user_id = $1
           AND o.seq > COALESCE(a.max_seq, 0)
         ORDER BY o.hlc_ms, o.op_id
         LIMIT $3",
    )
    .bind(user_id)
    .bind(peer_device_id)
    .bind(MAX_OPS_PER_BATCH)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let payload_json: serde_json::Value = row.get("payload_json");
            Ok(SyncOpWire {
                op_id: row.get("op_id"),
                origin_device_id: row.get("origin_device_id"),
                seq: row.get("seq"),
                hlc_ms: row.get("hlc_ms"),
                payload: serde_json::from_value(payload_json)?,
            })
        })
        .collect()
}

async fn snapshot(pool: &sqlx::PgPool, user_id: i64) -> Result<SyncSnapshot> {
    maybe_seed_local_user_state(pool, user_id).await?;
    let like_rows = sqlx::query(
        "SELECT content_id, liked, hlc_ms, op_id, fed_json
         FROM furumusic__fed_state_like
         WHERE user_id = $1
         ORDER BY content_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut likes = Vec::new();
    let mut unlikes = Vec::new();
    for row in like_rows {
        let content_id: String = row.get("content_id");
        let hlc_ms: i64 = row.get("hlc_ms");
        let op_id: String = row.get("op_id");
        if row.get::<bool, _>("liked") {
            let fed = row
                .get::<Option<serde_json::Value>, _>("fed_json")
                .map(serde_json::from_value)
                .transpose()?;
            likes.push(SnapshotLike {
                content_id,
                hlc_ms,
                op_id,
                fed,
            });
        } else {
            unlikes.push(SnapshotLikeTombstone {
                content_id,
                hlc_ms,
                op_id,
            });
        }
    }

    let playlist_rows = sqlx::query(
        "SELECT playlist_id, title, deleted, hlc_ms, op_id
         FROM furumusic__fed_state_playlist
         WHERE user_id = $1
         ORDER BY title, playlist_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut playlists = Vec::new();
    let mut deleted_playlists = Vec::new();
    for row in playlist_rows {
        let playlist_id: String = row.get("playlist_id");
        let hlc_ms: i64 = row.get("hlc_ms");
        let op_id: String = row.get("op_id");
        if row.get::<bool, _>("deleted") {
            deleted_playlists.push(SnapshotPlaylistTombstone {
                playlist_id,
                hlc_ms,
                op_id,
            });
        } else {
            let items = snapshot_playlist_items(pool, user_id, &playlist_id).await?;
            playlists.push(SnapshotPlaylist {
                playlist_id,
                title: row.get("title"),
                hlc_ms,
                op_id,
                items,
            });
        }
    }
    let removed_playlist_items = sqlx::query(
        "SELECT playlist_id, content_id, hlc_ms, op_id
         FROM furumusic__fed_state_playlist_item
         WHERE user_id = $1 AND present = false
         ORDER BY playlist_id, content_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| SnapshotPlaylistItemTombstone {
        playlist_id: row.get("playlist_id"),
        content_id: row.get("content_id"),
        hlc_ms: row.get("hlc_ms"),
        op_id: row.get("op_id"),
    })
    .collect();
    Ok(SyncSnapshot {
        likes,
        unlikes,
        playlists,
        deleted_playlists,
        removed_playlist_items,
    })
}

async fn snapshot_playlist_items(
    pool: &sqlx::PgPool,
    user_id: i64,
    playlist_id: &str,
) -> Result<Vec<SnapshotPlaylistItem>> {
    let rows = sqlx::query(
        "SELECT content_id, position, hlc_ms, op_id, fed_json
         FROM furumusic__fed_state_playlist_item
         WHERE user_id = $1 AND playlist_id = $2 AND present = true
         ORDER BY position, content_id",
    )
    .bind(user_id)
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let fed = row
                .get::<Option<serde_json::Value>, _>("fed_json")
                .map(serde_json::from_value)
                .transpose()?;
            Ok(SnapshotPlaylistItem {
                content_id: row.get("content_id"),
                position: row.get("position"),
                hlc_ms: row.get("hlc_ms"),
                op_id: row.get("op_id"),
                fed,
            })
        })
        .collect()
}

async fn device_profiles(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<DeviceProfileWire>> {
    let rows = sqlx::query(
        "SELECT device_id, name, client_version, protocol_version, endpoint_id,
                endpoint_ticket, revoked_at_ms IS NOT NULL AS revoked,
                revoke_cutoff_seq, COALESCE(last_seen_ms, trusted_at_ms, 0) AS updated_at_ms
         FROM furumusic__fed_device
         WHERE user_id = $1 AND trusted_at_ms IS NOT NULL
         ORDER BY device_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| DeviceProfileWire {
            device_id: row.get("device_id"),
            name: row.get("name"),
            client_version: row.get("client_version"),
            protocol_version: row.get::<i32, _>("protocol_version") as u16,
            endpoint_id: row.get("endpoint_id"),
            endpoint_ticket: row.get("endpoint_ticket"),
            revoked: row.get("revoked"),
            revoke_cutoff_seq: row.get("revoke_cutoff_seq"),
            updated_at_ms: row.get("updated_at_ms"),
        })
        .collect())
}

async fn mark_seen(
    pool: &sqlx::PgPool,
    user_id: i64,
    device_id: &str,
    endpoint_id: Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE furumusic__fed_device
         SET last_seen_ms = $3,
             endpoint_id = COALESCE($4, endpoint_id)
         WHERE user_id = $1 AND device_id = $2",
    )
    .bind(user_id)
    .bind(device_id)
    .bind(now_ms())
    .bind(endpoint_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_last_sync(pool: &sqlx::PgPool, user_id: i64, message: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE furumusic__fed_device_identity SET last_sync = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(message.map(|message| format!("{} · {message}", now_label())))
        .execute(pool)
        .await?;
    Ok(())
}

async fn set_last_error(pool: &sqlx::PgPool, user_id: i64, message: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE furumusic__fed_device_identity SET last_error = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(message)
        .execute(pool)
        .await?;
    Ok(())
}

async fn invite_row(
    pool: &sqlx::PgPool,
    invite_id: &str,
) -> Result<Option<(i64, String, i64, Option<i64>)>> {
    Ok(sqlx::query(
        "SELECT user_id, secret_hash, expires_at_ms, used_at_ms
         FROM furumusic__fed_device_invite
         WHERE invite_id = $1",
    )
    .bind(invite_id)
    .fetch_optional(pool)
    .await?
    .map(|row| {
        (
            row.get("user_id"),
            row.get("secret_hash"),
            row.get("expires_at_ms"),
            row.get("used_at_ms"),
        )
    }))
}

async fn pairing_already_accepted(
    pool: &sqlx::PgPool,
    user_id: i64,
    request_id: &str,
) -> Result<bool> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM furumusic__fed_pending_pairing
         WHERE user_id = $1 AND request_id = $2 AND status = 'accepted'",
    )
    .bind(user_id)
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

async fn pairing_status(
    pool: &sqlx::PgPool,
    user_id: i64,
    request_id: &str,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT status FROM furumusic__fed_pending_pairing
         WHERE user_id = $1 AND request_id = $2",
    )
    .bind(user_id)
    .bind(request_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn device_owner(pool: &sqlx::PgPool, device_id: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT user_id FROM furumusic__fed_device
         WHERE device_id = $1 AND trusted_at_ms IS NOT NULL
         LIMIT 1",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn active_device_user(pool: &sqlx::PgPool, device_id: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT user_id FROM furumusic__fed_device
         WHERE device_id = $1
           AND trusted_at_ms IS NOT NULL
           AND revoked_at_ms IS NULL
         LIMIT 1",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn enforce_single_user_binding(
    pool: &sqlx::PgPool,
    user_id: i64,
    device_id: &str,
) -> Result<()> {
    if let Some(owner_id) = device_owner(pool, device_id).await?
        && owner_id != user_id
    {
        anyhow::bail!("device already paired with another account");
    }
    Ok(())
}

async fn write_pair_reject(mut stream: ByteStream, message: &str) -> Result<()> {
    write_msg(
        &mut stream,
        &WireMessage::PairResponse {
            accepted: false,
            pending: false,
            error: Some(message.to_string()),
            group_id: None,
            profile: None,
            devices: Vec::new(),
            vector: BTreeMap::new(),
            ops: Vec::new(),
            snapshot: SyncSnapshot::default(),
            playback: None,
        },
    )
    .await?;
    finish_response(&mut stream).await
}

async fn write_sync_reject(mut stream: ByteStream, message: &str) -> Result<()> {
    write_msg(
        &mut stream,
        &WireMessage::SyncResponse {
            accepted: false,
            error: Some(message.to_string()),
            devices: Vec::new(),
            vector: BTreeMap::new(),
            ops: Vec::new(),
            snapshot: SyncSnapshot::default(),
            playback: None,
        },
    )
    .await?;
    finish_response(&mut stream).await
}

fn payload_kind(payload: &SyncOpPayload) -> &'static str {
    match payload {
        SyncOpPayload::TrackLikeSet { .. } => "track_like_set",
        SyncOpPayload::PlaylistCreated { .. } => "playlist_created",
        SyncOpPayload::PlaylistRenamed { .. } => "playlist_renamed",
        SyncOpPayload::PlaylistDeleted { .. } => "playlist_deleted",
        SyncOpPayload::PlaylistTrackAdded { .. } => "playlist_track_added",
        SyncOpPayload::PlaylistTrackRemoved { .. } => "playlist_track_removed",
        SyncOpPayload::DeviceProfileSet { .. } => "device_profile_set",
        SyncOpPayload::DeviceTrusted { .. } => "device_trusted",
        SyncOpPayload::DeviceRevoked { .. } => "device_revoked",
        SyncOpPayload::PlaybackCommand { .. } => "playback_command",
    }
}

async fn write_msg(stream: &mut ByteStream, message: &WireMessage) -> Result<()> {
    let mut payload = serde_json::to_vec(message)?;
    payload.push(b'\n');
    stream.send.write_all(&payload).await?;
    Ok(())
}

async fn finish_send(stream: &mut ByteStream) -> Result<()> {
    stream.send.finish()?;
    Ok(())
}

async fn finish_response(stream: &mut ByteStream) -> Result<()> {
    stream.send.finish()?;
    let _ = tokio::time::timeout(RESPONSE_DRAIN_TIMEOUT, stream.send.stopped()).await;
    Ok(())
}

async fn read_msg(stream: &mut ByteStream) -> Result<WireMessage> {
    let line = read_line(&mut stream.recv).await?;
    Ok(serde_json::from_slice(&line)?)
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader.read(&mut byte).await?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
        if out.len() > MAX_LINE {
            anyhow::bail!("protocol line is too large");
        }
    }
    Ok(out)
}

fn parse_invite(value: &str) -> Result<InviteWire> {
    let Some(token) = value.trim().strip_prefix("frid://i/") else {
        anyhow::bail!("usage: frid://i/<invite>");
    };
    let bytes = base64url_decode(token)?;
    let invite: InviteWire = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(invite.v == 1, "unsupported invite version");
    Ok(invite)
}

pub fn invite_network_id(value: &str) -> Result<NetworkId> {
    let invite = parse_invite(value)?;
    let ticket: PeerTicket = invite.ticket.parse().context("malformed invite ticket")?;
    Ok(ticket.network_id)
}

fn encode_invite(invite: &InviteWire) -> Result<String> {
    let bytes = serde_json::to_vec(invite)?;
    Ok(format!("frid://i/{}", base64url_encode(&bytes)))
}

fn pair_request_id(invite_id: &str, device_id: &str) -> String {
    let digest = blake3::hash(format!("{invite_id}:{device_id}").as_bytes());
    format!("pair_{}", &digest.to_hex()[..16])
}

fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

fn random_hex(bytes: usize) -> String {
    let key = SecretKey::generate();
    let mut seed = key.to_bytes().to_vec();
    while seed.len() < bytes {
        seed.extend_from_slice(blake3::hash(&seed).as_bytes());
    }
    hex_encode(&seed[..bytes])
}

fn ticket_endpoint_id(ticket: &str) -> Option<String> {
    let ticket = PeerTicket::from_str(ticket).ok()?;
    Some(ticket.endpoint_id().to_string())
}

fn repeat_label(repeat: PlaybackRepeat) -> &'static str {
    match repeat {
        PlaybackRepeat::Off => "off",
        PlaybackRepeat::One => "one",
        PlaybackRepeat::All => "all",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn iso_from_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn timestamp_from_iso(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn now_label() -> String {
    let secs = (now_ms() / 1000).max(0);
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

fn short_id(value: &str) -> String {
    value.chars().take(10).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied().unwrap_or(0);
        let b2 = bytes.get(i + 2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        }
        i += 3;
    }
    out
}

fn base64url_decode(value: &str) -> Result<Vec<u8>> {
    fn val(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i < bytes.len() {
        let a = val(bytes[i]).context("invalid base64url invite")?;
        let b = val(*bytes.get(i + 1).context("truncated base64url invite")?)
            .context("invalid base64url invite")?;
        let c = bytes.get(i + 2).and_then(|byte| val(*byte));
        let d = bytes.get(i + 3).and_then(|byte| val(*byte));
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            out.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                out.push((c << 6) | d);
            }
        }
        i += 4;
    }
    Ok(out)
}
