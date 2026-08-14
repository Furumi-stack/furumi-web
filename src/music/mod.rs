/// Music library models and migrations.
///
/// This module contains all database models related to the music library:
/// content (files, artists, releases, tracks, genres), user interactions
/// (likes, follows, playlists, play history, playback state), and the
/// AI-agent processing queue.
use cot::db::{Auto, Database, LimitedString, Model};

// ---------------------------------------------------------------------------
// MediaFile — audio files and cover art on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct MediaFile {
    #[model(primary_key)]
    pub id: Auto<i64>,
    /// "audio" or "cover_art"
    pub file_type: LimitedString<32>,
    /// Relative path on disk from the media root
    pub file_path: String,
    /// Original filename as uploaded
    pub original_filename: LimitedString<255>,
    /// MIME type, e.g. "audio/flac", "image/jpeg"
    pub mime_type: LimitedString<100>,
    /// File size in bytes
    pub file_size_bytes: i64,
    /// SHA-256 hex digest for dedup
    pub sha256_hash: LimitedString<64>,
    // Audio-specific fields (NULL for non-audio files)
    /// e.g. "mp3", "flac", "ogg", "wav"
    pub audio_format: Option<String>,
    /// Bitrate in kbps
    pub audio_bitrate: Option<i32>,
    /// Sample rate in Hz
    pub audio_sample_rate: Option<i32>,
    /// Bit depth (16, 24, 32)
    pub audio_bit_depth: Option<i32>,
    /// FK -> user who imported/uploaded the source, NULL when unknown.
    pub uploaded_by_user_id: Option<i64>,
    /// Stable display label for the uploader. Unknown uploads are stored as "UFO".
    pub uploader_name: LimitedString<255>,
    pub created_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// Artist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct Artist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    /// Canonical display name
    pub name: LimitedString<255>,
    /// Normalized for search/dedup (lowercase, stripped)
    pub name_sort: LimitedString<255>,
    /// FK → media_file (artist image), nullable
    pub image_file_id: Option<i64>,
    pub is_hidden: bool,
    /// NULL = human-created, non-NULL = LLM model that created it
    pub model_name: Option<String>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
}

fn now_iso() -> LimitedString<32> {
    LimitedString::new(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap()
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

impl Artist {
    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn get_by_id(db: &Database, artist_id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(artist_id)).await
    }

    pub async fn create(
        db: &Database,
        name: &str,
        model_name: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let mut artist = Self {
            id: Auto::auto(),
            name: LimitedString::new(name).unwrap(),
            name_sort: LimitedString::new(&normalize_name(name)).unwrap(),
            image_file_id: None,
            is_hidden: false,
            model_name: model_name.map(str::to_owned),
            created_at: now.clone(),
            updated_at: now,
        };
        artist.insert(db).await?;
        Ok(artist)
    }

    pub async fn update_name(&mut self, db: &Database, name: &str) -> cot::db::Result<()> {
        self.name = LimitedString::new(name).unwrap();
        self.name_sort = LimitedString::new(&normalize_name(name)).unwrap();
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn set_image_file_id(
        &mut self,
        db: &Database,
        file_id: Option<i64>,
    ) -> cot::db::Result<()> {
        self.image_file_id = file_id;
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn delete_by_id(db: &Database, artist_id: i64) -> cot::db::Result<()> {
        cot::db::query!(Artist, $id == Auto::Fixed(artist_id))
            .delete(db)
            .await?;
        Ok(())
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }

    pub fn name_str(&self) -> &str {
        &self.name
    }

    pub fn is_hidden(&self) -> bool {
        self.is_hidden
    }
}

// ---------------------------------------------------------------------------
// Release (album / single / EP / etc.)
// ---------------------------------------------------------------------------

pub const RELEASE_TYPES: &[(&str, &str, &str)] = &[
    ("album", "Album", "Альбом"),
    ("single", "Single", "Сингл"),
    ("ep", "EP", "EP"),
    ("compilation", "Compilation", "Сборник"),
    ("mixtape", "Mixtape", "Микстейп"),
    ("live", "Live", "Концерт"),
    ("soundtrack", "Soundtrack", "Саундтрек"),
    ("remix", "Remix", "Ремикс"),
    ("demo", "Demo", "Демо"),
];

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct Release {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub title: LimitedString<255>,
    /// Normalized for search/dedup
    pub title_sort: LimitedString<255>,
    /// One of: album, single, ep, compilation, mixtape, live, soundtrack, remix, demo
    pub release_type: LimitedString<32>,
    pub year: Option<i32>,
    /// FK → media_file (cover art), nullable
    pub cover_file_id: Option<i64>,
    pub total_tracks: Option<i32>,
    pub total_discs: Option<i32>,
    pub is_hidden: bool,
    /// NULL = human-created, non-NULL = LLM model that created it
    pub model_name: Option<String>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
}

#[allow(dead_code)]
impl Release {
    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn get_by_id(db: &Database, release_id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(release_id)).await
    }

    pub async fn create(
        db: &Database,
        title: &str,
        release_type: &str,
        year: Option<i32>,
        model_name: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let mut release = Self {
            id: Auto::auto(),
            title: LimitedString::new(title).unwrap(),
            title_sort: LimitedString::new(&normalize_name(title)).unwrap(),
            release_type: LimitedString::new(release_type).unwrap(),
            year,
            cover_file_id: None,
            total_tracks: None,
            total_discs: None,
            is_hidden: false,
            model_name: model_name.map(str::to_owned),
            created_at: now.clone(),
            updated_at: now,
        };
        release.insert(db).await?;
        Ok(release)
    }

    pub async fn update_fields(
        &mut self,
        db: &Database,
        title: &str,
        release_type: &str,
        year: Option<i32>,
    ) -> cot::db::Result<()> {
        self.title = LimitedString::new(title).unwrap();
        self.title_sort = LimitedString::new(&normalize_name(title)).unwrap();
        self.release_type = LimitedString::new(release_type).unwrap();
        self.year = year;
        self.updated_at = now_iso();
        self.save(db).await
    }

    pub async fn delete_by_id(db: &Database, release_id: i64) -> cot::db::Result<()> {
        // Also clean up release_artist links
        cot::db::query!(ReleaseArtist, $release_id == release_id)
            .delete(db)
            .await?;
        cot::db::query!(Release, $id == Auto::Fixed(release_id))
            .delete(db)
            .await?;
        Ok(())
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }

    pub fn title_str(&self) -> &str {
        &self.title
    }

    pub fn release_type_str(&self) -> &str {
        &self.release_type
    }

    pub fn year_val(&self) -> Option<i32> {
        self.year
    }

    pub fn year_display(&self) -> String {
        self.year.map(|y| y.to_string()).unwrap_or_default()
    }

    pub fn is_hidden(&self) -> bool {
        self.is_hidden
    }
}

// ---------------------------------------------------------------------------
// ReleaseArtist — M2M between releases and artists
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct ReleaseArtist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub release_id: i64,
    pub artist_id: i64,
    /// Display order
    pub position: i32,
}

impl ReleaseArtist {
    pub async fn find_by_release(db: &Database, release_id: i64) -> cot::db::Result<Vec<Self>> {
        cot::db::query!(ReleaseArtist, $release_id == release_id)
            .all(db)
            .await
    }

    pub async fn find_by_artist(db: &Database, artist_id: i64) -> cot::db::Result<Vec<Self>> {
        cot::db::query!(ReleaseArtist, $artist_id == artist_id)
            .all(db)
            .await
    }

    pub async fn count_by_artist(db: &Database, artist_id: i64) -> cot::db::Result<u64> {
        cot::db::query!(ReleaseArtist, $artist_id == artist_id)
            .count(db)
            .await
    }

    pub async fn set_artists(
        db: &Database,
        release_id: i64,
        artist_ids: &[i64],
    ) -> cot::db::Result<()> {
        // Remove existing links
        cot::db::query!(ReleaseArtist, $release_id == release_id)
            .delete(db)
            .await?;
        // Insert new links
        for (pos, &aid) in artist_ids.iter().enumerate() {
            let mut link = Self {
                id: Auto::auto(),
                release_id,
                artist_id: aid,
                position: pos as i32,
            };
            link.insert(db).await?;
        }
        Ok(())
    }

    pub fn artist_id(&self) -> i64 {
        self.artist_id
    }

    pub fn release_id(&self) -> i64 {
        self.release_id
    }
}

// ---------------------------------------------------------------------------
// Track
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct Track {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub title: LimitedString<255>,
    /// Normalized for search/dedup
    pub title_sort: LimitedString<255>,
    /// FK → release
    pub release_id: i64,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    /// Duration in seconds (float stored as f64)
    pub duration_seconds: f64,
    /// FK → media_file (audio)
    pub audio_file_id: i64,
    /// FK → media_file (cover art), nullable — falls back to release cover
    pub cover_file_id: Option<i64>,
    pub year: Option<i32>,
    pub is_hidden: bool,
    /// NULL = human-created, non-NULL = LLM model that created it
    pub model_name: Option<String>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// TrackArtist — M2M between tracks and artists (with role)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct TrackArtist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub track_id: i64,
    pub artist_id: i64,
    /// "main", "featuring", "remixer", "producer"
    pub role: LimitedString<32>,
    /// Display order
    pub position: i32,
}

impl TrackArtist {
    pub async fn count_by_artist(db: &Database, artist_id: i64) -> cot::db::Result<u64> {
        cot::db::query!(TrackArtist, $artist_id == artist_id)
            .count(db)
            .await
    }

    pub async fn create(
        db: &Database,
        track_id: i64,
        artist_id: i64,
        role: &str,
        position: i32,
    ) -> cot::db::Result<Self> {
        let mut link = Self {
            id: Auto::auto(),
            track_id,
            artist_id,
            role: LimitedString::new(role).unwrap(),
            position,
        };
        link.insert(db).await?;
        Ok(link)
    }
}

// ---------------------------------------------------------------------------
// Genre
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct Genre {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub name: LimitedString<100>,
    /// Normalized for dedup (lowercase, trimmed)
    pub name_normalized: LimitedString<100>,
}

// ---------------------------------------------------------------------------
// TrackGenre — M2M between tracks and genres
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct TrackGenre {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub track_id: i64,
    pub genre_id: i64,
}

// ---------------------------------------------------------------------------
// UserLikedTrack
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct UserLikedTrack {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub user_id: i64,
    pub track_id: i64,
    pub created_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// UserFollowedArtist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct UserFollowedArtist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub user_id: i64,
    pub artist_id: i64,
    pub created_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// Playlist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct Playlist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    /// FK → user (owner/creator)
    pub owner_id: i64,
    pub title: LimitedString<255>,
    pub description: Option<String>,
    pub is_public: bool,
    /// FK → media_file (custom cover), nullable
    pub cover_file_id: Option<i64>,
    /// FK → playlist (original, if this is a fork), nullable
    pub forked_from_id: Option<i64>,
    pub created_at: LimitedString<32>,
    pub updated_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// PlaylistTrack
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct PlaylistTrack {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub playlist_id: i64,
    pub track_id: i64,
    /// Order within the playlist
    pub position: i32,
    pub added_at: LimitedString<32>,
    /// FK → user (who added this track)
    pub added_by_user_id: i64,
}

// ---------------------------------------------------------------------------
// SavedPlaylist — user "follows" someone else's playlist
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct SavedPlaylist {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub user_id: i64,
    pub playlist_id: i64,
    pub saved_at: LimitedString<32>,
}

// ---------------------------------------------------------------------------
// PlayHistory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct PlayHistory {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub user_id: i64,
    pub track_id: i64,
    pub played_at: LimitedString<32>,
    /// How many seconds the user actually listened
    pub duration_listened: Option<i32>,
    /// Did the user listen to the end?
    pub completed: bool,
}

// ---------------------------------------------------------------------------
// PlaybackState — one per user, current queue + position
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct PlaybackState {
    #[model(primary_key)]
    pub id: Auto<i64>,
    pub user_id: i64,
    /// FK → track (currently playing), nullable
    pub current_track_id: Option<i64>,
    /// Current position in the track, in milliseconds
    pub position_ms: i32,
    /// JSON array of track IDs
    pub queue_json: String,
    /// Index of the current track in the queue
    pub queue_position: i32,
    pub shuffle: bool,
    /// "off", "all", "one"
    pub repeat_mode: LimitedString<16>,
    /// Volume level 0.0 – 1.0
    pub volume: f64,
    pub updated_at: LimitedString<32>,
}

impl Track {
    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn create(
        db: &Database,
        title: &str,
        release_id: i64,
        track_number: Option<i32>,
        disc_number: Option<i32>,
        duration_seconds: f64,
        audio_file_id: i64,
        year: Option<i32>,
        model_name: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let mut track = Self {
            id: Auto::auto(),
            title: LimitedString::new(title).unwrap(),
            title_sort: LimitedString::new(&normalize_name(title)).unwrap(),
            release_id,
            track_number,
            disc_number,
            duration_seconds,
            audio_file_id,
            cover_file_id: None,
            year,
            is_hidden: false,
            model_name: model_name.map(str::to_owned),
            created_at: now.clone(),
            updated_at: now,
        };
        track.insert(db).await?;
        Ok(track)
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }
}

#[allow(dead_code)]
impl MediaFile {
    pub async fn create(
        db: &Database,
        file_type: &str,
        file_path: &str,
        original_filename: &str,
        mime_type: &str,
        file_size_bytes: i64,
        sha256_hash: &str,
        audio_format: Option<&str>,
        audio_bitrate: Option<i32>,
        audio_sample_rate: Option<i32>,
        audio_bit_depth: Option<i32>,
        uploaded_by_user_id: Option<i64>,
        uploader_name: Option<&str>,
    ) -> cot::db::Result<Self> {
        let now = now_iso();
        let uploader_name = uploader_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("UFO");
        let mut mf = Self {
            id: Auto::auto(),
            file_type: LimitedString::new(file_type).unwrap(),
            file_path: file_path.to_owned(),
            original_filename: LimitedString::new(original_filename).unwrap(),
            mime_type: LimitedString::new(mime_type).unwrap(),
            file_size_bytes,
            sha256_hash: LimitedString::new(sha256_hash).unwrap(),
            audio_format: audio_format.map(str::to_owned),
            audio_bitrate,
            audio_sample_rate,
            audio_bit_depth,
            uploaded_by_user_id,
            uploader_name: LimitedString::new(uploader_name).unwrap(),
            created_at: now,
        };
        mf.insert(db).await?;
        Ok(mf)
    }

    pub fn id_val(&self) -> i64 {
        self.id.unwrap()
    }

    pub async fn list_all(db: &Database) -> cot::db::Result<Vec<Self>> {
        Self::objects().all(db).await
    }

    pub async fn get_by_id(db: &Database, id: i64) -> cot::db::Result<Option<Self>> {
        Self::get_by_primary_key(db, Auto::Fixed(id)).await
    }

    pub async fn delete_by_id(db: &Database, id: i64) -> cot::db::Result<()> {
        db.raw(&format!(
            "DELETE FROM furumusic__media_file WHERE id = {}",
            id
        ))
        .await?;
        Ok(())
    }

    pub fn file_type_str(&self) -> &str {
        &self.file_type
    }

    pub fn file_path_str(&self) -> &str {
        &self.file_path
    }

    pub fn original_filename_str(&self) -> &str {
        &self.original_filename
    }

    pub fn mime_type_str(&self) -> &str {
        &self.mime_type
    }

    pub fn sha256_hash_str(&self) -> &str {
        &self.sha256_hash
    }

    pub fn audio_format_str(&self) -> &str {
        self.audio_format.as_deref().unwrap_or("")
    }

    pub fn created_at_str(&self) -> &str {
        &self.created_at
    }

    pub fn file_size_display(&self) -> String {
        let bytes = self.file_size_bytes;
        if bytes >= 1_073_741_824 {
            format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
        } else if bytes >= 1_048_576 {
            format!("{:.1} MB", bytes as f64 / 1_048_576.0)
        } else if bytes >= 1024 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else {
            format!("{bytes} B")
        }
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

pub mod db_migrations {
    use cot::db::migrations::{self, Field, Operation, SyncDynMigration};
    use cot::db::{DatabaseField, Identifier, LimitedString};

    // -- M0006: create furumusic__media_file ----------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0006CreateMediaFile;

    impl migrations::Migration for M0006CreateMediaFile {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0006_create_media_file";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0005_oidc_link_indexes",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__media_file"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("file_type"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("file_path"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("original_filename"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("mime_type"),
                    <LimitedString<100> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("file_size_bytes"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("sha256_hash"),
                    <LimitedString<64> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("audio_format"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("audio_bitrate"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("audio_sample_rate"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("audio_bit_depth"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0007: create furumusic__artist --------------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0007CreateArtist;

    impl migrations::Migration for M0007CreateArtist {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0007_create_artist";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0006_create_media_file",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__artist"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("name"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("name_sort"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("image_file_id"),
                    <i64 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("is_hidden"), <bool as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0008: create furumusic__release -------------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0008CreateRelease;

    impl migrations::Migration for M0008CreateRelease {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0008_create_release";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0007_create_artist",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__release"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("title"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("title_sort"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("release_type"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("year"), <i32 as DatabaseField>::TYPE).set_null(true),
                Field::new(
                    Identifier::new("cover_file_id"),
                    <i64 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("total_tracks"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("total_discs"), <i32 as DatabaseField>::TYPE)
                    .set_null(true),
                Field::new(Identifier::new("is_hidden"), <bool as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0009: create furumusic__release_artist ------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0009CreateReleaseArtist;

    impl migrations::Migration for M0009CreateReleaseArtist {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0009_create_release_artist";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0008_create_release",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__release_artist"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("release_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("artist_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("position"), <i32 as DatabaseField>::TYPE),
            ])
            .build()];
    }

    // -- M0010: create furumusic__track ---------------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0010CreateTrack;

    impl migrations::Migration for M0010CreateTrack {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0010_create_track";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0009_create_release_artist",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__track"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("title"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("title_sort"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("release_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("track_number"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("disc_number"), <i32 as DatabaseField>::TYPE)
                    .set_null(true),
                Field::new(
                    Identifier::new("duration_seconds"),
                    <f64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("audio_file_id"),
                    <i64 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("cover_file_id"),
                    <i64 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("year"), <i32 as DatabaseField>::TYPE).set_null(true),
                Field::new(Identifier::new("is_hidden"), <bool as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0011: create furumusic__track_artist --------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0011CreateTrackArtist;

    impl migrations::Migration for M0011CreateTrackArtist {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0011_create_track_artist";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0010_create_track",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__track_artist"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("track_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("artist_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("role"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("position"), <i32 as DatabaseField>::TYPE),
            ])
            .build()];
    }

    // -- M0012: create furumusic__genre + furumusic__track_genre ---------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0012CreateGenreTables;

    impl migrations::Migration for M0012CreateGenreTables {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0012_create_genre_tables";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0011_create_track_artist",
            )];
        const OPERATIONS: &'static [Operation] = &[
            Operation::create_model()
                .table_name(Identifier::new("furumusic__genre"))
                .fields(&[
                    Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                        .primary_key()
                        .auto(),
                    Field::new(
                        Identifier::new("name"),
                        <LimitedString<100> as DatabaseField>::TYPE,
                    )
                    .unique(),
                    Field::new(
                        Identifier::new("name_normalized"),
                        <LimitedString<100> as DatabaseField>::TYPE,
                    ),
                ])
                .build(),
            Operation::create_model()
                .table_name(Identifier::new("furumusic__track_genre"))
                .fields(&[
                    Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                        .primary_key()
                        .auto(),
                    Field::new(Identifier::new("track_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(Identifier::new("genre_id"), <i64 as DatabaseField>::TYPE),
                ])
                .build(),
        ];
    }

    // -- M0013: create furumusic__user_liked_track ----------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0013CreateUserLikedTrack;

    impl migrations::Migration for M0013CreateUserLikedTrack {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0013_create_user_liked_track";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0012_create_genre_tables",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__user_liked_track"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("track_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0014: create furumusic__user_followed_artist ------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0014CreateUserFollowedArtist;

    impl migrations::Migration for M0014CreateUserFollowedArtist {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0014_create_user_followed_artist";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0013_create_user_liked_track",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__user_followed_artist"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("artist_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0015: create playlist tables ----------------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0015CreatePlaylistTables;

    impl migrations::Migration for M0015CreatePlaylistTables {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0015_create_playlist_tables";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0014_create_user_followed_artist",
            )];
        const OPERATIONS: &'static [Operation] = &[
            Operation::create_model()
                .table_name(Identifier::new("furumusic__playlist"))
                .fields(&[
                    Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                        .primary_key()
                        .auto(),
                    Field::new(Identifier::new("owner_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(
                        Identifier::new("title"),
                        <LimitedString<255> as DatabaseField>::TYPE,
                    ),
                    Field::new(
                        Identifier::new("description"),
                        <String as DatabaseField>::TYPE,
                    )
                    .set_null(true),
                    Field::new(Identifier::new("is_public"), <bool as DatabaseField>::TYPE),
                    Field::new(
                        Identifier::new("cover_file_id"),
                        <i64 as DatabaseField>::TYPE,
                    )
                    .set_null(true),
                    Field::new(
                        Identifier::new("forked_from_id"),
                        <i64 as DatabaseField>::TYPE,
                    )
                    .set_null(true),
                    Field::new(
                        Identifier::new("created_at"),
                        <LimitedString<32> as DatabaseField>::TYPE,
                    ),
                    Field::new(
                        Identifier::new("updated_at"),
                        <LimitedString<32> as DatabaseField>::TYPE,
                    ),
                ])
                .build(),
            Operation::create_model()
                .table_name(Identifier::new("furumusic__playlist_track"))
                .fields(&[
                    Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                        .primary_key()
                        .auto(),
                    Field::new(Identifier::new("playlist_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(Identifier::new("track_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(Identifier::new("position"), <i32 as DatabaseField>::TYPE),
                    Field::new(
                        Identifier::new("added_at"),
                        <LimitedString<32> as DatabaseField>::TYPE,
                    ),
                    Field::new(
                        Identifier::new("added_by_user_id"),
                        <i64 as DatabaseField>::TYPE,
                    ),
                ])
                .build(),
            Operation::create_model()
                .table_name(Identifier::new("furumusic__saved_playlist"))
                .fields(&[
                    Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                        .primary_key()
                        .auto(),
                    Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(Identifier::new("playlist_id"), <i64 as DatabaseField>::TYPE),
                    Field::new(
                        Identifier::new("saved_at"),
                        <LimitedString<32> as DatabaseField>::TYPE,
                    ),
                ])
                .build(),
        ];
    }

    // -- M0016: create furumusic__play_history --------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0016CreatePlayHistory;

    impl migrations::Migration for M0016CreatePlayHistory {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0016_create_play_history";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0015_create_playlist_tables",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__play_history"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(Identifier::new("track_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("played_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("duration_listened"),
                    <i32 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("completed"), <bool as DatabaseField>::TYPE),
            ])
            .build()];
    }

    // -- M0017: create furumusic__playback_state ------------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0017CreatePlaybackState;

    impl migrations::Migration for M0017CreatePlaybackState {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0017_create_playback_state";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0016_create_play_history",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__playback_state"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("current_track_id"),
                    <i64 as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("position_ms"), <i32 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("queue_json"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("queue_position"),
                    <i32 as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("shuffle"), <bool as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("repeat_mode"),
                    <LimitedString<16> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
            ])
            .build()];
    }

    // -- M0018: create furumusic__processing_task -----------------------------

    #[derive(Debug, Copy, Clone)]
    pub struct M0018CreateProcessingTask;

    impl migrations::Migration for M0018CreateProcessingTask {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0018_create_processing_task";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0017_create_playback_state",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__processing_task"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("status"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("task_type"),
                    <LimitedString<64> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("input_path"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("context_json"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("result_json"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("error_message"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(Identifier::new("attempts"), <i32 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("max_attempts"),
                    <i32 as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("created_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("updated_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("started_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("completed_at"),
                    <LimitedString<32> as DatabaseField>::TYPE,
                )
                .set_null(true),
            ])
            .build()];
    }

    // -- M0019: indexes for all music tables ----------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_music_indexes(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        let stmts = [
            // media_file: lookup by hash for dedup
            "CREATE INDEX idx_media_file_sha256 ON furumusic__media_file (sha256_hash)",
            // media_file: filter by type
            "CREATE INDEX idx_media_file_type ON furumusic__media_file (file_type)",
            // artist: search by normalized name
            "CREATE INDEX idx_artist_name_sort ON furumusic__artist (name_sort)",
            // release: search by normalized title
            "CREATE INDEX idx_release_title_sort ON furumusic__release (title_sort)",
            // release: filter by type
            "CREATE INDEX idx_release_type ON furumusic__release (release_type)",
            // release_artist: unique pair + lookup
            "CREATE UNIQUE INDEX idx_release_artist_uniq ON furumusic__release_artist (release_id, artist_id)",
            "CREATE INDEX idx_release_artist_artist ON furumusic__release_artist (artist_id)",
            // track: search by normalized title
            "CREATE INDEX idx_track_title_sort ON furumusic__track (title_sort)",
            // track: FK to release
            "CREATE INDEX idx_track_release ON furumusic__track (release_id)",
            // track: FK to audio file
            "CREATE INDEX idx_track_audio_file ON furumusic__track (audio_file_id)",
            // track_artist: unique triple + lookups
            "CREATE UNIQUE INDEX idx_track_artist_uniq ON furumusic__track_artist (track_id, artist_id, role)",
            "CREATE INDEX idx_track_artist_artist ON furumusic__track_artist (artist_id)",
            // track_genre: unique pair + lookup
            "CREATE UNIQUE INDEX idx_track_genre_uniq ON furumusic__track_genre (track_id, genre_id)",
            "CREATE INDEX idx_track_genre_genre ON furumusic__track_genre (genre_id)",
            // genre: lookup by normalized name
            "CREATE INDEX idx_genre_normalized ON furumusic__genre (name_normalized)",
            // user_liked_track: unique pair + lookup by track
            "CREATE UNIQUE INDEX idx_user_liked_track_uniq ON furumusic__user_liked_track (user_id, track_id)",
            "CREATE INDEX idx_user_liked_track_track ON furumusic__user_liked_track (track_id)",
            // user_followed_artist: unique pair + lookup by artist
            "CREATE UNIQUE INDEX idx_user_followed_artist_uniq ON furumusic__user_followed_artist (user_id, artist_id)",
            "CREATE INDEX idx_user_followed_artist_artist ON furumusic__user_followed_artist (artist_id)",
            // playlist: owner lookup
            "CREATE INDEX idx_playlist_owner ON furumusic__playlist (owner_id)",
            // playlist_track: ordered tracks in playlist + lookup by track
            "CREATE INDEX idx_playlist_track_playlist ON furumusic__playlist_track (playlist_id, position)",
            "CREATE INDEX idx_playlist_track_track ON furumusic__playlist_track (track_id)",
            // saved_playlist: unique pair + lookup by playlist
            "CREATE UNIQUE INDEX idx_saved_playlist_uniq ON furumusic__saved_playlist (user_id, playlist_id)",
            "CREATE INDEX idx_saved_playlist_playlist ON furumusic__saved_playlist (playlist_id)",
            // play_history: user timeline + lookup by track
            "CREATE INDEX idx_play_history_user ON furumusic__play_history (user_id, played_at)",
            "CREATE INDEX idx_play_history_track ON furumusic__play_history (track_id)",
            // playback_state: one per user
            "CREATE UNIQUE INDEX idx_playback_state_user ON furumusic__playback_state (user_id)",
            // processing_task: queue polling (status + created_at)
            "CREATE INDEX idx_processing_task_status ON furumusic__processing_task (status, created_at)",
        ];

        for stmt in stmts {
            ctx.db.raw(stmt).await?;
        }

        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0019CreateMusicIndexes;

    impl migrations::Migration for M0019CreateMusicIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0019_create_music_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0018_create_processing_task",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::custom(create_music_indexes).build()];
    }

    // -- M0020: enable pg_trgm extension --------------------------------------

    #[cot::db::migrations::migration_op]
    async fn enable_pg_trgm(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db.raw("CREATE EXTENSION IF NOT EXISTS pg_trgm").await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0020EnablePgTrgm;

    impl migrations::Migration for M0020EnablePgTrgm {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0020_enable_pg_trgm";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0019_create_music_indexes",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::custom(enable_pg_trgm).build()];
    }

    // -- M0021: GIN trigram indexes for fuzzy search --------------------------

    #[cot::db::migrations::migration_op]
    async fn create_trgm_indexes(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db.raw("CREATE INDEX idx_artist_name_sort_trgm ON furumusic__artist USING gin (name_sort gin_trgm_ops)").await?;
        ctx.db.raw("CREATE INDEX idx_release_title_sort_trgm ON furumusic__release USING gin (title_sort gin_trgm_ops)").await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0021CreateTrgmIndexes;

    impl migrations::Migration for M0021CreateTrgmIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0021_create_trgm_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0020_enable_pg_trgm",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::custom(create_trgm_indexes).build()];
    }

    // -- M0022: GIN trigram index on track.title_sort ---------------------------

    #[cot::db::migrations::migration_op]
    async fn create_track_trgm_index(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db.raw("CREATE INDEX IF NOT EXISTS idx_track_title_sort_trgm ON furumusic__track USING gin (title_sort gin_trgm_ops)").await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0022CreateTrackTrgmIndex;

    impl migrations::Migration for M0022CreateTrackTrgmIndex {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0022_create_track_trgm_index";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0021_create_trgm_indexes",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_track_trgm_index).build()];
    }

    // -- M0028: add model_name to artist, release, track -----------------------

    #[cot::db::migrations::migration_op]
    async fn add_model_name_columns(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furumusic__artist ADD COLUMN model_name VARCHAR(128) DEFAULT NULL")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__release ADD COLUMN model_name VARCHAR(128) DEFAULT NULL")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__track ADD COLUMN model_name VARCHAR(128) DEFAULT NULL")
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0028AddModelNameColumns;

    impl migrations::Migration for M0028AddModelNameColumns {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0028_add_model_name_columns";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0027_create_processing_stats",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(add_model_name_columns).build()];
    }

    // -- M0029: add volume column to playback_state ----------------------------

    #[cot::db::migrations::migration_op]
    async fn add_playback_volume(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furumusic__playback_state ADD COLUMN volume DOUBLE PRECISION NOT NULL DEFAULT 0.7")
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0029AddPlaybackVolume;

    impl migrations::Migration for M0029AddPlaybackVolume {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0029_add_playback_volume";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0028_add_model_name_columns",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::custom(add_playback_volume).build()];
    }

    // -- M0030: add uploader attribution to media_file ------------------------

    #[cot::db::migrations::migration_op]
    async fn add_media_file_uploader(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furumusic__media_file ADD COLUMN uploaded_by_user_id BIGINT DEFAULT NULL")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__media_file ADD COLUMN uploader_name VARCHAR(255) NOT NULL DEFAULT 'UFO'")
            .await?;
        ctx.db
            .raw("CREATE INDEX IF NOT EXISTS idx_media_file_uploaded_by_user ON furumusic__media_file (uploaded_by_user_id)")
            .await?;
        ctx.db
            .raw("CREATE INDEX IF NOT EXISTS idx_media_file_uploader_name ON furumusic__media_file (uploader_name)")
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0030AddMediaFileUploader;

    impl migrations::Migration for M0030AddMediaFileUploader {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0030_add_media_file_uploader";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0029_add_playback_volume",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(add_media_file_uploader).build()];
    }

    // -- M0031: persistent torrent import sessions ---------------------------

    #[cot::db::migrations::migration_op]
    async fn create_torrent_session(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__torrent_session (
                    id VARCHAR(36) PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    name TEXT NOT NULL,
                    info_hash VARCHAR(80) NOT NULL,
                    source_kind VARCHAR(32) NOT NULL,
                    source_label TEXT,
                    torrent_bytes BYTEA NOT NULL,
                    files_json TEXT NOT NULL,
                    selected_files_json TEXT NOT NULL DEFAULT '[]',
                    status VARCHAR(32) NOT NULL,
                    total_size BIGINT NOT NULL DEFAULT 0,
                    selected_size BIGINT NOT NULL DEFAULT 0,
                    downloaded_bytes BIGINT NOT NULL DEFAULT 0,
                    uploaded_bytes BIGINT NOT NULL DEFAULT 0,
                    progress_percent DOUBLE PRECISION NOT NULL DEFAULT 0,
                    error TEXT,
                    created_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    completed_at VARCHAR(32)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_torrent_session_user_updated
                   ON furumusic__torrent_session (user_id, updated_at DESC)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_torrent_session_user_status
                   ON furumusic__torrent_session (user_id, status)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0031CreateTorrentSession;

    impl migrations::Migration for M0031CreateTorrentSession {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0031_create_torrent_session";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0030_add_media_file_uploader",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_torrent_session).build()];
    }

    // -- M0032: Last.fm track popularity ------------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_lastfm_track_popularity(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furumusic__track ADD COLUMN lastfm_listeners BIGINT")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__track ADD COLUMN lastfm_playcount BIGINT")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__track ADD COLUMN lastfm_rating DOUBLE PRECISION")
            .await?;
        ctx.db
            .raw("ALTER TABLE furumusic__track ADD COLUMN lastfm_updated_at VARCHAR(32)")
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__track_popularity_history (
                    id BIGSERIAL PRIMARY KEY,
                    track_id BIGINT NOT NULL,
                    source VARCHAR(32) NOT NULL,
                    listeners BIGINT NOT NULL,
                    playcount BIGINT NOT NULL,
                    rating DOUBLE PRECISION NOT NULL,
                    fetched_at VARCHAR(32) NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_track_popularity_history_track
                   ON furumusic__track_popularity_history (track_id, fetched_at DESC)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0032CreateLastfmTrackPopularity;

    impl migrations::Migration for M0032CreateLastfmTrackPopularity {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0032_create_lastfm_track_popularity";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0031_create_torrent_session",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_lastfm_track_popularity).build()];
    }

    // -- M0033: Last.fm scrobbling -----------------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_lastfm_scrobbling(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__lastfm_account (
                    user_id BIGINT PRIMARY KEY,
                    lastfm_username VARCHAR(255) NOT NULL,
                    session_key TEXT NOT NULL,
                    connected_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    last_error TEXT,
                    reauth_required BOOLEAN NOT NULL DEFAULT FALSE
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__lastfm_auth_state (
                    state VARCHAR(64) PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    created_at VARCHAR(32) NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__lastfm_scrobble_outbox (
                    id BIGSERIAL PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    track_id BIGINT NOT NULL,
                    started_at BIGINT NOT NULL,
                    listened_seconds INTEGER NOT NULL,
                    duration_seconds INTEGER NOT NULL,
                    status VARCHAR(32) NOT NULL,
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    scrobbled_at VARCHAR(32),
                    dedupe_key VARCHAR(128) NOT NULL UNIQUE
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_lastfm_scrobble_outbox_status
                   ON furumusic__lastfm_scrobble_outbox (status, created_at, id)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_lastfm_scrobble_outbox_user
                   ON furumusic__lastfm_scrobble_outbox (user_id, status, created_at)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0033CreateLastfmScrobbling;

    impl migrations::Migration for M0033CreateLastfmScrobbling {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0033_create_lastfm_scrobbling";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0032_create_lastfm_track_popularity",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_lastfm_scrobbling).build()];
    }

    // -- M0034: Artwork lookup state --------------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_artwork_lookup_state(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__artwork_lookup_state (
                    id BIGSERIAL PRIMARY KEY,
                    entity_kind VARCHAR(32) NOT NULL,
                    entity_id BIGINT NOT NULL,
                    source VARCHAR(32) NOT NULL,
                    status VARCHAR(32) NOT NULL,
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_attempt_at VARCHAR(32) NOT NULL,
                    last_error TEXT,
                    source_url TEXT,
                    UNIQUE(entity_kind, entity_id, source)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_artwork_lookup_state_retry
                   ON furumusic__artwork_lookup_state (entity_kind, source, status, last_attempt_at)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0034CreateArtworkLookupState;

    impl migrations::Migration for M0034CreateArtworkLookupState {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0034_create_artwork_lookup_state";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0033_create_lastfm_scrobbling",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_artwork_lookup_state).build()];
    }

    // -- M0035: Weighted metadata tags for tracks, releases, and artists ----

    #[cot::db::migrations::migration_op]
    async fn create_entity_genre_tags(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__entity_genre_tag (
                    id BIGSERIAL PRIMARY KEY,
                    entity_kind VARCHAR(32) NOT NULL,
                    entity_id BIGINT NOT NULL,
                    genre_id BIGINT NOT NULL,
                    source VARCHAR(32) NOT NULL,
                    weight DOUBLE PRECISION NOT NULL DEFAULT 1,
                    updated_at VARCHAR(32) NOT NULL,
                    UNIQUE(entity_kind, entity_id, genre_id, source)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_entity_genre_tag_entity
                   ON furumusic__entity_genre_tag (entity_kind, entity_id, source)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_entity_genre_tag_genre
                   ON furumusic__entity_genre_tag (genre_id, entity_kind)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0035CreateEntityGenreTags;

    impl migrations::Migration for M0035CreateEntityGenreTags {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0035_create_entity_genre_tags";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0034_create_artwork_lookup_state",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_entity_genre_tags).build()];
    }

    // -- M0036: External metadata identifiers --------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_external_metadata_ids(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__external_metadata_id (
                    id BIGSERIAL PRIMARY KEY,
                    entity_kind VARCHAR(32) NOT NULL,
                    entity_id BIGINT NOT NULL,
                    source VARCHAR(32) NOT NULL,
                    id_kind VARCHAR(64) NOT NULL,
                    external_id VARCHAR(255) NOT NULL,
                    confidence DOUBLE PRECISION NOT NULL DEFAULT 1,
                    updated_at VARCHAR(32) NOT NULL,
                    UNIQUE(entity_kind, entity_id, source, id_kind)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_external_metadata_id_lookup
                   ON furumusic__external_metadata_id (source, id_kind, external_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0036CreateExternalMetadataIds;

    impl migrations::Migration for M0036CreateExternalMetadataIds {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0036_create_external_metadata_ids";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0035_create_entity_genre_tags",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_external_metadata_ids).build()];
    }

    // -- M0037: Shared playlist snapshots ------------------------------------

    #[cot::db::migrations::migration_op]
    async fn create_playlist_share_links(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__playlist_share_link (
                    token VARCHAR(64) PRIMARY KEY,
                    creator_user_id BIGINT NOT NULL,
                    title TEXT NOT NULL,
                    track_ids_json TEXT NOT NULL,
                    created_at VARCHAR(32) NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_playlist_share_link_creator
                   ON furumusic__playlist_share_link (creator_user_id, created_at DESC)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0037CreatePlaylistShareLinks;

    impl migrations::Migration for M0037CreatePlaylistShareLinks {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0037_create_playlist_share_links";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0036_create_external_metadata_ids",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_playlist_share_links).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_fed_device_sync(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__federation_content_id_cache (
                    media_file_id BIGINT PRIMARY KEY,
                    sha256_hash TEXT NOT NULL,
                    content_id TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_furumusic_federation_content_id_cache_content_id
                   ON furumusic__federation_content_id_cache (content_id)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_device_identity (
                    user_id BIGINT PRIMARY KEY,
                    device_id TEXT NOT NULL UNIQUE,
                    group_id TEXT NOT NULL,
                    device_name TEXT NOT NULL,
                    local_seq BIGINT NOT NULL DEFAULT 0,
                    last_hlc_ms BIGINT NOT NULL DEFAULT 0,
                    local_seeded_at_ms BIGINT NOT NULL DEFAULT 0,
                    last_sync TEXT,
                    last_error TEXT
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_device (
                    user_id BIGINT NOT NULL,
                    device_id TEXT NOT NULL,
                    name TEXT NOT NULL DEFAULT '',
                    client_version TEXT NOT NULL DEFAULT '',
                    protocol_version INTEGER NOT NULL DEFAULT 1,
                    endpoint_id TEXT NOT NULL DEFAULT '',
                    endpoint_ticket TEXT NOT NULL DEFAULT '',
                    trusted_at_ms BIGINT,
                    last_seen_ms BIGINT,
                    revoked_at_ms BIGINT,
                    revoked_by TEXT,
                    revoke_cutoff_seq BIGINT,
                    PRIMARY KEY (user_id, device_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_fed_device_single_user
                   ON furumusic__fed_device (device_id)
                 WHERE trusted_at_ms IS NOT NULL",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_device_invite (
                    invite_id TEXT PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    secret_hash TEXT NOT NULL,
                    expires_at_ms BIGINT NOT NULL,
                    created_at_ms BIGINT NOT NULL,
                    used_at_ms BIGINT
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_pending_pairing (
                    request_id TEXT PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    device_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    client_version TEXT NOT NULL,
                    endpoint_id TEXT NOT NULL,
                    endpoint_ticket TEXT NOT NULL,
                    invite_id TEXT NOT NULL,
                    created_at_ms BIGINT NOT NULL,
                    answered_at_ms BIGINT,
                    status TEXT NOT NULL,
                    requester_group_id TEXT,
                    requester_group_active_devices BIGINT NOT NULL DEFAULT 1,
                    requester_group_devices_json TEXT NOT NULL DEFAULT '[]',
                    use_requester_group BOOLEAN NOT NULL DEFAULT false
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_fed_pending_pairing_user_status
                   ON furumusic__fed_pending_pairing (user_id, status, created_at_ms DESC)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_sync_ops (
                    user_id BIGINT NOT NULL,
                    op_id TEXT NOT NULL,
                    origin_device_id TEXT NOT NULL,
                    seq BIGINT NOT NULL,
                    kind TEXT NOT NULL,
                    payload_json JSONB NOT NULL,
                    hlc_ms BIGINT NOT NULL,
                    received_at_ms BIGINT NOT NULL,
                    tombstone BOOLEAN NOT NULL DEFAULT false,
                    PRIMARY KEY (user_id, op_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_fed_sync_ops_origin_seq
                   ON furumusic__fed_sync_ops (user_id, origin_device_id, seq)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_fed_sync_ops_tombstone
                   ON furumusic__fed_sync_ops (user_id, tombstone)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_sync_vector (
                    user_id BIGINT NOT NULL,
                    device_id TEXT NOT NULL,
                    max_seq BIGINT NOT NULL,
                    PRIMARY KEY (user_id, device_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_peer_ack (
                    user_id BIGINT NOT NULL,
                    peer_device_id TEXT NOT NULL,
                    origin_device_id TEXT NOT NULL,
                    max_seq BIGINT NOT NULL,
                    updated_at_ms BIGINT NOT NULL,
                    PRIMARY KEY (user_id, peer_device_id, origin_device_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_state_like (
                    user_id BIGINT NOT NULL,
                    content_id TEXT NOT NULL,
                    liked BOOLEAN NOT NULL,
                    hlc_ms BIGINT NOT NULL,
                    op_id TEXT NOT NULL,
                    local_track_id BIGINT,
                    fed_json JSONB,
                    PRIMARY KEY (user_id, content_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_state_playlist (
                    user_id BIGINT NOT NULL,
                    playlist_id TEXT NOT NULL,
                    local_playlist_id BIGINT,
                    title TEXT NOT NULL,
                    deleted BOOLEAN NOT NULL DEFAULT false,
                    hlc_ms BIGINT NOT NULL,
                    op_id TEXT NOT NULL,
                    PRIMARY KEY (user_id, playlist_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_fed_state_playlist_local
                   ON furumusic__fed_state_playlist (user_id, local_playlist_id)
                 WHERE local_playlist_id IS NOT NULL",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_state_playlist_item (
                    user_id BIGINT NOT NULL,
                    playlist_id TEXT NOT NULL,
                    content_id TEXT NOT NULL,
                    present BOOLEAN NOT NULL DEFAULT true,
                    position BIGINT NOT NULL DEFAULT 0,
                    hlc_ms BIGINT NOT NULL,
                    op_id TEXT NOT NULL,
                    local_track_id BIGINT,
                    fed_json JSONB,
                    PRIMARY KEY (user_id, playlist_id, content_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_fed_state_playlist_item_playlist
                   ON furumusic__fed_state_playlist_item
                      (user_id, playlist_id, present, position)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__fed_playback_applied (
                    user_id BIGINT NOT NULL,
                    op_id TEXT NOT NULL,
                    applied_at_ms BIGINT NOT NULL,
                    PRIMARY KEY (user_id, op_id)
                )",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0038CreateFedDeviceSync;

    impl migrations::Migration for M0038CreateFedDeviceSync {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0038_create_fed_device_sync";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0037_create_playlist_share_links",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_fed_device_sync).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn ensure_federation_content_id_cache(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__federation_content_id_cache (
                    media_file_id BIGINT PRIMARY KEY,
                    sha256_hash TEXT NOT NULL,
                    content_id TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_furumusic_federation_content_id_cache_content_id
                   ON furumusic__federation_content_id_cache (content_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0039EnsureFederationContentIdCache;

    impl migrations::Migration for M0039EnsureFederationContentIdCache {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0039_ensure_federation_content_id_cache";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0038_create_fed_device_sync",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(ensure_federation_content_id_cache).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_content_addressed_music_refs(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        // A track reference is durable user-facing identity. `local_track_id`
        // is availability, not identity: it may become non-NULL after a
        // federated track is materialized without changing likes/playlists.
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__track_ref (
                    id BIGSERIAL PRIMARY KEY,
                    content_id TEXT NOT NULL UNIQUE,
                    local_track_id BIGINT UNIQUE,
                    title TEXT NOT NULL,
                    release_title TEXT,
                    year INTEGER,
                    duration_seconds DOUBLE PRECISION,
                    metadata_json JSONB NOT NULL DEFAULT '{}'::jsonb,
                    metadata_authority TEXT NOT NULL DEFAULT 'local',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_track_ref_local_track
                   ON furumusic__track_ref (local_track_id)
                 WHERE local_track_id IS NOT NULL",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__federation_track_source (
                    track_ref_id BIGINT NOT NULL REFERENCES furumusic__track_ref(id)
                        ON DELETE CASCADE,
                    owner_peer_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    last_seen_ms BIGINT NOT NULL,
                    metadata_json JSONB NOT NULL DEFAULT '{}'::jsonb,
                    PRIMARY KEY (owner_peer_id, item_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_federation_track_source_ref
                   ON furumusic__federation_track_source (track_ref_id, last_seen_ms DESC)",
            )
            .await?;
        ctx.db
            .raw(
                "ALTER TABLE furumusic__user_liked_track
                   ADD COLUMN IF NOT EXISTS track_ref_id BIGINT
                       REFERENCES furumusic__track_ref(id)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_user_liked_track_ref_uniq
                   ON furumusic__user_liked_track (user_id, track_ref_id)
                 WHERE track_ref_id IS NOT NULL",
            )
            .await?;
        ctx.db
            .raw(
                "ALTER TABLE furumusic__playlist_track
                   ADD COLUMN IF NOT EXISTS track_ref_id BIGINT
                       REFERENCES furumusic__track_ref(id)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_playlist_track_ref
                   ON furumusic__playlist_track (track_ref_id)
                 WHERE track_ref_id IS NOT NULL",
            )
            .await?;
        // History deliberately remains local-track based. Only the web
        // player's existing playback report records history and triggers
        // Last.fm scrobbling.
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0040CreateContentAddressedMusicRefs;

    impl migrations::Migration for M0040CreateContentAddressedMusicRefs {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0040_create_content_addressed_music_refs";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0039_ensure_federation_content_id_cache",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_content_addressed_music_refs).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_synced_listen_history(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__listen_event (
                    user_id BIGINT NOT NULL,
                    listen_id TEXT NOT NULL,
                    content_id TEXT NOT NULL,
                    local_track_id BIGINT,
                    origin_device_id TEXT NOT NULL,
                    started_at_ms BIGINT NOT NULL,
                    listened_ms BIGINT NOT NULL,
                    track_duration_ms BIGINT,
                    ended_reason TEXT NOT NULL,
                    qualified BOOLEAN NOT NULL,
                    metadata_json JSONB NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (user_id, listen_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_listen_event_user_time
                   ON furumusic__listen_event (user_id, started_at_ms DESC, listen_id)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_listen_event_content
                   ON furumusic__listen_event (user_id, content_id)",
            )
            .await?;
        ctx.db
            .raw(
                "INSERT INTO furumusic__listen_event
                    (user_id, listen_id, content_id, local_track_id,
                     origin_device_id, started_at_ms, listened_ms,
                     track_duration_ms, ended_reason, qualified,
                     metadata_json, created_at)
                 SELECT ph.user_id,
                        'legacy-web:' || ph.id::text,
                        tr.content_id,
                        ph.track_id,
                        ident.device_id,
                        (EXTRACT(EPOCH FROM ph.played_at::timestamptz) * 1000)::bigint,
                        COALESCE(ph.duration_listened, 0)::bigint * 1000,
                        (t.duration_seconds * 1000)::bigint,
                        CASE WHEN ph.completed THEN '\"finished\"' ELSE '\"unknown\"' END,
                        ph.completed,
                        jsonb_build_object(
                            'title', t.title::text,
                            'artist_names', COALESCE((
                                SELECT jsonb_agg(a.name::text ORDER BY ta.position)
                                  FROM furumusic__track_artist ta
                                  JOIN furumusic__artist a ON a.id = ta.artist_id
                                 WHERE ta.track_id = t.id
                                   AND ta.role <> 'featuring'
                            ), '[]'::jsonb),
                            'featured_artist_names', COALESCE((
                                SELECT jsonb_agg(a.name::text ORDER BY ta.position)
                                  FROM furumusic__track_artist ta
                                  JOIN furumusic__artist a ON a.id = ta.artist_id
                                 WHERE ta.track_id = t.id
                                   AND ta.role = 'featuring'
                            ), '[]'::jsonb),
                            'release_title', r.title::text
                        ),
                        ph.played_at::text
                   FROM furumusic__play_history ph
                   JOIN furumusic__track t ON t.id = ph.track_id
                   JOIN furumusic__track_ref tr ON tr.local_track_id = ph.track_id
                   JOIN furumusic__fed_device_identity ident
                     ON ident.user_id = ph.user_id
                   LEFT JOIN furumusic__release r ON r.id = t.release_id
                 ON CONFLICT (user_id, listen_id) DO NOTHING",
            )
            .await?;
        ctx.db
            .raw(
                "ALTER TABLE furumusic__lastfm_scrobble_outbox
                   ALTER COLUMN track_id DROP NOT NULL",
            )
            .await?;
        ctx.db
            .raw(
                "ALTER TABLE furumusic__lastfm_scrobble_outbox
                   ADD COLUMN IF NOT EXISTS track_title TEXT,
                   ADD COLUMN IF NOT EXISTS artist_name TEXT,
                   ADD COLUMN IF NOT EXISTS album_title TEXT",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0041CreateSyncedListenHistory;

    impl migrations::Migration for M0041CreateSyncedListenHistory {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0041_create_synced_listen_history";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0040_create_content_addressed_music_refs",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_synced_listen_history).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn repair_legacy_listen_qualification(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "UPDATE furumusic__listen_event le
                    SET qualified = (
                        ph.completed
                        OR (
                            COALESCE(ph.duration_listened, 0) >= 5
                            AND COALESCE(t.duration_seconds, 0) > 0
                            AND COALESCE(ph.duration_listened, 0) >= LEAST(
                                COALESCE(t.duration_seconds, 0) / 2.0,
                                240.0
                            )
                        )
                    )
                   FROM furumusic__play_history ph
                   JOIN furumusic__track t ON t.id = ph.track_id
                  WHERE le.user_id = ph.user_id
                    AND le.listen_id = 'legacy-web:' || ph.id::text",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0042RepairLegacyListenQualification;

    impl migrations::Migration for M0042RepairLegacyListenQualification {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0042_repair_legacy_listen_qualification";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0041_create_synced_listen_history",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(repair_legacy_listen_qualification).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_similarity_embeddings(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__similarity_profile (
                    profile_id TEXT PRIMARY KEY,
                    model_id TEXT NOT NULL,
                    model_version TEXT NOT NULL,
                    model_sha256 TEXT NOT NULL,
                    preprocessing TEXT NOT NULL,
                    dimensions INTEGER NOT NULL,
                    active BOOLEAN NOT NULL DEFAULT FALSE,
                    created_at TEXT NOT NULL
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_similarity_profile_active
                   ON furumusic__similarity_profile (active)
                 WHERE active = TRUE",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__track_embedding (
                    track_id BIGINT NOT NULL REFERENCES furumusic__track(id)
                        ON DELETE CASCADE,
                    profile_id TEXT NOT NULL REFERENCES furumusic__similarity_profile(profile_id)
                        ON DELETE CASCADE,
                    dimensions INTEGER NOT NULL,
                    vector BYTEA NOT NULL,
                    source_sha256 TEXT NOT NULL,
                    source_content_id TEXT,
                    computed_at TEXT NOT NULL,
                    PRIMARY KEY (track_id, profile_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_track_embedding_profile
                   ON furumusic__track_embedding (profile_id, track_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0043CreateSimilarityEmbeddings;

    impl migrations::Migration for M0043CreateSimilarityEmbeddings {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0043_create_similarity_embeddings";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0042_repair_legacy_listen_qualification",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_similarity_embeddings).build()];
    }

    #[cot::db::migrations::migration_op]
    async fn add_similarity_routing_signature(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "ALTER TABLE furumusic__track_embedding
                 ADD COLUMN IF NOT EXISTS routing_signature BYTEA",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0044AddSimilarityRoutingSignature;

    impl migrations::Migration for M0044AddSimilarityRoutingSignature {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0044_add_similarity_routing_signature";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0043_create_similarity_embeddings",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(add_similarity_routing_signature).build()];
    }

    // -- M0045: persistent YouTube download jobs ----------------------------

    #[cot::db::migrations::migration_op]
    async fn create_youtube_downloads(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__youtube_download (
                    id VARCHAR(36) PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    source_url TEXT NOT NULL,
                    title TEXT NOT NULL,
                    source_kind VARCHAR(32) NOT NULL,
                    status VARCHAR(32) NOT NULL,
                    total_items INTEGER NOT NULL DEFAULT 0,
                    completed_items INTEGER NOT NULL DEFAULT 0,
                    failed_items INTEGER NOT NULL DEFAULT 0,
                    review_items INTEGER NOT NULL DEFAULT 0,
                    error TEXT,
                    created_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    completed_at VARCHAR(32)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__youtube_download_item (
                    id VARCHAR(36) PRIMARY KEY,
                    job_id VARCHAR(36) NOT NULL REFERENCES furumusic__youtube_download(id) ON DELETE CASCADE,
                    source_id VARCHAR(128) NOT NULL,
                    source_url TEXT NOT NULL,
                    title TEXT NOT NULL,
                    playlist_index INTEGER NOT NULL,
                    status VARCHAR(32) NOT NULL,
                    progress_percent DOUBLE PRECISION NOT NULL DEFAULT 0,
                    downloaded_bytes BIGINT NOT NULL DEFAULT 0,
                    total_bytes BIGINT,
                    speed_bytes_per_sec BIGINT,
                    eta_seconds BIGINT,
                    chapter_count INTEGER NOT NULL DEFAULT 0,
                    audio_file_count INTEGER NOT NULL DEFAULT 0,
                    inbox_path TEXT,
                    error TEXT,
                    created_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    completed_at VARCHAR(32),
                    UNIQUE(job_id, source_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_youtube_download_user_updated
                   ON furumusic__youtube_download (user_id, updated_at DESC)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_youtube_download_user_status
                   ON furumusic__youtube_download (user_id, status)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_youtube_download_item_job_status
                   ON furumusic__youtube_download_item (job_id, status, playlist_index)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_youtube_download_item_source
                   ON furumusic__youtube_download_item (source_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0045CreateYouTubeDownloads;

    impl migrations::Migration for M0045CreateYouTubeDownloads {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0045_create_youtube_downloads";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0044_add_similarity_routing_signature",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_youtube_downloads).build()];
    }

    // -- M0046: persistent direct-file upload history -----------------------

    #[cot::db::migrations::migration_op]
    async fn create_local_upload_history(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__local_upload (
                    id VARCHAR(36) PRIMARY KEY,
                    user_id BIGINT NOT NULL,
                    filename TEXT NOT NULL,
                    size_bytes BIGINT NOT NULL DEFAULT 0,
                    status VARCHAR(32) NOT NULL,
                    inbox_path TEXT NOT NULL,
                    error TEXT,
                    created_at VARCHAR(32) NOT NULL,
                    updated_at VARCHAR(32) NOT NULL,
                    completed_at VARCHAR(32)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_local_upload_user_updated
                   ON furumusic__local_upload (user_id, updated_at DESC)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_local_upload_user_status
                   ON furumusic__local_upload (user_id, status)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0046CreateLocalUploadHistory;

    impl migrations::Migration for M0046CreateLocalUploadHistory {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0046_create_local_upload_history";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0045_create_youtube_downloads",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_local_upload_history).build()];
    }

    // -- M0047: durable YouTube item -> imported media links ---------------

    #[cot::db::migrations::migration_op]
    async fn create_youtube_import_media_links(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE TABLE IF NOT EXISTS furumusic__youtube_import_media (
                    item_id VARCHAR(36) NOT NULL
                        REFERENCES furumusic__youtube_download_item(id) ON DELETE CASCADE,
                    media_file_id BIGINT NOT NULL
                        REFERENCES furumusic__media_file(id) ON DELETE CASCADE,
                    PRIMARY KEY (item_id, media_file_id)
                )",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX IF NOT EXISTS idx_youtube_import_media_file
                   ON furumusic__youtube_import_media (media_file_id)",
            )
            .await?;
        // Backfill links for existing imports through the inbox review hash.
        ctx.db
            .raw(
                "INSERT INTO furumusic__youtube_import_media (item_id, media_file_id)
                 SELECT DISTINCT item.id, media.id
                   FROM furumusic__youtube_download_item item
                   JOIN furumusic__pending_review review
                     ON item.inbox_path IS NOT NULL
                    AND (review.input_path = item.inbox_path
                         OR left(review.input_path, length(item.inbox_path) + 1)
                            = item.inbox_path || '/')
                   JOIN furumusic__media_file media
                     ON media.sha256_hash::text = substring(
                            review.context_json
                            from '\"sha256\"[[:space:]]*:[[:space:]]*\"([0-9a-fA-F]{64})\"'
                        )
                   JOIN furumusic__track track ON track.audio_file_id = media.id
                 WHERE review.context_json IS NOT NULL
                 ON CONFLICT (item_id, media_file_id) DO NOTHING",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0047CreateYouTubeImportMediaLinks;

    impl migrations::Migration for M0047CreateYouTubeImportMediaLinks {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0047_create_youtube_import_media_links";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] = &[
            migrations::MigrationDependency::migration(
                "furumusic",
                "m_0046_create_local_upload_history",
            ),
            migrations::MigrationDependency::migration(
                "furumusic",
                "m_0027_create_processing_stats",
            ),
        ];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_youtube_import_media_links).build()];
    }

    pub const MIGRATIONS: &[&SyncDynMigration] = &[
        &M0006CreateMediaFile,
        &M0007CreateArtist,
        &M0008CreateRelease,
        &M0009CreateReleaseArtist,
        &M0010CreateTrack,
        &M0011CreateTrackArtist,
        &M0012CreateGenreTables,
        &M0013CreateUserLikedTrack,
        &M0014CreateUserFollowedArtist,
        &M0015CreatePlaylistTables,
        &M0016CreatePlayHistory,
        &M0017CreatePlaybackState,
        &M0018CreateProcessingTask,
        &M0019CreateMusicIndexes,
        &M0020EnablePgTrgm,
        &M0021CreateTrgmIndexes,
        &M0022CreateTrackTrgmIndex,
        &M0028AddModelNameColumns,
        &M0029AddPlaybackVolume,
        &M0030AddMediaFileUploader,
        &M0031CreateTorrentSession,
        &M0032CreateLastfmTrackPopularity,
        &M0033CreateLastfmScrobbling,
        &M0034CreateArtworkLookupState,
        &M0035CreateEntityGenreTags,
        &M0036CreateExternalMetadataIds,
        &M0037CreatePlaylistShareLinks,
        &M0038CreateFedDeviceSync,
        &M0039EnsureFederationContentIdCache,
        &M0040CreateContentAddressedMusicRefs,
        &M0041CreateSyncedListenHistory,
        &M0042RepairLegacyListenQualification,
        &M0043CreateSimilarityEmbeddings,
        &M0044AddSimilarityRoutingSignature,
        &M0045CreateYouTubeDownloads,
        &M0046CreateLocalUploadHistory,
        &M0047CreateYouTubeImportMediaLinks,
    ];
}
