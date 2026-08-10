//! Informational publication of the protocol versions exposed by this peer.

use std::time::Duration;

use anyhow::Result;
use music_dht::StreamAcceptor;
use music_dht::capabilities::{
    CAPABILITIES_PROTOCOL_VERSION, CapabilityManifest, CapabilityMessage, JAM_ID, SIMILARITY_ID,
    read_message, write_message,
};

use super::serve::AUDIO_PROTOCOL_VERSION;

fn local_manifest() -> CapabilityManifest {
    CapabilityManifest::frid("furumusic", env!("CARGO_PKG_VERSION"))
        // The web server does not expose federation Jam yet.
        .without_protocol(JAM_ID)
        .with_protocol("audio", AUDIO_PROTOCOL_VERSION)
        .with_protocol(
            SIMILARITY_ID,
            music_dht::similarity::SIMILARITY_PROTOCOL_VERSION,
        )
}

pub async fn serve(mut acceptor: StreamAcceptor) {
    while let Some(stream) = acceptor.accept().await {
        tokio::spawn(async move {
            if let Err(error) = serve_one(stream).await {
                tracing::debug!("capability stream failed: {error:#}");
            }
        });
    }
}

async fn serve_one(mut stream: music_dht::ByteStream) -> Result<()> {
    let response = match read_message(&mut stream).await? {
        CapabilityMessage::Get {
            version: CAPABILITIES_PROTOCOL_VERSION,
        } => CapabilityMessage::Manifest {
            manifest: local_manifest(),
        },
        CapabilityMessage::Get { version } => CapabilityMessage::Error {
            message: format!("unsupported capability protocol {version}"),
        },
        _ => CapabilityMessage::Error {
            message: "expected capability request".to_string(),
        },
    };
    write_message(&mut stream, &response).await?;
    stream.send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.send.stopped()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_describes_only_supported_player_protocols() {
        let manifest = local_manifest();
        assert_eq!(manifest.application, "furumusic");
        assert_eq!(
            manifest.protocols.get("audio"),
            Some(&AUDIO_PROTOCOL_VERSION)
        );
        assert!(!manifest.protocols.contains_key(JAM_ID));
        assert_eq!(
            manifest.protocols.get(SIMILARITY_ID),
            Some(&music_dht::similarity::SIMILARITY_PROTOCOL_VERSION)
        );
        assert_eq!(
            manifest
                .protocols
                .get(music_dht::capabilities::SIMILARITY_DHT_ID),
            Some(&music_dht::similarity_lsh::SIMILARITY_DHT_PROTOCOL_VERSION)
        );
        manifest.validate().unwrap();
    }
}
