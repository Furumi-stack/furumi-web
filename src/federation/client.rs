//! Receiving side of music federation.
//!
//! User-facing identity is content-addressed. An `(owner, item_id)` pair is
//! only a source locator and several locators may resolve the same track.

use std::collections::HashMap;

use anyhow::{Context, Result};
use music_dht::{ItemKind, LibraryItem, normalize_content_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row as _;
use tokio::io::AsyncReadExt;

use super::{Federation, now_iso};
mod local_catalog;
#[cfg(test)]
mod search_tests;
use local_catalog::LocalCatalog;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity_score: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchEvent {
    pub score: u32,
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
                similarity_score: Some(track.similarity_score),
            };
            prepared.push(dto);
        }
        persist_track_refs(&pool, prepared.iter()).await?;
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
                // Deep links carry normalized names. Peers may require their
                // original spelling (for example "AC/DC") in catalog requests.
                let mut owners = HashMap::new();
                for item in &outcome.network_results {
                    if item.kind == ItemKind::Artist && item.normalized_name == normalized {
                        owners.insert(item.owner, item.name.clone());
                    } else if let Some(display) = item
                        .artist_names
                        .iter()
                        .chain(&item.featured_artist_names)
                        .find(|artist| music_dht::normalize_name(artist) == normalized)
                    {
                        owners.entry(item.owner).or_insert_with(|| display.clone());
                    }
                }
                for (owner, name) in owners {
                    let service = std::sync::Arc::clone(&service);
                    let sender = sender.clone();
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
        let events = build_search_events(&pool, search_id, query, &all_items).await?;
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

async fn build_search_events(
    pool: &sqlx::PgPool,
    search_id: &str,
    query: &str,
    all_items: &[LibraryItem],
) -> Result<Vec<SearchEvent>> {
    let catalog = LocalCatalog::for_items(pool, all_items).await?;
    let relevance = crate::search_relevance::SearchRelevance::new(query);
    let mut by_content: HashMap<String, TrackDto> = HashMap::new();
    let mut track_covers = HashMap::new();
    for item in all_items.iter().filter(|item| item.kind == ItemKind::Track) {
        let Some(content_id) = item.content_id.as_deref().and_then(normalize_content_id) else {
            // A globally usable track reference must be verifiable.
            continue;
        };
        let source = FederationSourceDto {
            owner: item.owner.to_string(),
            item_id: hex(item.id.as_bytes()),
        };
        if let Some(title) = item.release_title.as_deref() {
            let key = ReleaseKeyDto::new(
                title,
                &item.artist_names,
                item.year,
                item.release_type.as_deref(),
            );
            track_covers.entry((item.owner, key)).or_insert_with(|| {
                format!(
                    "/api/player/federation/tracks/artwork?owner={}&item_id={}",
                    source.owner, source.item_id
                )
            });
        }
        let entry = by_content.entry(content_id.clone()).or_insert_with(|| {
            let mut dto = track_from_item(content_id, item);
            catalog.enrich(&mut dto);
            dto
        });
        if !entry
            .availability
            .federation
            .iter()
            .any(|candidate| candidate.owner == source.owner && candidate.item_id == source.item_id)
        {
            entry.availability.federation.push(source);
        }
    }

    let mut tracks: Vec<_> = by_content.into_values().collect();
    tracks.sort_by_cached_key(|t| (t.metadata.title.to_lowercase(), t.key.content_id.clone()));

    let mut events = Vec::with_capacity(all_items.len());
    let mut retained = Vec::new();
    for track in tracks {
        let m = &track.metadata;
        let Some(score) = relevance.score(
            &m.title,
            m.artists
                .iter()
                .chain(&m.featured_artists)
                .map(|a| a.name.as_str()),
            m.release.as_ref().map(|r| r.title.as_str()),
        ) else {
            continue;
        };
        retained.push((score, track));
    }
    persist_track_refs(pool, retained.iter().map(|(_, track)| track)).await?;
    let local_ids: Vec<_> = retained
        .iter()
        .filter_map(|(_, track)| track.availability.local.as_ref().map(|l| l.track_id))
        .collect();
    let mut local_tracks: HashMap<_, _> = crate::player::load_track_items_by_ids(pool, &local_ids)
        .await?
        .into_iter()
        .map(|track| (track.id, track))
        .collect();
    for (score, track) in retained {
        let peer = track
            .availability
            .federation
            .first()
            .map(|source| source.owner.clone());
        let local_track = track
            .availability
            .local
            .as_ref()
            .and_then(|l| local_tracks.remove(&l.track_id));
        let mut entity = serde_json::to_value(&track)?;
        if let Some(local) = local_track {
            entity["local_track"] = serde_json::to_value(local)?;
        }
        events.push(SearchEvent {
            search_id: search_id.to_owned(),
            score,
            sequence: events.len() as u64 + 1,
            kind: "federation.track",
            peer,
            entity_key: serde_json::to_value(&track.key)?,
            entity,
        });
    }
    let mut artist_peers: HashMap<String, (String, Vec<String>)> = HashMap::new();
    let mut releases = HashMap::<ReleaseKeyDto, (Value, Vec<FederationSourceDto>, u32)>::new();
    for item in all_items {
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
                let remote_key = ReleaseKeyDto::new(
                    &item.name,
                    &item.artist_names,
                    item.year,
                    item.release_type.as_deref(),
                );
                let local = catalog.release(&remote_key);
                let title = local.map_or(item.name.as_str(), |r| &r.title);
                let artists = local.map_or(&item.artist_names, |r| &r.artists);
                let Some(score) = relevance.score(title, artists.iter().map(String::as_str), None)
                else {
                    continue;
                };
                let cover_url =
                    crate::player::cover_variant_url(local.and_then(|r| r.cover_file_id), "medium")
                        .or_else(|| track_covers.get(&(item.owner, remote_key.clone())).cloned());
                let key = local.map_or(remote_key, |release| release.key());
                let (entity, sources, _) = releases.entry(key.clone()).or_insert_with(|| {
                    (
                        json!({
                            "key": key,
                            "local_id": local.map(|r|r.id),
                            "title": title,
                            "artists": artists,
                            "year": key.year,
                            "release_type": key.release_type,
                            "cover_url": null
                        }),
                        Vec::new(),
                        score,
                    )
                });
                if entity["cover_url"].is_null() {
                    entity["cover_url"] = json!(cover_url);
                }
                let source = FederationSourceDto {
                    owner: item.owner.to_string(),
                    item_id: hex(item.id.as_bytes()),
                };
                if !sources
                    .iter()
                    .any(|s| s.owner == source.owner && s.item_id == source.item_id)
                {
                    sources.push(source);
                }
            }
            ItemKind::Track => {}
        }
    }
    for (key, (name, peers)) in artist_peers {
        let local = catalog.artist(&key);
        let name = local.map_or(name.as_str(), |a| &a.name);
        let Some(score) = relevance.score(name, std::iter::empty(), None) else {
            continue;
        };
        let sequence = events.len() as u64 + 1;
        events.push(SearchEvent {
                search_id: search_id.to_owned(),
                sequence,
                score,
                kind: "federation.artist",
                peer: peers.first().cloned(),
                entity_key: json!({ "normalized_name": key }),
                entity: json!({
                    "key": { "normalized_name": key },
                    "name": name,
                    "local_id": local.map(|a|a.id),
                    "image_url": crate::player::cover_variant_url(local.and_then(|a|a.image_file_id),"medium"),
                    "peers": peers,
                }),
            });
    }
    for (key, (mut release, sources, score)) in releases {
        release["sources"] = serde_json::to_value(sources)?;
        let sequence = events.len() as u64 + 1;
        events.push(SearchEvent {
            search_id: search_id.to_owned(),
            sequence,
            score,
            kind: "federation.release",
            peer: None,
            entity_key: json!({ "composite": serde_json::to_string(&key)? }),
            entity: release,
        });
    }
    Ok(events)
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

fn track_from_item(content_id: String, item: &LibraryItem) -> TrackDto {
    let owner = item.owner.to_string();
    let item_id = hex(item.id.as_bytes());
    let artists = artist_refs(&item.artist_names);
    let featured_artists = artist_refs(&item.featured_artist_names);
    let release = item.release_title.as_ref().map(|title| ReleaseRefDto {
        key: ReleaseKeyDto::new(
            title,
            &item.artist_names,
            item.year,
            item.release_type.as_deref(),
        ),
        local_id: None,
        title: title.clone(),
    });
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
            state: "federated",
            local: None,
            federation: vec![FederationSourceDto { owner, item_id }],
        },
        similarity_score: None,
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

async fn persist_track_refs<'a>(
    pool: &sqlx::PgPool,
    tracks: impl Iterator<Item = &'a TrackDto>,
) -> Result<()> {
    let tracks: Vec<_> = tracks.collect();
    if tracks.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let (local_ids, content_ids): (Vec<_>, Vec<_>) = tracks
        .iter()
        .filter_map(|track| {
            track
                .availability
                .local
                .as_ref()
                .map(|local| (local.track_id, track.key.content_id.as_str()))
        })
        .unzip();
    if !local_ids.is_empty() {
        // Replacing a track's audio keeps its SQL id. Its previous content
        // reference still owns its likes and sources, but no longer the local id.
        sqlx::query(
            "UPDATE furumusic__track_ref ref SET local_track_id=NULL
                     FROM unnest($1::bigint[],$2::text[]) AS live(track_id,content_id)
                     WHERE ref.local_track_id=live.track_id AND ref.content_id<>live.content_id",
        )
        .bind(local_ids)
        .bind(content_ids)
        .execute(&mut *tx)
        .await?;
    }
    // Sort conflicting keys consistently when multiple people search concurrently.
    sqlx::query(
        "WITH payload AS (
             SELECT track->'key'->>'content_id' AS content_id,
                    (track->'availability'->'local'->>'track_id')::bigint AS local_id,
                    track->'metadata' AS meta, track->'availability'->'federation' AS sources
             FROM jsonb_array_elements($1::jsonb) track
         ), refs AS (
             INSERT INTO furumusic__track_ref
                 (content_id, local_track_id, title, release_title, year, duration_seconds,
                  metadata_json, metadata_authority, created_at, updated_at)
             SELECT DISTINCT ON (content_id) content_id, local_id, meta->>'title', meta->'release'->>'title',
                    (meta->>'year')::int, (meta->>'duration_seconds')::float8, meta,
                    CASE WHEN local_id IS NULL THEN 'federation' ELSE 'local' END, $2, $2
             FROM payload ORDER BY content_id
             ON CONFLICT (content_id) DO UPDATE SET
                 local_track_id=COALESCE(EXCLUDED.local_track_id,furumusic__track_ref.local_track_id),
                 title=EXCLUDED.title, release_title=EXCLUDED.release_title, year=EXCLUDED.year,
                 duration_seconds=EXCLUDED.duration_seconds, metadata_json=EXCLUDED.metadata_json,
                 metadata_authority=EXCLUDED.metadata_authority, updated_at=EXCLUDED.updated_at
             RETURNING id,content_id
         )
         INSERT INTO furumusic__federation_track_source
              (track_ref_id,owner_peer_id,item_id,last_seen_ms,metadata_json)
         SELECT DISTINCT ON (source->>'owner',source->>'item_id') refs.id,source->>'owner',
                source->>'item_id',$3,jsonb_build_object('track',payload.meta)
         FROM payload JOIN refs USING (content_id) CROSS JOIN LATERAL jsonb_array_elements(sources) source
         ORDER BY source->>'owner',source->>'item_id',refs.id
         ON CONFLICT (owner_peer_id,item_id) DO UPDATE SET
              track_ref_id=EXCLUDED.track_ref_id,last_seen_ms=EXCLUDED.last_seen_ms,metadata_json=EXCLUDED.metadata_json")
        .bind(sqlx::types::Json(tracks)).bind(now_iso()).bind(chrono::Utc::now().timestamp_millis())
        .execute(&mut *tx).await.context("persisting federated track references failed")?;
    tx.commit().await?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
