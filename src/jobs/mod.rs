pub mod archive_cleanup;
pub mod artwork_backfill;
pub mod inbox_discover;
pub mod inbox_process;
pub mod lastfm_popularity;
pub mod lastfm_scrobble;
pub mod metadata_backfill;
pub mod musicbrainz;

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct UploaderAttribution {
    pub user_id: Option<i64>,
    pub name: String,
}

impl UploaderAttribution {
    pub fn unknown() -> Self {
        Self {
            user_id: None,
            name: "UFO".to_string(),
        }
    }
}

pub fn strip_user_upload_prefix(relative_path: &Path) -> PathBuf {
    let components: Vec<_> = relative_path.components().collect();
    if components.len() >= 3
        && matches!(components[0], Component::Normal(value) if value == "user_uploads")
    {
        let start = if matches!(components[2], Component::Normal(value)
            if value.to_str().and_then(|v| v.strip_prefix("local-"))
                .and_then(|v| uuid::Uuid::parse_str(v).ok()).is_some())
        {
            3
        } else {
            2
        };
        components[start..].iter().collect()
    } else {
        relative_path.to_path_buf()
    }
}

pub async fn uploader_from_relative_path(
    pool: &sqlx::PgPool,
    relative_path: &Path,
) -> UploaderAttribution {
    let components: Vec<_> = relative_path.components().collect();
    let Some(Component::Normal(root)) = components.first() else {
        return UploaderAttribution::unknown();
    };
    if *root != "user_uploads" {
        return UploaderAttribution::unknown();
    }

    let Some(Component::Normal(user_id_os)) = components.get(1) else {
        return UploaderAttribution::unknown();
    };
    let Some(user_id_str) = user_id_os.to_str() else {
        return UploaderAttribution::unknown();
    };
    let Ok(user_id) = user_id_str.parse::<i64>() else {
        return UploaderAttribution::unknown();
    };

    let name: Option<String> = sqlx::query_scalar(
        r#"SELECT COALESCE(NULLIF(display_name, ''), username)::text
           FROM furumusic__user
           WHERE id = $1 AND is_active = true"#,
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    match name {
        Some(name) if !name.trim().is_empty() => UploaderAttribution {
            user_id: Some(user_id),
            name,
        },
        _ => UploaderAttribution::unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_upload_batch_id_is_not_an_artist_hint() {
        let path = Path::new(
            "user_uploads/6/local-123e4567-e89b-12d3-a456-426614174000/Artist/Album/01.mp3",
        );
        assert_eq!(
            strip_user_upload_prefix(path),
            PathBuf::from("Artist/Album/01.mp3")
        );
        assert_eq!(
            strip_user_upload_prefix(Path::new(
                "user_uploads/6/local-123e4567-e89b-12d3-a456-426614174000/01.mp3"
            )),
            PathBuf::from("01.mp3")
        );
    }

    #[test]
    fn ordinary_upload_folder_names_are_preserved() {
        assert_eq!(
            strip_user_upload_prefix(Path::new("user_uploads/6/local-band/Album/01.mp3")),
            PathBuf::from("local-band/Album/01.mp3")
        );
    }
}
