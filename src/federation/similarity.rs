//! Furumusic policy and PostgreSQL adapter for the shared similarity protocol.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::stream::{self, StreamExt as _};
use music_dht::similarity::{self as wire, SimilarityHit, SimilarityRequest, SimilarityResponse};
use music_dht::similarity_dht::SimilarityDht;
use music_dht::{
    ByteStream, EndpointId, ItemId, ItemKind, MusicDhtService, PeerTicket, StreamAcceptor,
};

use crate::similarity::{Manager, QueryVector};

use super::TransportStats;

pub use music_dht::similarity::SIMILARITY_ALPN;

const INITIAL_QUERY_PEERS: usize = 16;
const MAX_QUERY_PEERS: usize = 48;
const QUERY_CONCURRENCY: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const ROUTING_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PER_ARTIST: usize = 3;
const MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE: u32 = 8;

#[derive(Debug, Clone)]
pub struct RemoteSimilarityTrack {
    pub owner: String,
    pub item_id: String,
    pub title: String,
    pub artist_names: Vec<String>,
    pub featured_artist_names: Vec<String>,
    pub year: Option<i32>,
    pub duration_seconds: Option<i64>,
    pub content_id: Option<String>,
    pub release_title: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub similarity_score: f32,
}

pub struct SimilaritySearchOutcome {
    pub tracks: Vec<RemoteSimilarityTrack>,
    pub queried_peers: usize,
}

pub async fn serve_peers(
    mut acceptor: StreamAcceptor,
    manager: Arc<Manager>,
    own: EndpointId,
    transport: Arc<TransportStats>,
) {
    while let Some(stream) = acceptor.accept().await {
        let manager = Arc::clone(&manager);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            let peer = stream.peer_id;
            if let Err(error) = serve_one(stream, manager, own, transport).await {
                tracing::warn!(peer = %peer, "similarity request failed: {error:#}");
            }
        });
    }
}

async fn serve_one(
    mut stream: ByteStream,
    manager: Arc<Manager>,
    own: EndpointId,
    transport: Arc<TransportStats>,
) -> Result<()> {
    super::record_stream_transport(&transport, "similarity", "inbound", "open", &stream);
    let request = wire::read_request(&mut stream).await?;
    let response = if !manager.enabled() {
        SimilarityResponse::refused("similarity search is disabled on this instance")?
    } else {
        let profile_id = request.profile_id;
        let vector = request.vector;
        let limit = request.limit;
        let rank_manager = Arc::clone(&manager);
        let ranked = tokio::task::spawn_blocking(move || {
            rank_manager.rank_vector(&profile_id, &vector, None, None, limit)
        })
        .await
        .context("local similarity task failed")
        .and_then(|result| result);
        match ranked {
            Ok(ranked) => {
                let ids = ranked
                    .iter()
                    .map(|track| track.track_id)
                    .collect::<Vec<_>>();
                match manager.metadata_for_tracks(&ids).await {
                    Ok(metadata) => {
                        let by_id = ranked
                            .into_iter()
                            .map(|track| (track.track_id, track))
                            .collect::<HashMap<_, _>>();
                        let hits = metadata
                            .into_iter()
                            .filter_map(|track| {
                                let ranked = by_id.get(&track.track_id)?;
                                let hit = SimilarityHit {
                                    score: ranked.score,
                                    item_id: hex(
                                        ItemId::derive(
                                            &own,
                                            ItemKind::Track,
                                            &format!("track:{}", track.track_id),
                                        )
                                        .as_bytes(),
                                    ),
                                    title: track.title,
                                    artist_names: track.artist_names,
                                    featured_artist_names: track.featured_artist_names,
                                    year: track.year,
                                    duration_seconds: Some(track.duration_seconds.round() as i64),
                                    content_id: track.content_id,
                                    release_title: Some(track.release_title),
                                    track_number: track.track_number,
                                    disc_number: track.disc_number,
                                    embedding_signature: Some(ranked.embedding_signature),
                                };
                                match hit.validate() {
                                    Ok(()) => Some(hit),
                                    Err(error) => {
                                        tracing::debug!(%error, "invalid local similarity metadata skipped");
                                        None
                                    }
                                }
                            })
                            .collect();
                        SimilarityResponse::success(hits)?
                    }
                    Err(error) => SimilarityResponse::refused(format!(
                        "similarity metadata is unavailable: {error:#}"
                    ))?,
                }
            }
            Err(error) => {
                SimilarityResponse::refused(format!("similarity query is unavailable: {error:#}"))?
            }
        }
    };
    wire::write_response(&mut stream, &response).await?;
    stream.send.finish()?;
    let _ = stream.send.stopped().await;
    super::record_stream_transport(&transport, "similarity", "inbound", "done", &stream);
    Ok(())
}

pub async fn search(
    service: Arc<MusicDhtService>,
    routing: Arc<SimilarityDht>,
    query: QueryVector,
    limit: usize,
    transport: Arc<TransportStats>,
) -> Result<SimilaritySearchOutcome> {
    let own = service.endpoint_id();
    let routed = match tokio::time::timeout(
        ROUTING_TIMEOUT,
        routing.find_peers(&query.profile_id, &query.vector, MAX_QUERY_PEERS),
    )
    .await
    {
        Ok(Ok(peers)) => peers,
        Err(_) => {
            tracing::debug!("similarity DHT lookup timed out; using known peers");
            Vec::new()
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "similarity DHT lookup unavailable; using known peers");
            Vec::new()
        }
    };
    let mut seen = HashSet::new();
    let mut peers: Vec<QueryPeer> = routed
        .into_iter()
        .filter_map(|ticket| {
            let owner = ticket.endpoint_id();
            (owner != own && seen.insert(owner)).then_some(QueryPeer {
                owner,
                ticket: Some(ticket),
            })
        })
        .collect();
    for peer in service
        .connected_peers()
        .into_iter()
        .chain(service.known_peers().into_iter().map(|peer| peer.peer_id))
    {
        if peer != own && seen.insert(peer) {
            peers.push(QueryPeer {
                owner: peer,
                ticket: None,
            });
        }
        if peers.len() >= MAX_QUERY_PEERS {
            break;
        }
    }
    let query_signature = wire::embedding_signature(&query.vector)?;
    let request = Arc::new(SimilarityRequest::new(
        query.profile_id,
        query.vector,
        limit.clamp(1, wire::MAX_SIMILARITY_RESULTS),
    )?);

    let mut hits = Vec::new();
    let initial = peers.len().min(INITIAL_QUERY_PEERS);
    let mut queried_peers = initial;
    let responses = query_peers(
        Arc::clone(&service),
        &peers[..initial],
        Arc::clone(&request),
        Arc::clone(&transport),
    )
    .await;
    let mut successful = 0usize;
    for response in responses {
        match response {
            Ok(peer_hits) => {
                successful += 1;
                hits.extend(peer_hits);
            }
            Err(error) => tracing::debug!(%error, "similarity peer query skipped"),
        }
    }
    if initial < peers.len() && (hits.len() < limit || successful < initial.min(4)) {
        queried_peers += peers.len() - initial;
        for response in query_peers(
            Arc::clone(&service),
            &peers[initial..],
            Arc::clone(&request),
            Arc::clone(&transport),
        )
        .await
        {
            match response {
                Ok(peer_hits) => hits.extend(peer_hits),
                Err(error) => tracing::debug!(%error, "fallback similarity peer query skipped"),
            }
        }
    }
    hits.sort_by(|left, right| right.1.total_cmp(&left.1));
    let mut dedup = HashSet::new();
    let mut signatures = vec![query_signature];
    let mut artist_counts: HashMap<String, usize> = HashMap::new();
    let mut tracks = Vec::new();
    for (track, _, signature) in hits {
        if query
            .source_content_id
            .as_deref()
            .is_some_and(|source| track.content_id.as_deref() == Some(source))
        {
            continue;
        }
        let key = track
            .content_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", track.owner, track.item_id));
        if !dedup.insert(key) {
            continue;
        }
        if signature.is_some_and(|candidate| {
            signatures.iter().any(|existing| {
                wire::signature_distance(&candidate, existing)
                    <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE
            })
        }) {
            continue;
        }
        let artist = track
            .artist_names
            .first()
            .map(|name| music_dht::normalize_name(name))
            .unwrap_or_default();
        let count = artist_counts.entry(artist.clone()).or_default();
        if !artist.is_empty() && *count >= MAX_PER_ARTIST {
            continue;
        }
        *count += 1;
        if let Some(signature) = signature {
            signatures.push(signature);
        }
        tracks.push(track);
        if tracks.len() >= limit.min(wire::MAX_SIMILARITY_RESULTS) {
            break;
        }
    }
    Ok(SimilaritySearchOutcome {
        tracks,
        queried_peers,
    })
}

type PeerHits = Vec<(
    RemoteSimilarityTrack,
    f32,
    Option<[u8; wire::SIMILARITY_SIGNATURE_BYTES]>,
)>;

#[derive(Clone)]
struct QueryPeer {
    owner: EndpointId,
    ticket: Option<PeerTicket>,
}

async fn query_peers(
    service: Arc<MusicDhtService>,
    peers: &[QueryPeer],
    request: Arc<SimilarityRequest>,
    transport: Arc<TransportStats>,
) -> Vec<Result<PeerHits>> {
    stream::iter(peers.iter().cloned().map(|peer| {
        let service = Arc::clone(&service);
        let request = Arc::clone(&request);
        let transport = Arc::clone(&transport);
        async move {
            tokio::time::timeout(
                QUERY_TIMEOUT,
                query_peer(service, peer, &request, transport),
            )
            .await
            .map_err(|_| anyhow::anyhow!("similarity peer timed out"))?
        }
    }))
    .buffer_unordered(QUERY_CONCURRENCY)
    .collect()
    .await
}

async fn query_peer(
    service: Arc<MusicDhtService>,
    peer: QueryPeer,
    request: &SimilarityRequest,
    transport: Arc<TransportStats>,
) -> Result<PeerHits> {
    let owner = peer.owner;
    let mut stream = match peer.ticket {
        Some(ticket) => service.open_stream_to(&ticket, SIMILARITY_ALPN).await,
        None => service.open_stream(owner, SIMILARITY_ALPN).await,
    }
    .map_err(|error| anyhow::anyhow!("cannot reach similarity peer: {error}"))?;
    super::record_stream_transport(&transport, "similarity", "outbound", "open", &stream);
    let response = wire::exchange(&mut stream, request).await?;
    super::record_stream_transport(&transport, "similarity", "outbound", "done", &stream);
    anyhow::ensure!(
        response.ok,
        "peer refused similarity query: {}",
        response.error.unwrap_or_default()
    );
    Ok(response
        .hits
        .into_iter()
        .map(|hit| {
            let score = hit.score;
            let signature = hit.embedding_signature;
            (
                RemoteSimilarityTrack {
                    owner: owner.to_string(),
                    item_id: hit.item_id,
                    title: hit.title,
                    artist_names: hit.artist_names,
                    featured_artist_names: hit.featured_artist_names,
                    year: hit.year,
                    duration_seconds: hit.duration_seconds,
                    content_id: hit.content_id,
                    release_title: hit.release_title,
                    track_number: hit.track_number,
                    disc_number: hit.disc_number,
                    similarity_score: score,
                },
                score,
                signature,
            )
        })
        .collect())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_threshold_keeps_exact_and_near_duplicates_out() {
        let query = [0u8; wire::SIMILARITY_SIGNATURE_BYTES];
        let mut near = query;
        near[0] = 0b0000_0111;
        assert!(wire::signature_distance(&query, &near) <= MAX_NEAR_DUPLICATE_SIGNATURE_DISTANCE);
    }
}
