//! Optional lyrics overlay, keyed by the exact audio content.
use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use music_dht::lyrics::{self, LyricsRequest, LyricsResponse, LyricsVersion};
use music_dht::{ByteStream, EndpointId, MusicDhtService, StreamAcceptor};
use sqlx::PgPool;
use std::collections::HashSet;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

const PEER_TIMEOUT: Duration = Duration::from_secs(5);
const LOOKUP_INTERVAL: Duration = Duration::from_secs(120);

impl super::Federation {
    pub async fn track_lyrics(
        &self,
        owner: Option<&str>,
        content_id: &str,
    ) -> Result<LyricsResponse> {
        let request = LyricsRequest::new(content_id)?;
        let preferred = owner.map(str::parse::<EndpointId>).transpose()?;
        let mut versions = Vec::new();
        if let (Ok(_permit), Ok(service)) = (self.lyrics_slots.try_acquire(), self.service().await)
        {
            let mut seen = HashSet::from([service.endpoint_id()]);
            let outcome =
                tokio::time::timeout(PEER_TIMEOUT, service.search_content_id(content_id)).await;
            let owners = outcome
                .ok()
                .and_then(Result::ok)
                .into_iter()
                .flat_map(|outcome| {
                    outcome
                        .local_results
                        .into_iter()
                        .chain(outcome.network_results)
                })
                .filter(|item| {
                    item.kind == music_dht::ItemKind::Track
                        && item.content_id.as_deref() == Some(content_id)
                })
                .map(|item| item.owner);
            let peers = preferred
                .into_iter()
                .chain(owners)
                .chain(service.connected_peers())
                .filter(|peer| seen.insert(*peer))
                .take(12);
            let mut replies = stream::iter(peers)
                .map(|peer| fetch(&service, peer, &request))
                .buffer_unordered(6);
            while let Some(reply) = replies.next().await {
                match reply {
                    Ok(found) => append_versions(&mut versions, found),
                    Err(error) => tracing::trace!(%error, "peer lyrics unavailable"),
                }
            }
            if !versions.is_empty() && self.save_on_listen.load(Ordering::Relaxed) {
                let result = async {
                    let pool = self.pool().await?;
                    crate::lyrics::save_peer_versions(&pool, content_id, &versions).await
                }
                .await;
                if let Err(error) = result {
                    tracing::warn!(%content_id, %error, "could not save federated lyrics");
                }
            }
        }
        Ok(response(content_id.to_owned(), versions))
    }

    pub fn refresh_track_lyrics(self: &Arc<Self>, track_id: i64, owner: Option<String>) {
        if !self.save_on_listen.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        {
            let mut attempts = super::lock(&self.lyrics_attempts);
            attempts.retain(|_, checked| now.duration_since(*checked) < LOOKUP_INTERVAL);
            if attempts.len() >= 512 || attempts.contains_key(&track_id) {
                return;
            }
            attempts.insert(track_id, now);
        }
        let federation = Arc::clone(self);
        tokio::spawn(async move {
            let result = async {
                let pool = federation.pool().await?;
                if let Some(content_id) =
                    crate::lyrics::track_content_id(&pool, track_id, true).await?
                {
                    federation
                        .track_lyrics(owner.as_deref(), &content_id)
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(track_id, %error, "could not refresh federated lyrics");
            }
        });
    }
}

fn append_versions(versions: &mut Vec<LyricsVersion>, found: Vec<LyricsVersion>) {
    for mut version in found {
        if versions.len() >= lyrics::MAX_LYRICS_VERSIONS {
            break;
        }
        if versions
            .iter()
            .any(|existing| crate::lyrics::same_text(existing, &version))
        {
            continue;
        }
        // Peer IDs are scoped to one peer's response and may collide.
        version.id = crate::lyrics::peer_version_id(&version);
        versions.push(version);
    }
}

async fn fetch(
    service: &MusicDhtService,
    peer: EndpointId,
    request: &LyricsRequest,
) -> Result<Vec<LyricsVersion>> {
    tokio::time::timeout(PEER_TIMEOUT, async {
        let mut stream = service.open_stream(peer, lyrics::LYRICS_ALPN).await?;
        lyrics::write_request(&mut stream, request).await?;
        stream.send.finish()?;
        let response = lyrics::read_response_from(&mut stream.recv).await?;
        anyhow::ensure!(
            response.content_id == request.content_id,
            "lyrics content identity mismatch"
        );
        Ok(response.versions)
    })
    .await
    .context("peer lyrics timed out")?
}

fn response(content_id: String, mut versions: Vec<LyricsVersion>) -> LyricsResponse {
    let mut remaining = lyrics::MAX_LYRICS_RESPONSE_BYTES - 256;
    versions.retain(|version| {
        let Ok(encoded) = serde_json::to_vec(version) else {
            return false;
        };
        let Some(rest) = remaining.checked_sub(encoded.len() + 1) else {
            return false;
        };
        remaining = rest;
        true
    });
    LyricsResponse {
        version: lyrics::LYRICS_PROTOCOL_VERSION,
        content_id,
        versions,
    }
}

pub async fn serve(mut acceptor: StreamAcceptor, pool: PgPool) {
    let limit = Arc::new(tokio::sync::Semaphore::new(16));
    while let Some(stream) = acceptor.accept().await {
        let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
            continue;
        };
        let pool = pool.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(Duration::from_secs(12), serve_one(stream, &pool)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "could not serve lyrics"),
                Err(_) => tracing::debug!("lyrics stream timed out"),
            }
        });
    }
}

async fn serve_one(mut stream: ByteStream, pool: &PgPool) -> Result<()> {
    let request = lyrics::read_request_from(&mut stream.recv).await?;
    let versions = match crate::lyrics::visible_content_hash(pool, &request.content_id).await? {
        Some(hash) => crate::lyrics::versions(pool, &hash).await?,
        None => vec![],
    };
    send_response(stream, response(request.content_id, versions)).await
}

async fn send_response(mut stream: ByteStream, reply: LyricsResponse) -> Result<()> {
    lyrics::write_response(&mut stream, &reply).await?;
    stream.send.finish()?;
    // ByteStream owns the connection: give the FIN time to reach the receiver.
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.send.stopped()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn localhost_peer_receives_complete_lyrics_response() {
        use music_dht::{MusicDhtConfig, NetworkId};

        let scratch = std::env::temp_dir().join(format!(
            "furumi-web-lyrics-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let server_dir = scratch.join("server");
        let client_dir = scratch.join("client");
        std::fs::create_dir_all(&server_dir).unwrap();
        std::fs::create_dir_all(&client_dir).unwrap();
        let network = NetworkId::from_name(&scratch.to_string_lossy());
        let server_config = MusicDhtConfig::builder()
            .data_dir(server_dir)
            .network_id(network)
            .schema_independent_stream_protocol(lyrics::LYRICS_ALPN)
            .build()
            .unwrap();
        let client_config = MusicDhtConfig::builder()
            .data_dir(client_dir)
            .network_id(network)
            .build()
            .unwrap();
        let (server, _server_events) = MusicDhtService::start(server_config).await.unwrap();
        let (client, _client_events) = MusicDhtService::start(client_config).await.unwrap();
        client
            .connect(server.ticket().await.unwrap())
            .await
            .unwrap();
        let mut acceptor = server.stream_acceptor(lyrics::LYRICS_ALPN).unwrap();
        let task = tokio::spawn(async move {
            let mut stream = acceptor.accept().await.unwrap();
            let request = lyrics::read_request_from(&mut stream.recv).await.unwrap();
            let version = LyricsVersion {
                id: "version".into(),
                label: "Lyrics".into(),
                source: "manual".into(),
                source_id: None,
                language: None,
                plain_text: Some("Words".into()),
                synced_lrc: None,
                instrumental: false,
            };
            send_response(stream, response(request.content_id, vec![version]))
                .await
                .unwrap();
        });
        let request = LyricsRequest::new(format!("b3:{}", "a".repeat(64))).unwrap();
        let versions = fetch(&client, server.endpoint_id(), &request)
            .await
            .unwrap();
        assert_eq!(versions[0].plain_text.as_deref(), Some("Words"));
        task.await.unwrap();
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        // Release both SQLite handles before removing the test directories on Windows.
        drop(client);
        drop(server);
        std::fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn merges_alternatives_with_colliding_peer_ids() {
        let make = |text: &str| LyricsVersion {
            id: "original".into(),
            label: "Lyrics".into(),
            source: "manual".into(),
            source_id: None,
            language: None,
            plain_text: Some(text.into()),
            synced_lrc: None,
            instrumental: false,
        };
        let mut versions = vec![];
        append_versions(
            &mut versions,
            vec![make("First"), make("Second"), make("First")],
        );
        assert_eq!(versions.len(), 2);
        assert_ne!(versions[0].id, versions[1].id);
        response(format!("b3:{}", "a".repeat(64)), versions)
            .validate()
            .unwrap();
    }
}
