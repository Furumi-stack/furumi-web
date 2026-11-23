use super::{SearchArtistRow, helpers, load_artist_detail};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires FURU_ARTIST_TEST_DATABASE_URL pointing to PostgreSQL"]
async fn artist_credits_include_visible_features_and_count_each_track_once() -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("FURU_ARTIST_TEST_DATABASE_URL")?)
        .await?;
    sqlx::raw_sql(
        r#"
        CREATE TEMP TABLE furumusic__artist (
            id bigint, name text, image_file_id bigint, is_hidden bool DEFAULT false);
        CREATE TEMP TABLE furumusic__release (
            id bigint, title text, title_sort text, release_type text DEFAULT 'album', year int,
            cover_file_id bigint, is_hidden bool DEFAULT false);
        CREATE TEMP TABLE furumusic__track (
            id bigint, title text, release_id bigint, audio_file_id bigint, track_number int,
            disc_number int, duration_seconds float8 DEFAULT 120, cover_file_id bigint,
            lastfm_listeners bigint, lastfm_playcount bigint, lastfm_rating float8,
            lastfm_updated_at text, is_hidden bool DEFAULT false);
        CREATE TEMP TABLE furumusic__media_file (
            id bigint, sha256_hash text, uploader_name text, audio_format text, audio_bitrate int,
            audio_sample_rate int, audio_bit_depth int, file_size_bytes bigint);
        CREATE TEMP TABLE furumusic__release_artist (artist_id bigint, release_id bigint);
        CREATE TEMP TABLE furumusic__track_artist (artist_id bigint, track_id bigint, position int, role text);
        CREATE TEMP TABLE furumusic__play_history (track_id bigint);
        CREATE TEMP TABLE furumusic__lyrics (audio_sha256 text);
        CREATE TEMP TABLE furumusic__federation_content_id_cache (media_file_id bigint, sha256_hash text, content_id text);

        INSERT INTO furumusic__artist (id,name) VALUES (1,'Кизиль'),(2,'Трагедия Всей Жизни');
        INSERT INTO furumusic__release (id,title,cover_file_id,is_hidden)
            VALUES (10,'13/87',123,false),(11,'Hidden release',NULL,true),(12,'Compilation',NULL,false);
        INSERT INTO furumusic__release_artist VALUES (2,10);
        INSERT INTO furumusic__track (id,title,release_id,audio_file_id,is_hidden) VALUES
            (20,'Диагноз',10,30,false),(21,'Hidden track',10,NULL,true),(22,'Hidden release track',11,NULL,false),
            (23,'Only release credit',10,31,false),(24,'Only track credit',12,NULL,false);
        INSERT INTO furumusic__track_artist VALUES
            (2,20,0,'main'),(1,20,1,'featuring'),(1,21,1,'featuring'),(1,22,1,'featuring'),(2,24,0,'main');
        INSERT INTO furumusic__media_file (id,sha256_hash,uploader_name) VALUES (30,'abc','test'),(31,'new','test');
        INSERT INTO furumusic__federation_content_id_cache VALUES (30,'abc','valid-id'),(31,'old','stale-id');
        INSERT INTO furumusic__play_history VALUES (20),(20),(21),(22),(23),(24);
        "#,
    )
    .execute(&pool)
    .await?;

    let guest = load_artist_detail(&pool, 1).await?.unwrap();
    assert!(guest.releases.is_empty());
    assert!(guest.top_tracks.is_empty());
    assert_eq!(guest.total_track_count, 1);
    assert_eq!(guest.total_play_count, 2);
    assert_eq!(guest.track_content_ids, ["valid-id"]);
    assert_eq!(guest.featured_tracks.len(), 1);
    let track = &guest.featured_tracks[0];
    assert_eq!(track.id, 20);
    assert_eq!(track.content_id.as_deref(), Some("valid-id"));
    assert_eq!(track.stream_url, "/api/player/stream/20");
    assert_eq!(
        track.cover_url.as_deref(),
        Some("/api/player/cover/123/medium")
    );
    assert_eq!(track.uploader_name, "test");
    assert_eq!(track.artists[0].id, 2);
    assert_eq!(track.featured_artists[0].id, 1);

    let main = load_artist_detail(&pool, 2).await?.unwrap();
    assert_eq!(main.releases.len(), 1);
    assert_eq!(main.total_track_count, 3);
    assert_eq!(main.total_play_count, 4);
    assert_eq!(main.track_content_ids, ["valid-id"]);
    let top_ids: Vec<_> = main.top_tracks.iter().map(|track| track.id).collect();
    assert_eq!(top_ids, [20, 23, 24]);

    let rows = [&guest, &main].map(|artist| SearchArtistRow {
        id: artist.id,
        name: artist.name.clone(),
        image_file_id: None,
        release_count: artist.releases.len() as i64,
    });
    let cards = helpers::artist_cards(&pool, rows.into(), "medium").await?;
    assert_eq!((cards[0].release_count, cards[0].track_count), (0, 1));
    assert_eq!((cards[1].release_count, cards[1].track_count), (1, 3));
    Ok(())
}
