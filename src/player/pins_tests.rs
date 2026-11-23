use super::*;

fn change(href: &str, pinned: bool) -> PinChange {
    PinChange {
        href: href.into(),
        pinned,
        title: "Peer album".into(),
        cover_url: Some("/api/player/federation/catalog/artwork/example".into()),
        before_href: None,
        after_href: None,
    }
}

async fn set(pool: &PgPool, user: i64, href: &str, pinned: bool) -> Result<bool, sqlx::Error> {
    Ok(mutate(pool, user, Mutation::Set(change(href, pinned))).await? == Outcome::Applied)
}

async fn move_before(
    pool: &PgPool,
    user: i64,
    href: &str,
    before: Option<&str>,
) -> Result<Outcome, sqlx::Error> {
    mutate(
        pool,
        user,
        Mutation::Move(PinMove {
            href: href.into(),
            before_href: before.map(String::from),
        }),
    )
    .await
}

async fn check_order(pool: &PgPool, expected: &[&str]) -> anyhow::Result<()> {
    let items = list(pool, 3).await?;
    let order: Vec<_> = items.iter().map(|p| p["href"].as_str().unwrap()).collect();
    assert_eq!(order, expected);
    Ok(())
}

async fn exercise_order(pool: &PgPool) -> anyhow::Result<()> {
    let [a, b, c, d] = [
        "#artistfed?name=a",
        "#artistfed?name=b",
        "#artistfed?name=c",
        "#artistfed?name=d",
    ];
    check_order(pool, &[a, b, c]).await?;
    for _ in 0..2 {
        assert_eq!(move_before(pool, 3, c, Some(a)).await?, Outcome::Applied);
        check_order(pool, &[c, a, b]).await?;
    }
    assert!(set(pool, 3, d, true).await?);
    assert_eq!(move_before(pool, 3, b, None).await?, Outcome::Applied);
    check_order(pool, &[c, a, d, b]).await?;
    assert!(set(pool, 3, a, false).await?);
    let undo = |next: &str| {
        let mut request = change(a, true);
        request.before_href = Some(next.into());
        request.after_href = Some(c.into());
        Mutation::Set(request)
    };
    assert_eq!(mutate(pool, 3, undo(d)).await?, Outcome::Applied);
    check_order(pool, &[c, a, d, b]).await?;
    assert_eq!(move_before(pool, 3, a, None).await?, Outcome::Applied);
    assert_eq!(mutate(pool, 3, undo(d)).await?, Outcome::Applied);
    check_order(pool, &[c, d, b, a]).await?;
    assert!(set(pool, 3, a, false).await?);
    assert_eq!(
        mutate(pool, 3, undo("#artistfed?name=gone")).await?,
        Outcome::Applied
    );
    check_order(pool, &[c, a, d, b]).await?;

    let (left, right) = tokio::try_join!(
        move_before(pool, 3, a, Some(c)),
        move_before(pool, 3, b, Some(d))
    )?;
    assert_eq!((left, right), (Outcome::Applied, Outcome::Applied));
    check_order(pool, &[a, c, b, d]).await?;
    assert_eq!(move_before(pool, 2, a, None).await?, Outcome::Stale);
    assert!(list(pool, 2).await?.is_empty());

    assert!(set(pool, 3, "#playlist/32", true).await?);
    sqlx::query("UPDATE furumusic__playlist SET is_public = false WHERE id=32")
        .execute(pool)
        .await?;
    assert_eq!(
        move_before(pool, 3, a, Some("#playlist/32")).await?,
        Outcome::Stale
    );
    assert_eq!(
        move_before(pool, 3, "#playlist/32", None).await?,
        Outcome::Stale
    );
    check_order(pool, &[a, c, b, d]).await?;
    sqlx::query("UPDATE furumusic__playlist SET is_public = true WHERE id=32")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM furumusic__user WHERE id=3")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
async fn sidebar_pins_respect_visibility_and_catalog_changes() -> anyhow::Result<()> {
    use std::str::FromStr;
    let url = std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?;
    let admin = PgPool::connect(&url).await?;
    let schema = format!("sidebar_pins_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await?;
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)?
        .options([("search_path", schema.as_str())]);
    let pool = PgPool::connect_with(options).await?;
    let result = exercise_pins(&pool).await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    result
}

async fn exercise_pins(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql("
        CREATE TABLE furumusic__user (id bigint PRIMARY KEY);
        CREATE TABLE furumusic__artist (id bigint PRIMARY KEY, name text, is_hidden bool, image_file_id bigint);
        CREATE TABLE furumusic__release (id bigint PRIMARY KEY, title text, is_hidden bool, cover_file_id bigint);
        CREATE TABLE furumusic__playlist (id bigint PRIMARY KEY, title text, owner_id bigint, is_public bool, cover_file_id bigint);
        CREATE TABLE furumusic__track (id bigint PRIMARY KEY, release_id bigint, is_hidden bool, cover_file_id bigint);
        CREATE TABLE furumusic__playlist_track (id bigint, playlist_id bigint, track_id bigint, position int);
        CREATE TABLE furumusic__saved_playlist (user_id bigint, playlist_id bigint);
        INSERT INTO furumusic__user VALUES (1),(2),(3);
        INSERT INTO furumusic__artist VALUES (10,'Artist',false,70), (11,'Hidden artist',true,NULL);
        INSERT INTO furumusic__release VALUES (12,'Album',false,80), (13,'Hidden album',true,81);
        INSERT INTO furumusic__track VALUES (20,12,true,90), (21,13,false,NULL), (22,12,false,NULL), (23,12,false,91);
        INSERT INTO furumusic__playlist VALUES (30,'My list',1,false,99), (31,'Someone else',2,false,NULL), (32,'Public list',2,true,NULL);
        INSERT INTO furumusic__playlist_track VALUES (1,32,20,1), (2,32,21,2), (3,32,22,3), (4,32,23,4);
    ").execute(pool).await?;
    sqlx::raw_sql(include_str!("sidebar_pins.sql"))
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO furumusic__sidebar_pin (user_id,href,peer_title,created_at) VALUES
        (3,'#artistfed?name=c','C','2026-01-02'), (3,'#artistfed?name=b','B','2026-01-01'), (3,'#artistfed?name=a','A','2026-01-01')")
        .execute(pool).await?;
    sqlx::raw_sql(include_str!("sidebar_pin_order.sql"))
        .execute(pool)
        .await?;
    exercise_order(pool).await?;
    assert!(set(pool, 1, "#playlist/30", true).await?);
    assert!(set(pool, 1, "#playlist/30", true).await?);
    assert_eq!(list(pool, 1).await?.len(), 1);
    assert_eq!(
        list(pool, 1).await?[0]["cover_url"],
        "/api/player/cover/99/small"
    );
    for href in ["#playlist/31", "#artist/11", "#release/13", "#artist/99"] {
        assert!(!set(pool, 1, href, true).await?, "{href}");
    }
    assert!(set(pool, 2, "#playlist/31", true).await?);
    assert!(set(pool, 1, "#playlist/31", false).await?);
    assert_eq!(list(pool, 2).await?.len(), 1);
    assert!(set(pool, 1, "#playlist/32", true).await?);
    let public = list(pool, 1)
        .await?
        .into_iter()
        .find(|p| p["id"] == 32)
        .unwrap();
    assert_eq!(public["cover_url"], "/api/player/cover/80/small");
    sqlx::query("UPDATE furumusic__playlist SET is_public = false WHERE id=32")
        .execute(pool)
        .await?;
    assert_eq!(list(pool, 1).await?.len(), 1);
    sqlx::query("INSERT INTO furumusic__saved_playlist VALUES (1,32)")
        .execute(pool)
        .await?;
    assert_eq!(list(pool, 1).await?.len(), 2);
    assert!(set(pool, 1, "#artist/10", true).await?);
    assert!(set(pool, 1, "#release/12", true).await?);
    let peer = "#releasefed?key=%7B%22normalized_title%22%3A%22album%22%7D";
    assert!(set(pool, 1, peer, true).await?);
    let pinned = list(pool, 1).await?;
    assert_eq!(pinned.len(), 5);
    let remote = pinned.into_iter().find(|p| p["href"] == peer).unwrap();
    assert_eq!(remote["title"], "Peer album");
    assert!(remote["id"].is_null());
    assert_eq!(
        remote["cover_url"],
        "/api/player/federation/catalog/artwork/example"
    );
    sqlx::raw_sql("DELETE FROM furumusic__artist WHERE id=10; DELETE FROM furumusic__release WHERE id=12; DELETE FROM furumusic__playlist WHERE id=30;")
        .execute(pool).await?;
    assert_eq!(list(pool, 1).await?.len(), 2);
    assert!(set(pool, 1, peer, false).await?);
    sqlx::query("DELETE FROM furumusic__user WHERE id=1")
        .execute(pool)
        .await?;
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__sidebar_pin")
        .fetch_one(pool)
        .await?;
    assert_eq!(remaining, 1);
    assert_eq!(list(pool, 2).await?[0]["href"], "#playlist/31");
    Ok(())
}
