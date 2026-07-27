use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ArtistCard {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) image_url: Option<String>,
    pub(super) release_count: i64,
    pub(super) track_count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Paginated<T: Serialize> {
    pub(super) items: Vec<T>,
    pub(super) total: i64,
    pub(super) page: i32,
    pub(super) per_page: i32,
    pub(super) has_more: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ReleaseCard {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) cover_url: Option<String>,
    pub(super) track_count: i64,
    pub(super) uploaders: Vec<UploaderSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ArtistDetail {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) image_url: Option<String>,
    pub(super) total_track_count: i64,
    pub(super) total_play_count: i64,
    pub(super) top_tracks: Vec<TrackItem>,
    pub(super) releases: Vec<ReleaseCard>,
    pub(super) featured_tracks: Vec<ArtistAppearanceTrack>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct ArtistRef {
    pub(super) id: i64,
    pub(super) name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct TrackItem {
    pub(super) id: i64,
    pub(super) content_id: Option<String>,
    pub(super) title: String,
    pub(super) track_number: Option<i32>,
    pub(super) disc_number: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) artists: Vec<ArtistRef>,
    pub(super) featured_artists: Vec<ArtistRef>,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_year: Option<i32>,
    pub(super) cover_url: Option<String>,
    pub(super) stream_url: String,
    pub(super) uploader_name: String,
    pub(super) audio_format: Option<String>,
    pub(super) audio_bitrate: Option<i32>,
    pub(super) audio_sample_rate: Option<i32>,
    pub(super) audio_bit_depth: Option<i32>,
    pub(super) file_size_bytes: Option<i64>,
    pub(super) lastfm_listeners: Option<i64>,
    pub(super) lastfm_playcount: Option<i64>,
    pub(super) lastfm_rating: Option<f64>,
    pub(super) lastfm_updated_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlaylistTrackItem {
    pub(super) playlist_track_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) sort_key: Option<i64>,
    #[serde(flatten)]
    pub(super) track: TrackItem,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ArtistAppearanceTrack {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_year: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) artists: Vec<ArtistRef>,
    pub(super) featured_artists: Vec<ArtistRef>,
    pub(super) cover_url: Option<String>,
    pub(super) stream_url: String,
    pub(super) uploader_name: String,
    pub(super) audio_format: Option<String>,
    pub(super) audio_bitrate: Option<i32>,
    pub(super) audio_sample_rate: Option<i32>,
    pub(super) audio_bit_depth: Option<i32>,
    pub(super) file_size_bytes: Option<i64>,
    pub(super) lastfm_listeners: Option<i64>,
    pub(super) lastfm_playcount: Option<i64>,
    pub(super) lastfm_rating: Option<f64>,
    pub(super) lastfm_updated_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ReleaseDetail {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) cover_url: Option<String>,
    pub(super) artists: Vec<ArtistRef>,
    pub(super) tracks: Vec<TrackItem>,
    pub(super) uploaders: Vec<UploaderSummary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct UploaderSummary {
    pub(super) name: String,
    pub(super) track_count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlaylistCard {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) track_count: i64,
    pub(super) is_own: bool,
    pub(super) owner_name: Option<String>,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
    pub(super) kind: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(super) struct PlaybackStateDto {
    pub(super) current_track_id: Option<i64>,
    pub(super) position_ms: i32,
    pub(super) queue: Vec<i64>,
    pub(super) queue_position: i32,
    pub(super) shuffle: bool,
    pub(super) repeat_mode: String,
    pub(super) volume: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct DeviceHeartbeatRequest {
    pub(super) device_id: String,
    pub(super) user_agent: Option<String>,
    pub(super) current_jam_id: Option<String>,
    pub(super) playback_state: Option<PlayerDevicePlaybackStateDto>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct DeviceSelectRequest {
    pub(super) device_id: String,
    pub(super) current_device_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct DeviceCommandRequest {
    pub(super) target_device_id: Option<String>,
    pub(super) jam_id: Option<String>,
    pub(super) command: String,
    #[serde(default)]
    pub(super) payload: serde_json::Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerDeviceDto {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) is_current: bool,
    pub(super) is_active: bool,
    pub(super) last_seen_ms: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerJamDto {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) host_user_id: i64,
    pub(super) host_name: String,
    pub(super) is_owner: bool,
    pub(super) is_member: bool,
    pub(super) is_pending: bool,
    pub(super) is_active: bool,
    pub(super) member_count: i64,
    pub(super) host_last_seen_ms: i64,
    pub(super) host_device_online: bool,
    pub(super) members: Vec<PlayerJamMemberDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerJamMemberDto {
    pub(super) user_id: i64,
    pub(super) name: String,
    pub(super) is_joined: bool,
    pub(super) is_current_user: bool,
    pub(super) last_seen_ms: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PlayerJamCreateRequest {
    pub(super) device_id: String,
    #[serde(default)]
    pub(super) invitee_user_ids: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PlayerJamInviteRequest {
    pub(super) jam_id: String,
    pub(super) device_id: String,
    #[serde(default)]
    pub(super) invitee_user_ids: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PlayerJamJoinRequest {
    pub(super) jam_id: String,
    pub(super) device_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PlayerJamLeaveRequest {
    pub(super) jam_id: String,
    pub(super) device_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerJamUserDto {
    pub(super) id: i64,
    pub(super) username: String,
    pub(super) display_name: Option<String>,
    pub(super) email: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerDeviceCommandDto {
    pub(super) id: String,
    pub(super) command: String,
    pub(super) payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct PlayerDevicePlaybackStateDto {
    pub(super) track: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) tracks: Vec<serde_json::Value>,
    pub(super) index: i32,
    pub(super) position_seconds: f64,
    pub(super) duration_seconds: f64,
    pub(super) paused: bool,
    pub(super) shuffle: bool,
    pub(super) repeat_mode: String,
    pub(super) volume: f64,
    #[serde(default)]
    pub(super) updated_at_ms: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerDevicesResponse {
    pub(super) device_id: String,
    pub(super) active_device_id: Option<String>,
    pub(super) devices: Vec<PlayerDeviceDto>,
    pub(super) jams: Vec<PlayerJamDto>,
    pub(super) current_jam_id: Option<String>,
    pub(super) playback_state: Option<PlayerDevicePlaybackStateDto>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FedDeviceConnectRequest {
    pub(super) invite: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FedDevicePairingAnswerRequest {
    pub(super) request_id: String,
    pub(super) accept: bool,
    #[serde(default)]
    pub(super) use_requester_group: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FedDeviceRevokeRequest {
    pub(super) device_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayerDevicePollResponse {
    pub(super) device_id: String,
    pub(super) active_device_id: Option<String>,
    pub(super) devices: Vec<PlayerDeviceDto>,
    pub(super) jams: Vec<PlayerJamDto>,
    pub(super) current_jam_id: Option<String>,
    pub(super) commands: Vec<PlayerDeviceCommandDto>,
    pub(super) playback_state: Option<PlayerDevicePlaybackStateDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlaylistDetail {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) description: Option<String>,
    pub(super) is_own: bool,
    pub(super) owner_name: Option<String>,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
    pub(super) kind: String,
    pub(super) tracks: Vec<PlaylistTrackItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ShareLinkResponse {
    pub(super) token: String,
    pub(super) url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlaylistShareDetail {
    pub(super) token: String,
    pub(super) title: String,
    pub(super) tracks: Vec<TrackItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SearchResults {
    pub(super) artists: Vec<ArtistCard>,
    pub(super) releases: Vec<ReleaseCard>,
    pub(super) tracks: Vec<TrackItem>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserStats {
    pub(super) liked_tracks: i64,
    pub(super) playlists: i64,
    pub(super) plays: i64,
    pub(super) listened_minutes: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserProfile {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) role: String,
    pub(super) stats: UserStats,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct OfflineManifestResponse {
    pub(super) generated_at: String,
    pub(super) tracks: Vec<OfflineTrackManifestItem>,
    pub(super) playlists: Vec<OfflinePlaylistManifestItem>,
    pub(super) liked_track_ids: Vec<i64>,
    pub(super) followed_artist_ids: Vec<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct OfflineTrackManifestItem {
    pub(super) id: i64,
    pub(super) updated_at: String,
    pub(super) stream_url: String,
    pub(super) audio_file_id: i64,
    pub(super) audio_hash: String,
    pub(super) audio_size_bytes: i64,
    pub(super) audio_mime_type: String,
    pub(super) audio_updated_at: String,
    pub(super) cover_file_id: Option<i64>,
    pub(super) cover_url: Option<String>,
    pub(super) cover_hash: Option<String>,
    pub(super) cover_updated_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct OfflinePlaylistManifestItem {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) description: Option<String>,
    pub(super) updated_at: String,
    pub(super) is_own: bool,
    pub(super) owner_name: Option<String>,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
    pub(super) kind: String,
    pub(super) track_ids: Vec<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LastfmStatus {
    pub(super) configured: bool,
    pub(super) connected: bool,
    pub(super) username: Option<String>,
    pub(super) reauth_required: bool,
    pub(super) last_error: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LastfmActionResponse {
    pub(super) ok: bool,
    pub(super) queued: bool,
    pub(super) sent: bool,
    pub(super) message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct LastfmNowPlayingRequest {
    pub(super) track_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct LastfmScrobbleRequest {
    pub(super) track_id: i64,
    pub(super) started_at: Option<i64>,
    pub(super) listened_seconds: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AgentQueueStatus {
    pub(super) queued_count: i64,
    pub(super) processing_count: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(super) struct UserUploadTrack {
    pub(super) track: TrackItem,
    pub(super) media_file_id: i64,
    pub(super) is_hidden: bool,
    pub(super) release_is_hidden: bool,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) uploaded_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserUploadRelease {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) is_hidden: bool,
    pub(super) artists: Vec<ArtistRef>,
    pub(super) tracks: Vec<UserUploadTrack>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserUploadReviewFields {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) album: String,
    pub(super) year: String,
    pub(super) track_number: String,
    pub(super) genre: String,
    pub(super) featured_artists: Vec<String>,
    pub(super) release_type: String,
    pub(super) notes: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserUploadReviewItem {
    pub(super) id: i64,
    pub(super) status: String,
    pub(super) filename: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) error_message: Option<String>,
    pub(super) fields: UserUploadReviewFields,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserUploadQueueItem {
    pub(super) id: i64,
    pub(super) status: String,
    pub(super) filename: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) error_message: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct UserUploadsPage {
    pub(super) tracks: Vec<UserUploadTrack>,
    pub(super) releases: Vec<UserUploadRelease>,
    pub(super) pending: Vec<UserUploadReviewItem>,
    pub(super) queued: Vec<UserUploadQueueItem>,
    pub(super) pending_total: i64,
    pub(super) queued_total: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UserUploadTrackUpdateRequest {
    pub(super) title: Option<String>,
    pub(super) artist_names: Option<Vec<String>>,
    pub(super) featured_artist_names: Option<Vec<String>>,
    pub(super) release_title: Option<String>,
    pub(super) release_type: Option<String>,
    pub(super) release_year: Option<String>,
    pub(super) track_number: Option<String>,
    pub(super) disc_number: Option<String>,
    pub(super) is_hidden: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UserUploadReleaseUpdateRequest {
    pub(super) title: Option<String>,
    pub(super) artist_names: Option<Vec<String>>,
    pub(super) release_type: Option<String>,
    pub(super) year: Option<String>,
    pub(super) is_hidden: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UserUploadBulkTrackUpdateRequest {
    pub(super) track_ids: Vec<i64>,
    pub(super) artist_names: Option<Vec<String>>,
    pub(super) featured_artist_names: Option<Vec<String>>,
    pub(super) release_title: Option<String>,
    pub(super) release_type: Option<String>,
    pub(super) release_year: Option<String>,
    pub(super) is_hidden: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct UserUploadReviewUpdateRequest {
    pub(super) title: Option<String>,
    pub(super) artist: Option<String>,
    pub(super) album: Option<String>,
    pub(super) year: Option<String>,
    pub(super) track_number: Option<String>,
    pub(super) genre: Option<String>,
    pub(super) featured_artists: Option<Vec<String>>,
    pub(super) release_type: Option<String>,
    pub(super) notes: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayHistoryItem {
    pub(super) id: i64,
    pub(super) track_id: i64,
    pub(super) track_title: String,
    pub(super) release_title: Option<String>,
    pub(super) track: TrackItem,
    pub(super) played_at: String,
    pub(super) duration_listened: Option<i32>,
    pub(super) completed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PlayHistoryPage {
    pub(super) items: Vec<PlayHistoryItem>,
    pub(super) total: i64,
    pub(super) page: i32,
    pub(super) per_page: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LikeStatus {
    pub(super) liked: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ContentTrackMutation {
    pub(super) content_id: String,
    pub(super) liked: Option<bool>,
    pub(super) playlist_id: Option<i64>,
    pub(super) position: Option<i64>,
    pub(super) federation: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PrepareFederatedTrackRequest {
    pub(super) content_id: String,
    pub(super) owner: String,
    pub(super) item_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FederationArtworkQuery {
    pub(super) owner: String,
    pub(super) item_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FederationArtistQuery {
    pub(super) name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FederationCatalogArtworkQuery {
    pub(super) owner: String,
    pub(super) artist: String,
    pub(super) release: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FederationArtworkDiscoveryQuery {
    pub(super) artist: String,
    pub(super) release: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct LikedIds {
    pub(super) track_ids: Vec<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FollowStatus {
    pub(super) followed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FollowedArtists {
    pub(super) artist_ids: Vec<i64>,
    pub(super) artists: Vec<ArtistCard>,
}
