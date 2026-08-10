//! Receiving side of music federation.
//!
//! User-facing identity is content-addressed. An `(owner, item_id)` pair is
//! only a source locator and several locators may resolve the same track.

use std::collections::HashMap;

use anyhow::{Context, Result};
use music_dht::{ItemKind, LibraryItem, normalize_content_id};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row as _;
use tokio::io::AsyncReadExt;

use super::{Federation, now_iso};

const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct TrackKeyDto {
    pub content_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtistKeyDto {
    pub normalized_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtistRefDto {
    pub key: ArtistKeyDto,
    pub name: String,
    pub local_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseKeyDto {
    pub normalized_title: String,
    pub primary_artists: Vec<String>,
    pub release_type: Option<String>,
    pub year: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseRefDto {
    pub key: ReleaseKeyDto,
    pub local_id: Option<i64>,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FederationSourceDto {
    pub owner: String,
    pub item_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalAvailabilityDto {
    pub track_id: i64,
    pub stream_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackMetadataDto {
    pub title: String,
    pub artists: Vec<ArtistRefDto>,
    pub featured_artists: Vec<ArtistRefDto>,
    pub release: Option<ReleaseRefDto>,
    pub year: Option<i32>,
    pub duration_seconds: Option<f64>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackAvailabilityDto {
    pub state: &'static str,
    pub local: Option<LocalAvailabilityDto>,
    pub federation: Vec<FederationSourceDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackDto {
    pub key: TrackKeyDto,
    pub metadata: TrackMetadataDto,
    pub availability: TrackAvailabilityDto,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchEvent {
    pub search_id: String,
    pub sequence: u64,
    pub kind: &'static str,
    pub peer: Option<String>,
    pub entity_key: Value,
    pub entity: Value,
}

impl Federation {
    pub async fn prepare_similarity_tracks(
        &self,
        tracks: Vec<super::similarity::RemoteSimilarityTrack>,
    ) -> Result<Vec<TrackDto>> {
        let pool = self.pool().await?;
        let mut prepared = Vec::new();
        for track in tracks {
            let Some(content_id) = track.content_id.as_deref().and_then(normalize_content_id)
            else {
                continue;
            };
            let local = local_availability(&pool, &content_id).await?;
            // A local result is already present in the first result section.
            if local.is_some() {
                continue;
            }
            let owner = track.owner;
            let item_id = track.item_id;
            let dto = TrackDto {
                key: TrackKeyDto {
                    content_id: content_id.clone(),
                },
                metadata: TrackMetadataDto {
                    title: track.title,
                    artists: artist_refs(&track.artist_names),
                    featured_artists: artist_refs(&track.featured_artist_names),
                    release: track.release_title.map(|title| ReleaseRefDto {
                        key: ReleaseKeyDto {
                            normalized_title: music_dht::normalize_name(&title),
                            primary_artists: track
                                .artist_names
                                .iter()
                                .map(|artist| music_dht::normalize_name(artist))
                                .collect(),
                            release_type: None,
                            year: track.year,
                        },
                        local_id: None,
                        title,
                    }),
                    year: track.year,
                    duration_seconds: track.duration_seconds.map(|value| value as f64),
                    track_number: track.track_number,
                    disc_number: track.disc_number,
                    cover_url: Some(format!(
                        "/api/player/federation/tracks/artwork?owner={owner}&item_id={item_id}"
                    )),
                },
                availability: TrackAvailabilityDto {
                    state: "federated",
                    local: None,
                    federation: vec![FederationSourceDto { owner, item_id }],
                },
            };
            persist_track_ref(&pool, &dto).await?;
            prepared.push(dto);
        }
        Ok(prepared)
    }

    pub fn stream_artist_catalogs(
        self: &std::sync::Arc<Self>,
        name: String,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Result<(String, music_dht::catalog::CatalogArtist)>>
    {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let federation = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let result = async {
                let service = federation.service().await?;
                let normalized = music_dht::normalize_name(&name);
                let outcome = service
                    .search_network(&name)
                    .await
                    .map_err(|err| anyhow::anyhow!("federated artist search failed: {err}"))?;
                let owners: std::collections::HashSet<_> = outcome
                    .network_results
                    .iter()
                    .filter(|item| {
                        (item.kind == ItemKind::Artist && item.normalized_name == normalized)
                            || item
                                .artist_names
                                .iter()
                                .chain(&item.featured_artist_names)
                                .any(|artist| music_dht::normalize_name(artist) == normalized)
                    })
                    .map(|item| item.owner)
                    .collect();
                for owner in owners {
                    let service = std::sync::Arc::clone(&service);
                    let sender = sender.clone();
                    let name = name.clone();
                    tokio::spawn(async move {
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            fetch_artist_catalog(&service, owner, &name),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("catalog request timed out"))
                        .and_then(|result| result)
                        .map(|artist| (owner.to_string(), artist));
                        let _ = sender.send(result);
                    });
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(err) = result {
                let _ = sender.send(Err(err));
            }
        });
        receiver
    }

    /// Performs one bounded DHT search and returns entity upserts. The HTTP
    /// layer streams each upsert independently; catalog fan-out can append
    /// events to the same contract without changing the browser model.
    pub async fn search_events(&self, search_id: &str, query: &str) -> Result<Vec<SearchEvent>> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "search query is empty");
        anyhow::ensure!(query.chars().count() <= 200, "search query is too long");

        let started = std::time::Instant::now();
        let service = self.service().await?;
        tracing::info!(
            search_id,
            query,
            connected_peers = service.connected_peers().len(),
            known_contacts = service.known_peers().len(),
            "federated search started"
        );
        let own = service.endpoint_id();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            service.search_network(query),
        )
        .await
        .map_err(|_| anyhow::anyhow!("federated search timed out after 20 seconds"))?
        .map_err(|err| anyhow::anyhow!("federated search failed: {err}"))?;
        tracing::info!(
            search_id,
            query,
            local_results = result.local_results.len(),
            network_results = result.network_results.len(),
            queried_nodes = result.queried_nodes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "federated DHT search finished"
        );
        let all_items: Vec<LibraryItem> = result
            .local_results
            .into_iter()
            .chain(result.network_results)
            .collect();

        let pool = self.pool().await?;
        let mut by_content: HashMap<String, TrackDto> = HashMap::new();
        for item in all_items.iter().filter(|item| item.kind == ItemKind::Track) {
            let Some(content_id) = item.content_id.as_deref().and_then(normalize_content_id) else {
                // A globally usable track reference must be verifiable.
                continue;
            };
            let local = local_availability(&pool, &content_id).await?;
            let source = FederationSourceDto {
                owner: item.owner.to_string(),
                item_id: hex(item.id.as_bytes()),
            };
            let entry = by_content.entry(content_id.clone()).or_insert_with(|| {
                track_from_item(content_id.clone(), item, local, item.owner == own)
            });
            if !entry.availability.federation.iter().any(|candidate| {
                candidate.owner == source.owner && candidate.item_id == source.item_id
            }) {
                entry.availability.federation.push(source);
            }
            if entry.availability.local.is_some() {
                entry.availability.state = "local";
            }
        }

        let mut tracks: Vec<_> = by_content.into_values().collect();
        tracks.sort_by(|left, right| {
            left.metadata
                .title
                .to_lowercase()
                .cmp(&right.metadata.title.to_lowercase())
        });

        let mut events = Vec::with_capacity(all_items.len());
        for (index, track) in tracks.into_iter().enumerate() {
            persist_track_ref(&pool, &track).await?;
            let peer = track
                .availability
                .federation
                .first()
                .map(|source| source.owner.clone());
            events.push(SearchEvent {
                search_id: search_id.to_owned(),
                sequence: index as u64 + 1,
                kind: "federation.track",
                peer,
                entity_key: serde_json::to_value(&track.key)?,
                entity: serde_json::to_value(track)?,
            });
        }
        let mut artist_peers: HashMap<String, (String, Vec<String>)> = HashMap::new();
        let mut releases: HashMap<String, Value> = HashMap::new();
        for item in &all_items {
            match item.kind {
                ItemKind::Artist => {
                    let key = music_dht::normalize_name(&item.name);
                    let entry = artist_peers
                        .entry(key)
                        .or_insert_with(|| (item.name.clone(), Vec::new()));
                    let owner = item.owner.to_string();
                    if !entry.1.contains(&owner) {
                        entry.1.push(owner);
                    }
                }
                ItemKind::Release => {
                    let artist_keys: Vec<String> = item
                        .artist_names
                        .iter()
                        .map(|name| music_dht::normalize_name(name))
                        .collect();
                    let normalized_title = music_dht::normalize_name(&item.name);
                    let cover_url = all_items
                        .iter()
                        .find(|track| {
                            track.kind == ItemKind::Track
                                && track.release_title.as_deref().is_some_and(|title| {
                                    music_dht::normalize_name(title) == normalized_title
                                })
                                && track.year == item.year
                        })
                        .map(|track| {
                            format!(
                                "/api/player/federation/tracks/artwork?owner={}&item_id={}",
                                track.owner,
                                hex(track.id.as_bytes())
                            )
                        });
                    let key = format!(
                        "{}|{}|{}",
                        normalized_title,
                        artist_keys.join(","),
                        item.year.map_or_else(String::new, |year| year.to_string())
                    );
                    releases.entry(key.clone()).or_insert_with(|| {
                        json!({
                            "key": {
                                "normalized_title": music_dht::normalize_name(&item.name),
                                "primary_artists": artist_keys,
                                "release_type": null,
                                "year": item.year,
                            },
                            "title": item.name,
                            "artists": item.artist_names,
                            "year": item.year,
                            "cover_url": cover_url,
                            "sources": [{
                                "owner": item.owner.to_string(),
                                "item_id": hex(item.id.as_bytes()),
                            }],
                        })
                    });
                }
                ItemKind::Track => {}
            }
        }
        for (key, (name, peers)) in artist_peers {
            let sequence = events.len() as u64 + 1;
            events.push(SearchEvent {
                search_id: search_id.to_owned(),
                sequence,
                kind: "federation.artist",
                peer: peers.first().cloned(),
                entity_key: json!({ "normalized_name": key }),
                entity: json!({
                    "key": { "normalized_name": key },
                    "name": name,
                    "image_url": null,
                    "peers": peers,
                }),
            });
        }
        for (key, release) in releases {
            let sequence = events.len() as u64 + 1;
            events.push(SearchEvent {
                search_id: search_id.to_owned(),
                sequence,
                kind: "federation.release",
                peer: None,
                entity_key: json!({ "composite": key }),
                entity: release,
            });
        }
        tracing::info!(
            search_id,
            query,
            events = events.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "federated search response ready"
        );
        Ok(events)
    }
}

async fn fetch_artist_catalog(
    service: &music_dht::MusicDhtService,
    owner: music_dht::EndpointId,
    artist: &str,
) -> Result<music_dht::catalog::CatalogArtist> {
    let mut stream = service
        .open_stream(owner, super::CATALOG_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("cannot reach catalog peer: {err}"))?;
    let mut request = serde_json::to_vec(&music_dht::catalog::CatalogRequest {
        artist: artist.to_owned(),
        want: Some("catalog".to_owned()),
        ..Default::default()
    })?;
    request.push(b'\n');
    stream.send.write_all(&request).await?;
    stream.send.finish()?;
    let mut payload = Vec::new();
    stream
        .recv
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut payload)
        .await?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_CATALOG_BYTES,
        "catalog response is too large"
    );
    let response: music_dht::catalog::CatalogResponse =
        serde_json::from_slice(&payload).context("invalid catalog response")?;
    anyhow::ensure!(
        response.ok,
        "peer refused catalog: {}",
        response.error.unwrap_or_else(|| "unknown error".to_owned())
    );
    response.artist.context("peer returned no artist catalog")
}

fn track_from_item(
    content_id: String,
    item: &LibraryItem,
    local: Option<LocalAvailabilityDto>,
    own: bool,
) -> TrackDto {
    let owner = item.owner.to_string();
    let item_id = hex(item.id.as_bytes());
    let artists = artist_refs(&item.artist_names);
    let featured_artists = artist_refs(&item.featured_artist_names);
    let release = item.release_title.as_ref().map(|title| ReleaseRefDto {
        key: ReleaseKeyDto {
            normalized_title: music_dht::normalize_name(title),
            primary_artists: item
                .artist_names
                .iter()
                .map(|artist| music_dht::normalize_name(artist))
                .collect(),
            release_type: None,
            year: item.year,
        },
        local_id: None,
        title: title.clone(),
    });
    let state = if local.is_some() || own {
        "local"
    } else {
        "federated"
    };
    TrackDto {
        key: TrackKeyDto { content_id },
        metadata: TrackMetadataDto {
            title: item.name.clone(),
            artists,
            featured_artists,
            release,
            year: item.year,
            duration_seconds: item.duration_seconds,
            track_number: item.track_number,
            disc_number: item.disc_number,
            cover_url: Some(format!(
                "/api/player/federation/tracks/artwork?owner={owner}&item_id={item_id}"
            )),
        },
        availability: TrackAvailabilityDto {
            state,
            local,
            federation: vec![FederationSourceDto { owner, item_id }],
        },
    }
}

fn artist_refs(names: &[String]) -> Vec<ArtistRefDto> {
    names
        .iter()
        .map(|name| ArtistRefDto {
            key: ArtistKeyDto {
                normalized_name: music_dht::normalize_name(name),
            },
            name: name.clone(),
            local_id: None,
        })
        .collect()
}

async fn local_availability(
    pool: &sqlx::PgPool,
    content_id: &str,
) -> Result<Option<LocalAvailabilityDto>> {
    let row = sqlx::query(
        "SELECT t.id
           FROM furumusic__federation_content_id_cache c
           JOIN furumusic__track t ON t.audio_file_id = c.media_file_id
          WHERE c.content_id = $1 AND t.is_hidden = false
          LIMIT 1",
    )
    .bind(content_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| {
        let track_id: i64 = row.get(0);
        LocalAvailabilityDto {
            track_id,
            stream_url: format!("/api/player/stream/{track_id}"),
        }
    }))
}

async fn persist_track_ref(pool: &sqlx::PgPool, track: &TrackDto) -> Result<()> {
    let metadata = serde_json::to_value(&track.metadata)?;
    let local_id = track
        .availability
        .local
        .as_ref()
        .map(|local| local.track_id);
    let row = sqlx::query(
        "INSERT INTO furumusic__track_ref
            (content_id, local_track_id, title, release_title, year,
             duration_seconds, metadata_json, metadata_authority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'federation', $8, $8)
         ON CONFLICT (content_id) DO UPDATE SET
            local_track_id = COALESCE(furumusic__track_ref.local_track_id, EXCLUDED.local_track_id),
            title = EXCLUDED.title,
            release_title = EXCLUDED.release_title,
            year = EXCLUDED.year,
            duration_seconds = EXCLUDED.duration_seconds,
            metadata_json = EXCLUDED.metadata_json,
            updated_at = EXCLUDED.updated_at
         RETURNING id",
    )
    .bind(&track.key.content_id)
    .bind(local_id)
    .bind(&track.metadata.title)
    .bind(
        track
            .metadata
            .release
            .as_ref()
            .map(|release| &release.title),
    )
    .bind(track.metadata.year)
    .bind(track.metadata.duration_seconds)
    .bind(metadata)
    .bind(now_iso())
    .fetch_one(pool)
    .await
    .context("persisting content-addressed track reference failed")?;
    let track_ref_id: i64 = row.get(0);
    for source in &track.availability.federation {
        sqlx::query(
            "INSERT INTO furumusic__federation_track_source
                (track_ref_id, owner_peer_id, item_id, last_seen_ms, metadata_json)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (owner_peer_id, item_id) DO UPDATE SET
                track_ref_id = EXCLUDED.track_ref_id,
                last_seen_ms = EXCLUDED.last_seen_ms,
                metadata_json = EXCLUDED.metadata_json",
        )
        .bind(track_ref_id)
        .bind(&source.owner)
        .bind(&source.item_id)
        .bind(chrono::Utc::now().timestamp_millis())
        .bind(json!({ "track": track.metadata }))
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
