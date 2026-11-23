use std::collections::{HashMap, HashSet};

use anyhow::Result;
use music_dht::{ItemKind, LibraryItem, normalize_content_id, normalize_name};
use serde::Deserialize;
use sqlx::{FromRow, PgPool, types::Json};

use super::{
    ArtistKeyDto, ArtistRefDto, LocalAvailabilityDto, ReleaseKeyDto, ReleaseRefDto, TrackDto,
};
use crate::player::cover_variant_url;

impl ReleaseKeyDto {
    pub(super) fn new(
        title: &str,
        artists: &[String],
        year: Option<i32>,
        kind: Option<&str>,
    ) -> Self {
        let mut primary_artists: Vec<_> = artists
            .iter()
            .map(|a| normalize_name(a))
            .filter(|a| !a.is_empty())
            .collect();
        primary_artists.sort();
        primary_artists.dedup();
        Self {
            normalized_title: normalize_name(title),
            primary_artists,
            year,
            release_type: kind.map(normalize_name).filter(|s| !s.is_empty()),
        }
    }

    pub fn from_url(json: &str) -> Option<Self> {
        if json.len() > 8192 {
            return None;
        }
        let input: Self = serde_json::from_str(json).ok()?;
        if input.primary_artists.len() > 32 {
            return None;
        }
        let key = Self::new(
            &input.normalized_title,
            &input.primary_artists,
            input.year,
            input.release_type.as_deref(),
        );
        (!key.normalized_title.is_empty()).then_some(key)
    }
}

#[derive(FromRow)]
pub(super) struct Artist {
    pub id: i64,
    pub name: String,
    pub image_file_id: Option<i64>,
}

#[derive(FromRow)]
pub(super) struct Release {
    pub id: i64,
    pub title: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub cover_file_id: Option<i64>,
    pub artists: Vec<String>,
    has_hidden_credit: bool,
}

impl Release {
    pub(super) fn key(&self) -> ReleaseKeyDto {
        ReleaseKeyDto::new(
            &self.title,
            &self.artists,
            self.year,
            Some(&self.release_type),
        )
    }
}

#[derive(Deserialize)]
struct Credit {
    id: i64,
    name: String,
    role: String,
}

#[derive(FromRow)]
struct Track {
    content_id: String,
    id: i64,
    title: String,
    year: Option<i32>,
    release_id: i64,
    duration_seconds: f64,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    cover_file_id: Option<i64>,
    credits: Json<Vec<Credit>>,
}

#[derive(Default)]
pub(super) struct LocalCatalog {
    artists: HashMap<String, Vec<Artist>>,
    releases: HashMap<i64, Release>,
    // Year/type may be unknown on older peers. Keep every candidate until
    // both optional constraints have been applied and ambiguity ruled out.
    editions: HashMap<(String, Vec<String>), Vec<i64>>,
    tracks: HashMap<String, Track>,
}

impl LocalCatalog {
    pub(super) async fn for_items(pool: &PgPool, items: &[LibraryItem]) -> Result<Self> {
        let mut artists = HashSet::new();
        let mut album_artists = HashSet::new();
        let mut ids = HashSet::new();
        for item in items {
            if item.kind == ItemKind::Artist {
                artists.insert(normalize_name(&item.name));
            }
            artists.extend(
                item.artist_names
                    .iter()
                    .chain(&item.featured_artist_names)
                    .map(|s| normalize_name(s)),
            );
            if item.kind == ItemKind::Release || item.release_title.is_some() {
                album_artists.extend(item.artist_names.iter().map(|s| normalize_name(s)));
            }
            if item.kind == ItemKind::Track
                && let Some(id) = item.content_id.as_deref().and_then(normalize_content_id)
            {
                ids.insert(id);
            }
        }
        Self::load(pool, artists, &album_artists, ids.into_iter().collect()).await
    }

    async fn load(
        pool: &PgPool,
        artists: HashSet<String>,
        album_artists: &HashSet<String>,
        ids: Vec<String>,
    ) -> Result<Self> {
        let mut catalog = Self::default();
        if !artists.is_empty() {
            // Old name_sort values preserve punctuation, newer imports can store
            // the protocol spelling. Use a literal token to narrow candidates,
            // then apply the exact same Unicode normalization as the DHT in Rust.
            let patterns: Vec<_> = artists
                .iter()
                .filter_map(|key| key.split_whitespace().max_by_key(|w| w.len()))
                .map(|word| format!("%{word}%"))
                .collect();
            let rows = sqlx::query_as::<_, Artist>(
                "SELECT id, name::text, image_file_id FROM furumusic__artist
                 WHERE NOT is_hidden AND (name_sort LIKE ANY($1) OR name IS NOT NFKC NORMALIZED)",
            )
            .bind(patterns)
            .fetch_all(pool)
            .await?;
            for artist in rows {
                let key = normalize_name(&artist.name);
                if artists.contains(&key) {
                    catalog.artists.entry(key).or_default().push(artist);
                }
            }
        }
        if !ids.is_empty() {
            let tracks = sqlx::query_as::<_, Track>(
                "SELECT DISTINCT ON (c.content_id) c.content_id, t.id, t.title::text, t.release_id,
                        t.year, t.track_number, t.disc_number, t.cover_file_id, t.duration_seconds,
                        COALESCE((SELECT jsonb_agg(jsonb_build_object('id',a.id,'name',a.name,'role',ta.role)
                                       ORDER BY ta.position,a.id)
                                  FROM furumusic__track_artist ta JOIN furumusic__artist a ON a.id=ta.artist_id
                                  WHERE ta.track_id=t.id AND NOT a.is_hidden AND ta.role IN ('main','featuring')),
                                 '[]'::jsonb) AS credits
                 FROM furumusic__federation_content_id_cache c
                 JOIN furumusic__media_file m ON m.id=c.media_file_id AND m.sha256_hash=c.sha256_hash
                 JOIN furumusic__track t ON t.audio_file_id=m.id
                 JOIN furumusic__release r ON r.id=t.release_id
                 WHERE c.content_id=ANY($1) AND NOT t.is_hidden AND NOT r.is_hidden
                 ORDER BY c.content_id,t.id")
                .bind(ids).fetch_all(pool).await?;
            for track in tracks {
                catalog.tracks.insert(track.content_id.clone(), track);
            }
        }
        let artist_ids: Vec<_> = album_artists
            .iter()
            .filter_map(|a| catalog.artists.get(a))
            .flatten()
            .map(|a| a.id)
            .collect();
        let release_ids: Vec<_> = catalog.tracks.values().map(|t| t.release_id).collect();
        if !artist_ids.is_empty() || !release_ids.is_empty() {
            let rows = sqlx::query_as::<_, Release>(
                "WITH candidates AS (
                     SELECT release_id AS id FROM furumusic__release_artist WHERE artist_id=ANY($1)
                     UNION SELECT unnest($2::bigint[]))
                 SELECT r.id, r.title::text, r.release_type::text, r.year, r.cover_file_id,
                        ARRAY(SELECT a.name::text FROM furumusic__release_artist ra
                              JOIN furumusic__artist a ON a.id=ra.artist_id
                              WHERE ra.release_id=r.id AND NOT a.is_hidden ORDER BY ra.position,a.id) AS artists,
                        EXISTS(SELECT 1 FROM furumusic__release_artist ra
                               JOIN furumusic__artist a ON a.id=ra.artist_id
                               WHERE ra.release_id=r.id AND a.is_hidden) AS has_hidden_credit
                 FROM candidates JOIN furumusic__release r USING(id) WHERE NOT r.is_hidden")
                .bind(artist_ids).bind(release_ids).fetch_all(pool).await?;
            for release in rows {
                let key = release.key();
                if !release.has_hidden_credit {
                    catalog
                        .editions
                        .entry((key.normalized_title, key.primary_artists))
                        .or_default()
                        .push(release.id);
                }
                catalog.releases.insert(release.id, release);
            }
        }
        Ok(catalog)
    }

    pub(super) fn artist(&self, key: &str) -> Option<&Artist> {
        let [only] = self.artists.get(key)?.as_slice() else {
            return None;
        };
        Some(only)
    }

    pub(super) fn release(&self, key: &ReleaseKeyDto) -> Option<&Release> {
        if key.primary_artists.is_empty() {
            return None;
        }
        let ids = self
            .editions
            .get(&(key.normalized_title.clone(), key.primary_artists.clone()))?;
        let mut matches = ids.iter().map(|id| &self.releases[id]).filter(|release| {
            key.year.is_none_or(|year| release.year == Some(year))
                && key
                    .release_type
                    .as_ref()
                    .is_none_or(|kind| *kind == normalize_name(&release.release_type))
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub(super) fn enrich(&self, dto: &mut TrackDto) {
        let m = &mut dto.metadata;
        for artist in m.artists.iter_mut().chain(&mut m.featured_artists) {
            if let Some(local) = self.artist(&artist.key.normalized_name) {
                artist.name = local.name.clone();
                artist.local_id = Some(local.id);
            }
        }
        let local = self.tracks.get(&dto.key.content_id);
        let release = local
            .and_then(|t| self.releases.get(&t.release_id))
            .or_else(|| m.release.as_ref().and_then(|r| self.release(&r.key)));
        let release_cover = release.and_then(|r| r.cover_file_id);
        if let Some(r) = release {
            m.release = Some(ReleaseRefDto {
                key: r.key(),
                title: r.title.clone(),
                local_id: Some(r.id),
            });
        }
        if let Some(track) = local {
            dto.availability.local = Some(LocalAvailabilityDto {
                track_id: track.id,
                stream_url: format!("/api/player/stream/{}", track.id),
            });
            dto.availability.state = "local";
            m.title = track.title.clone();
            m.year = track
                .year
                .or_else(|| release.and_then(|r| r.year))
                .or(m.year);
            m.track_number = track.track_number.or(m.track_number);
            m.disc_number = track.disc_number.or(m.disc_number);
            if track.duration_seconds > 0. {
                m.duration_seconds = Some(track.duration_seconds);
            }
            if !track.credits.is_empty() {
                m.artists.clear();
                m.featured_artists.clear();
                for credit in &track.credits.0 {
                    let credits = if credit.role == "featuring" {
                        &mut m.featured_artists
                    } else {
                        &mut m.artists
                    };
                    credits.push(ArtistRefDto {
                        key: ArtistKeyDto {
                            normalized_name: normalize_name(&credit.name),
                        },
                        name: credit.name.clone(),
                        local_id: Some(credit.id),
                    });
                }
            }
        }
        if let Some(cover) = cover_variant_url(
            local.and_then(|t| t.cover_file_id).or(release_cover),
            "medium",
        ) {
            m.cover_url = Some(cover);
        }
    }
}

impl super::Federation {
    pub async fn local_release_id(&self, key: &ReleaseKeyDto) -> Result<Option<i64>> {
        let artists = key.primary_artists.iter().cloned().collect::<HashSet<_>>();
        let catalog =
            LocalCatalog::load(&self.pool().await?, artists.clone(), &artists, Vec::new()).await?;
        Ok(catalog.release(key).map(|r| r.id))
    }

    pub async fn local_artist_id(&self, name: &str) -> Result<Option<i64>> {
        let key = normalize_name(name);
        let catalog = LocalCatalog::load(
            &self.pool().await?,
            HashSet::from([key.clone()]),
            &HashSet::new(),
            Vec::new(),
        )
        .await?;
        Ok(catalog.artist(&key).map(|a| a.id))
    }
}
