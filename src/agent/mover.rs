use std::path::{Path, PathBuf};

pub enum MoveOutcome {
    /// File was moved/renamed to destination.
    Moved(PathBuf),
    /// Destination already existed; inbox duplicate was removed.
    Merged(PathBuf),
}

/// Move a file from inbox to the permanent storage directory.
///
/// Creates the directory structure: `storage_dir/artist/album/filename`
///
/// Copies into the destination filesystem before removing the inbox source.
/// Filename collisions are merged only when both physical files have the
/// same SHA-256. Distinct recordings receive a separate storage filename.
pub async fn move_to_storage(
    storage_dir: &Path,
    artist: &str,
    album: &str,
    filename: &str,
    source: &Path,
) -> anyhow::Result<MoveOutcome> {
    let artist_dir = sanitize_dir_name(artist);
    let album_dir = sanitize_dir_name(album);

    let dest_dir = storage_dir.join(&artist_dir).join(&album_dir);
    tokio::fs::create_dir_all(&dest_dir).await?;

    let mut dest = dest_dir.join(filename);

    // File already at destination — remove the inbox duplicate
    if dest.exists() {
        if sha256_file(source).await? == sha256_file(&dest).await? {
            tokio::fs::remove_file(source).await?;
            tracing::info!(from = ?source, to = ?dest, "merged duplicate into existing storage file");
            return Ok(MoveOutcome::Merged(dest));
        }
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        let ext = Path::new(filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        dest = dest_dir.join(format!("{stem} [{}].{ext}", uuid::Uuid::new_v4()));
    }

    publish_copy(source, &dest).await?;
    tokio::fs::remove_file(source).await?;

    tracing::info!(from = ?source, to = ?dest, "moved file to storage");
    Ok(MoveOutcome::Moved(dest))
}

/// Copy into the destination filesystem, then expose the complete file without
/// replacing an existing destination. The source remains available on errors.
pub async fn publish_copy(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("destination has no parent"))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".{}.moving", uuid::Uuid::new_v4()));
    let result = async {
        tokio::fs::copy(source, &temporary).await?;
        tokio::fs::hard_link(&temporary, destination).await
    }
    .await;
    let _ = tokio::fs::remove_file(temporary).await;
    result?;
    Ok(())
}

pub async fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok::<_, std::io::Error>(format!("{:x}", hash.finalize()))
    })
    .await?
    .map_err(Into::into)
}

/// Remove characters that are unsafe for directory names.
fn sanitize_dir_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn filename_collision_does_not_discard_a_different_recording() {
        let root = std::env::temp_dir().join(format!("furumi-move-{}", uuid::Uuid::new_v4()));
        let storage = root.join("storage");
        tokio::fs::create_dir_all(storage.join("Artist/Album"))
            .await
            .unwrap();
        let old = storage.join("Artist/Album/song.mp3");
        let incoming = root.join("incoming.mp3");
        tokio::fs::write(&old, b"original recording").await.unwrap();
        tokio::fs::write(&incoming, b"different recording")
            .await
            .unwrap();
        let outcome = move_to_storage(&storage, "Artist", "Album", "song.mp3", &incoming)
            .await
            .unwrap();
        let MoveOutcome::Moved(new) = outcome else {
            panic!("different recording was merged")
        };
        assert_ne!(new, old);
        assert_eq!(tokio::fs::read(&old).await.unwrap(), b"original recording");
        assert_eq!(tokio::fs::read(new).await.unwrap(), b"different recording");
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
