//! Optional lyrics ALPN; existing audio and catalog framing stay unchanged.
use anyhow::Result;
use music_dht::lyrics::{self, LYRICS_PROTOCOL_VERSION, LyricsResponse};
use music_dht::{ByteStream, StreamAcceptor};
use sqlx::PgPool;
use std::time::Duration;

impl super::Federation {
    pub async fn track_lyrics(&self, owner: &str, content_id: &str) -> Result<LyricsResponse> {
        let request = lyrics::LyricsRequest::new(content_id)?;
        let service = self.service().await?;
        let owner = owner.parse::<music_dht::EndpointId>()?;
        let response = tokio::time::timeout(Duration::from_secs(15), async {
            let mut stream = service.open_stream(owner, lyrics::LYRICS_ALPN).await?;
            lyrics::write_request(&mut stream, &request).await?;
            stream.send.finish()?;
            let response = lyrics::read_response_from(&mut stream.recv).await?;
            anyhow::ensure!(
                response.content_id == request.content_id,
                "lyrics content identity mismatch"
            );
            Ok::<_, anyhow::Error>(response)
        })
        .await;
        // Optional protocol: older peers do not support this ALPN.
        match response {
            Ok(Ok(response)) => Ok(response),
            _ => Ok(LyricsResponse {
                version: LYRICS_PROTOCOL_VERSION,
                content_id: request.content_id,
                versions: vec![],
            }),
        }
    }
}

pub async fn serve(mut acceptor: StreamAcceptor, pool: PgPool) {
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
    while let Some(stream) = acceptor.accept().await {
        let Ok(permit) = std::sync::Arc::clone(&limit).try_acquire_owned() else {
            continue;
        };
        let pool = pool.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) =
                tokio::time::timeout(Duration::from_secs(30), serve_one(stream, &pool)).await
            {
                tracing::debug!(%error,"lyrics stream timed out");
            }
        });
    }
}
async fn serve_one(mut stream: ByteStream, pool: &PgPool) -> Result<()> {
    let request = lyrics::read_request_from(&mut stream.recv).await?;
    let hash:Option<String>=sqlx::query_scalar("SELECT m.sha256_hash::text FROM furumusic__track t
        JOIN furumusic__release r ON r.id=t.release_id
        JOIN furumusic__media_file m ON m.id=t.audio_file_id
        JOIN furumusic__federation_content_id_cache c ON c.media_file_id=m.id AND c.sha256_hash=m.sha256_hash
        WHERE c.content_id=$1 AND NOT t.is_hidden AND NOT r.is_hidden LIMIT 1")
        .bind(&request.content_id).fetch_optional(pool).await?;
    let versions = match hash {
        Some(hash) => crate::lyrics::versions(pool, &hash).await?,
        None => vec![],
    };
    lyrics::write_response(
        &mut stream,
        &LyricsResponse {
            version: LYRICS_PROTOCOL_VERSION,
            content_id: request.content_id,
            versions,
        },
    )
    .await?;
    stream.send.finish()?;
    Ok(())
}
