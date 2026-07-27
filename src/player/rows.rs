#[derive(sqlx::FromRow)]
pub(super) struct ArtistRow {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) image_file_id: Option<i64>,
    pub(super) release_count: i64,
    pub(super) track_count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct CountRow {
    pub(super) count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct PlayerJamUserRow {
    pub(super) id: i64,
    pub(super) username: String,
    pub(super) display_name: Option<String>,
    pub(super) email: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(super) struct ReleaseRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) cover_file_id: Option<i64>,
    pub(super) track_count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct ArtistBriefRow {
    pub(super) id: i64,
    pub(super) name: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct TrackArtistRow {
    pub(super) track_id: i64,
    pub(super) artist_id: i64,
    pub(super) artist_name: String,
    pub(super) role: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct ReleaseArtistRefRow {
    pub(super) release_id: i64,
    pub(super) artist_id: i64,
    pub(super) artist_name: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct MediaFileRow {
    pub(super) file_path: String,
    pub(super) mime_type: String,
    pub(super) file_size_bytes: i64,
    pub(super) sha256_hash: String,
    pub(super) created_at: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct PlaybackStateRow {
    pub(super) current_track_id: Option<i64>,
    pub(super) position_ms: i32,
    pub(super) queue_json: String,
    pub(super) queue_position: i32,
    pub(super) shuffle: bool,
    pub(super) repeat_mode: String,
    pub(super) volume: f64,
}

#[derive(sqlx::FromRow)]
pub(super) struct PlaylistRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) track_count: i64,
    pub(super) is_own: bool,
    pub(super) owner_name: String,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
}

#[derive(sqlx::FromRow)]
pub(super) struct PlaylistInfoRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) description: Option<String>,
    pub(super) owner_id: i64,
    pub(super) owner_name: String,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
}

#[derive(sqlx::FromRow)]
pub(super) struct PlaylistTrackRow {
    pub(super) playlist_track_id: Option<i64>,
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) track_number: Option<i32>,
    pub(super) disc_number: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) cover_file_id: Option<i64>,
    pub(super) release_cover_file_id: Option<i64>,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_year: Option<i32>,
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

#[derive(sqlx::FromRow)]
pub(super) struct UploadedTrackRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) track_number: Option<i32>,
    pub(super) disc_number: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) cover_file_id: Option<i64>,
    pub(super) release_cover_file_id: Option<i64>,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_type: String,
    pub(super) release_year: Option<i32>,
    pub(super) release_is_hidden: bool,
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
    pub(super) media_file_id: i64,
    pub(super) is_hidden: bool,
    pub(super) year: Option<i32>,
    pub(super) uploaded_at: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct UserUploadQueueRow {
    pub(super) id: i64,
    pub(super) status: String,
    pub(super) input_path: Option<String>,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) error_message: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(super) struct UserUploadReviewRow {
    pub(super) id: i64,
    pub(super) status: String,
    pub(super) input_path: Option<String>,
    pub(super) result_json: Option<String>,
    pub(super) context_json: Option<String>,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) error_message: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(super) struct UploadTrackEditRow {
    pub(super) release_id: i64,
    pub(super) title: String,
    pub(super) track_number: Option<i32>,
    pub(super) disc_number: Option<i32>,
    pub(super) is_hidden: bool,
    pub(super) release_title: String,
    pub(super) release_type: String,
    pub(super) release_year: Option<i32>,
}

#[derive(sqlx::FromRow)]
pub(super) struct AppearanceTrackRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_year: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) cover_file_id: Option<i64>,
    pub(super) release_cover_file_id: Option<i64>,
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

#[derive(sqlx::FromRow)]
pub(super) struct SearchArtistRow {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) image_file_id: Option<i64>,
    pub(super) release_count: i64,
    pub(super) track_count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct SearchReleaseRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) cover_file_id: Option<i64>,
    pub(super) track_count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct SearchTrackRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) track_number: Option<i32>,
    pub(super) disc_number: Option<i32>,
    pub(super) duration_seconds: f64,
    pub(super) cover_file_id: Option<i64>,
    pub(super) release_cover_file_id: Option<i64>,
    pub(super) release_id: i64,
    pub(super) release_title: String,
    pub(super) release_year: Option<i32>,
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

#[derive(sqlx::FromRow)]
pub(super) struct ReleaseUploaderRow {
    pub(super) release_id: i64,
    pub(super) uploader_name: String,
    pub(super) track_count: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct ReleaseInfoRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) release_type: String,
    pub(super) year: Option<i32>,
    pub(super) cover_file_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct OfflineTrackManifestRow {
    pub(super) id: i64,
    pub(super) updated_at: String,
    pub(super) audio_file_id: i64,
    pub(super) audio_hash: String,
    pub(super) audio_size_bytes: i64,
    pub(super) audio_mime_type: String,
    pub(super) audio_updated_at: String,
    pub(super) cover_file_id: Option<i64>,
    pub(super) cover_hash: Option<String>,
    pub(super) cover_updated_at: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(super) struct OfflinePlaylistManifestRow {
    pub(super) id: i64,
    pub(super) title: String,
    pub(super) description: Option<String>,
    pub(super) updated_at: String,
    pub(super) is_own: bool,
    pub(super) owner_name: String,
    pub(super) is_public: bool,
    pub(super) is_saved: bool,
    pub(super) track_ids: Vec<i64>,
}
