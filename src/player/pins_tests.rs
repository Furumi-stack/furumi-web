use super::*;

async fn set(pool: &PgPool, user: i64, href: &str, pinned: bool) -> Result<bool, sqlx::Error> {
    let change = PinChange {
        href: href.into(),
        pinned,
        title: "Peer album".into(),
        cover_url: Some("/api/player/federation/catalog/artwork/example".into()),
    };
    update_pin(pool, user, Target::parse(href).unwrap(), &change).await
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
async fn sidebar_pins_respect_visibility_and_catalog_changes() -> anyhow::Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?)
        .await?;
    // PostgreSQL creates the migration's table in the connection-local temporary schema.
    sqlx::raw_sql("
        SET search_path TO pg_temp;
        CREATE TEMP TABLE furumusic__user (id bigint PRIMARY KEY);
        CREATE TEMP TABLE furumusic__artist (id bigint PRIMARY KEY, name text, is_hidden bool, image_file_id bigint);
        CREATE TEMP TABLE furumusic__release (id bigint PRIMARY KEY, title text, is_hidden bool, cover_file_id bigint);
        CREATE TEMP TABLE furumusic__playlist (id bigint PRIMARY KEY, title text, owner_id bigint, is_public bool, cover_file_id bigint);
        CREATE TEMP TABLE furumusic__track (id bigint PRIMARY KEY, release_id bigint, is_hidden bool, cover_file_id bigint);
        CREATE TEMP TABLE furumusic__playlist_track (id bigint, playlist_id bigint, track_id bigint, position int);
        CREATE TEMP TABLE furumusic__saved_playlist (user_id bigint, playlist_id bigint);
        INSERT INTO furumusic__user VALUES (1),(2);
        INSERT INTO furumusic__artist VALUES (10,'Artist',false,70), (11,'Hidden artist',true,NULL);
        INSERT INTO furumusic__release VALUES (12,'Album',false,80), (13,'Hidden album',true,81);
        INSERT INTO furumusic__track VALUES (20,12,true,90), (21,13,false,NULL), (22,12,false,NULL), (23,12,false,91);
        INSERT INTO furumusic__playlist VALUES (30,'My list',1,false,99), (31,'Someone else',2,false,NULL), (32,'Public list',2,true,NULL);
        INSERT INTO furumusic__playlist_track VALUES (1,32,20,1), (2,32,21,2), (3,32,22,3), (4,32,23,4);
    ").execute(&pool).await?;
    sqlx::raw_sql(include_str!("sidebar_pins.sql"))
        .execute(&pool)
        .await?;
    assert!(set(&pool, 1, "#playlist/30", true).await?);
    assert!(set(&pool, 1, "#playlist/30", true).await?);
    assert_eq!(list(&pool, 1).await?.len(), 1);
    assert_eq!(
        list(&pool, 1).await?[0]["cover_url"],
        "/api/player/cover/99/small"
    );
    for href in ["#playlist/31", "#artist/11", "#release/13", "#artist/99"] {
        assert!(!set(&pool, 1, href, true).await?, "{href}");
    }
    assert!(set(&pool, 2, "#playlist/31", true).await?);
    assert!(set(&pool, 1, "#playlist/31", false).await?);
    assert_eq!(list(&pool, 2).await?.len(), 1);
    assert!(set(&pool, 1, "#playlist/32", true).await?);
    let public = list(&pool, 1)
        .await?
        .into_iter()
        .find(|p| p["id"] == 32)
        .unwrap();
    assert_eq!(public["cover_url"], "/api/player/cover/80/small");
    sqlx::query("UPDATE furumusic__playlist SET is_public = false WHERE id=32")
        .execute(&pool)
        .await?;
    assert_eq!(list(&pool, 1).await?.len(), 1);
    sqlx::query("INSERT INTO furumusic__saved_playlist VALUES (1,32)")
        .execute(&pool)
        .await?;
    assert_eq!(list(&pool, 1).await?.len(), 2);
    assert!(set(&pool, 1, "#artist/10", true).await?);
    assert!(set(&pool, 1, "#release/12", true).await?);
    let peer = "#releasefed?key=%7B%22normalized_title%22%3A%22album%22%7D";
    assert!(set(&pool, 1, peer, true).await?);
    let pinned = list(&pool, 1).await?;
    assert_eq!(pinned.len(), 5);
    let remote = pinned.into_iter().find(|p| p["href"] == peer).unwrap();
    assert_eq!(remote["title"], "Peer album");
    assert!(remote["id"].is_null());
    assert_eq!(
        remote["cover_url"],
        "/api/player/federation/catalog/artwork/example"
    );
    sqlx::raw_sql("DELETE FROM furumusic__artist WHERE id=10; DELETE FROM furumusic__release WHERE id=12; DELETE FROM furumusic__playlist WHERE id=30;")
        .execute(&pool).await?;
    assert_eq!(list(&pool, 1).await?.len(), 2);
    assert!(set(&pool, 1, peer, false).await?);
    sqlx::query("DELETE FROM furumusic__user WHERE id=1")
        .execute(&pool)
        .await?;
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__sidebar_pin")
        .fetch_one(&pool)
        .await?;
    assert_eq!(remaining, 1);
    assert_eq!(list(&pool, 2).await?[0]["href"], "#playlist/31");
    Ok(())
}
