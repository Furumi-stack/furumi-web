use crate::player::dto::{ArtistCard, UploaderSummary};
use crate::player::rows::{ReleaseUploaderRow, SearchArtistRow};

// Match by either credit. UNION keeps appearances on the artist's own releases
// and duplicate credit roles from counting as extra tracks.
const VISIBLE_ARTIST_TRACKS: &str = r#"
    SELECT credited.artist_id, t.id AS track_id, t.audio_file_id
      FROM (
          SELECT ta.artist_id, ta.track_id FROM furumusic__track_artist ta
           WHERE ta.artist_id = ANY($1)
          UNION
          SELECT ra.artist_id, t.id FROM furumusic__release_artist ra
            JOIN furumusic__track t ON t.release_id = ra.release_id
           WHERE ra.artist_id = ANY($1)
      ) credited
      JOIN furumusic__track t ON t.id = credited.track_id AND NOT t.is_hidden
      JOIN furumusic__release r ON r.id = t.release_id AND NOT r.is_hidden"#;

pub(super) async fn artist_track_totals(
    pool: &sqlx::PgPool,
    artist_id: i64,
) -> Result<(i64, i64, Vec<String>), sqlx::Error> {
    sqlx::query_as(&format!(
        "WITH tracks AS ({VISIBLE_ARTIST_TRACKS})
         SELECT COUNT(*),
                (SELECT COUNT(*) FROM furumusic__play_history ph JOIN tracks ON tracks.track_id = ph.track_id),
                ARRAY(SELECT DISTINCT c.content_id FROM tracks
                      JOIN furumusic__media_file m ON m.id = tracks.audio_file_id
                      JOIN furumusic__federation_content_id_cache c ON c.media_file_id = m.id AND c.sha256_hash = m.sha256_hash
                      ORDER BY c.content_id)
         FROM tracks"
    ))
    .bind(&[artist_id][..])
    .fetch_one(pool)
    .await
}

pub(super) async fn artist_cards(
    pool: &sqlx::PgPool,
    rows: Vec<SearchArtistRow>,
    image_variant: &str,
) -> Result<Vec<ArtistCard>, sqlx::Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    let counts: std::collections::HashMap<_, _> = sqlx::query_as::<_, (i64, i64)>(&format!(
        "SELECT artist_id, COUNT(*) FROM ({VISIBLE_ARTIST_TRACKS}) tracks GROUP BY artist_id"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(rows
        .into_iter()
        .map(|r| ArtistCard {
            id: r.id,
            name: r.name,
            image_url: cover_variant_url(r.image_file_id, image_variant),
            release_count: r.release_count,
            track_count: counts.get(&r.id).copied().unwrap_or_default(),
        })
        .collect())
}

pub(crate) fn cover_variant_url(file_id: Option<i64>, variant: &str) -> Option<String> {
    file_id.map(|id| format!("/api/player/cover/{id}/{variant}"))
}

pub(super) fn track_cover_variant_url(
    track_cover: Option<i64>,
    release_cover: Option<i64>,
    variant: &str,
) -> Option<String> {
    cover_variant_url(track_cover.or(release_cover), variant)
}

pub(super) async fn load_release_uploaders(
    pool: &sqlx::PgPool,
    release_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<UploaderSummary>>, sqlx::Error> {
    if release_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows = sqlx::query_as::<_, ReleaseUploaderRow>(
        r#"SELECT t.release_id,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  COUNT(*)::bigint AS track_count
           FROM furumusic__track t
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.release_id = ANY($1) AND t.is_hidden = false
           GROUP BY t.release_id, COALESCE(mf.uploader_name, 'UFO')
           ORDER BY t.release_id, track_count DESC, uploader_name"#,
    )
    .bind(release_ids)
    .fetch_all(pool)
    .await?;

    let mut map: std::collections::HashMap<i64, Vec<UploaderSummary>> =
        std::collections::HashMap::new();
    for row in rows {
        map.entry(row.release_id)
            .or_default()
            .push(UploaderSummary {
                name: row.uploader_name,
                track_count: row.track_count,
            });
    }
    Ok(map)
}
