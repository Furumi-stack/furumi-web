//! Pinned files for the E5 embedding profile, cached beside the audio similarity models.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};

const BASE_URL: &str = "https://huggingface.co/intfloat/multilingual-e5-small/resolve/614241f622f53c4eeff9890bdc4f31cfecc418b3/onnx";
const NOTICE: &str = include_str!("e5-notice.txt");

struct Artifact {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

const FILES: &[Artifact] = &[
    Artifact {
        name: "model.onnx",
        size: 470_268_510,
        sha256: "ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665",
    },
    Artifact {
        name: "tokenizer.json",
        size: 17_082_730,
        sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    },
    Artifact {
        name: "config.json",
        size: 653,
        sha256: "bbb7c1333fc4b3e27fbc9cd5d2070aabcc1d4dfb99917c3633e772f97545a6b6",
    },
    Artifact {
        name: "special_tokens_map.json",
        size: 167,
        sha256: "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    },
    Artifact {
        name: "tokenizer_config.json",
        size: 443,
        sha256: "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    },
];

pub(super) fn directory() -> PathBuf {
    std::env::var_os("FURU_LYRICS_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::media_paths::resolve_config_path_buf("similarity-models/multilingual-e5-small")
        })
}

pub(super) async fn prepare(download: bool) -> Result<PathBuf> {
    static PREPARING: Mutex<()> = Mutex::const_new(());
    let _guard = PREPARING.lock().await;
    let dir = directory();
    prepare_files(&dir, BASE_URL, FILES, download).await?;
    Ok(dir)
}

async fn prepare_files(
    dir: &Path,
    base_url: &str,
    files: &[Artifact],
    download: bool,
) -> Result<()> {
    let mut missing = Vec::new();
    for file in files {
        let path = dir.join(file.name);
        let size = match fs::metadata(&path).await {
            Ok(meta) => Some(meta.len()),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if size == Some(file.size) && crate::agent::mover::sha256_file(&path).await? == file.sha256
        {
            continue;
        }
        ensure!(
            download,
            "missing or invalid cached lyrics model file {}",
            path.display()
        );
        missing.push(file);
    }
    if missing.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut notice = NamedTempFile::new_in(dir)?;
    notice.write_all(NOTICE.as_bytes())?;
    notice.persist(dir.join("NOTICE"))?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()?;
    for file in missing {
        tracing::info!(
            "Downloading lyrics model file {} ({} bytes)",
            file.name,
            file.size
        );
        download_file(&client, base_url, dir, file)
            .await
            .with_context(|| format!("downloading lyrics model file {}", file.name))?;
    }
    Ok(())
}

async fn download_file(
    client: &reqwest::Client,
    base_url: &str,
    dir: &Path,
    file: &Artifact,
) -> Result<()> {
    let mut response = client
        .get(format!("{base_url}/{}", file.name))
        .send()
        .await?
        .error_for_status()?;
    // A unique temporary file beside the destination makes publication atomic even
    // if two instances share a volume. Cancelling this future discards its partial file.
    let (handle, temp_path) = NamedTempFile::new_in(dir)?.into_parts();
    let mut output = fs::File::from_std(handle);
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    while let Some(chunk) = response.chunk().await? {
        bytes += chunk.len() as u64;
        ensure!(bytes <= file.size, "response exceeds pinned file size");
        hash.update(&chunk);
        output.write_all(&chunk).await?;
    }
    ensure!(
        bytes == file.size,
        "incomplete response ({bytes} of {} bytes)",
        file.size
    );
    ensure!(
        format!("{:x}", hash.finalize()) == file.sha256,
        "SHA-256 mismatch"
    );
    output.flush().await?;
    drop(output);
    temp_path.persist(dir.join(file.name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    const EXAMPLE: &[Artifact] = &[Artifact {
        name: "model.onnx",
        size: 5,
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    }];

    #[tokio::test]
    async fn repairs_corrupt_model_and_reuses_verified_files_offline() -> Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("model");
        assert!(prepare_files(&dir, "", EXAMPLE, false).await.is_err());
        assert!(!dir.exists());
        fs::create_dir(&dir).await?;
        let model = dir.join("model.onnx");
        fs::write(&model, b"prior").await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            for body in ["wrong", "hello"] {
                let (mut socket, _) = listener.accept().await?;
                let mut buffer = [0; 4096];
                socket.read(&mut buffer).await?;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n{body}"
                );
                socket.write_all(reply.as_bytes()).await?;
            }
            std::io::Result::Ok(())
        });
        let err = prepare_files(&dir, &url, EXAMPLE, true).await.unwrap_err();
        assert!(format!("{err:#}").contains("SHA-256 mismatch"));
        assert_eq!(fs::read(&model).await?, b"prior");
        assert_eq!(dir.read_dir()?.count(), 2);
        prepare_files(&dir, &url, EXAMPLE, true).await?;
        server.await??;
        assert_eq!(fs::read(&model).await?, b"hello");
        assert_eq!(fs::read_to_string(dir.join("NOTICE")).await?, NOTICE);
        prepare_files(&dir, "", EXAMPLE, true).await?;
        prepare_files(&dir, "", EXAMPLE, false).await
    }

    #[tokio::test]
    #[ignore = "uses the real E5 model cache and downloads missing files"]
    async fn prepare_pinned_model() {
        prepare(true).await.unwrap();
    }
}
