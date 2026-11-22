use super::*;
use cot::Template;

#[test]
fn episode_routes_accept_encoded_colons_and_reject_invalid_ids() {
    let id = format!("ep1:{}", "a".repeat(52));
    assert_eq!(episode_path_id(&id).unwrap().as_str(), id);
    assert_eq!(
        episode_path_id(&id.replace(':', "%3A")).unwrap().as_str(),
        id
    );
    assert_eq!(
        episode_path_id(&id.replace(':', "%3a")).unwrap().as_str(),
        id
    );
    assert!(episode_path_id("../private").is_err());
}

#[test]
fn renders_podcast_navigation_and_views() {
    let html = crate::player::PlayerPageTemplate {
        t: crate::i18n::Translations::for_lang(crate::i18n::Lang::En),
        guest: false,
        share_token: String::new(),
        downloads_enabled: true,
        torrent_downloads_enabled: true,
        youtube_downloads_enabled: true,
    }
    .render()
    .unwrap();
    assert_eq!(
        html.matches("@click=\"$store.podcasts.enter()\"").count(),
        2
    );
    assert!(html.contains("/api/podcasts/action"));
    if std::env::var_os("FURU_PODCAST_RENDER_FIXTURE").is_some() {
        std::fs::write("target/podcast-player-fixture.html", html).unwrap();
    }
}

#[tokio::test]
#[ignore = "requires FURU_PODCAST_TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
async fn postgres_multiuser_progress_retention_and_admin() {
    let url = std::env::var("FURU_PODCAST_TEST_DATABASE_URL").expect("disposable database URL");
    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = format!("podcast_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .unwrap();
    let search_path = format!("SET search_path TO {schema}");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |connection, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query(&query).execute(connection).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE furumusic__user(id BIGINT PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    for statement in include_str!("schema.sql")
        .split(';')
        .filter(|s| !s.trim().is_empty())
    {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    // Repeatability matters for partially applied upgrades.
    for statement in include_str!("schema.sql")
        .split(';')
        .filter(|s| !s.trim().is_empty())
    {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO furumusic__user VALUES(1),(2)")
        .execute(&pool)
        .await
        .unwrap();
    let origin = FeedUrl::parse("https://example.test/feed.xml").unwrap();
    let podcast = PodcastId::from_feed_url(&origin);
    let feed=parse_feed_with_base(br#"<rss><channel><title>Fixture</title><item><title>Episode</title><guid>one</guid><enclosure url="https://example.test/audio.mp3"/></item></channel></rss>"#,Some(origin.as_str())).unwrap();
    let episode = feed.episodes[0].to_meta(
        &podcast,
        feed.resolve_episode_ids(&podcast, &[])[0].clone().unwrap(),
    );
    if std::env::var_os("FURU_PODCAST_RENDER_FIXTURE").is_some() {
        std::fs::write("target/podcast-feed-fixture.json",json!({"id":podcast,"metadata":feed.to_podcast_meta(origin.clone()),"episode":episode,"cache_name":blake3::hash(episode.episode_id.as_str().as_bytes()).to_hex().to_string()}).to_string()).unwrap();
    }
    sqlx::query(
        "INSERT INTO furumusic__podcast(id,origin_url,feed_url,metadata) VALUES($1,$2,$2,$3)",
    )
    .bind(podcast.as_str())
    .bind(origin.as_str())
    .bind(json!(feed.to_podcast_meta(origin.clone())))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO furumusic__podcast_subscription(user_id,podcast_id,shared) VALUES(1,$1,true),(2,$1,false)").bind(podcast.as_str()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO furumusic__podcast_episode(id,podcast_id,metadata) VALUES($1,$2,$3)")
        .bind(episode.episode_id.as_str())
        .bind(podcast.as_str())
        .bind(json!(episode))
        .execute(&pool)
        .await
        .unwrap();
    let directory =
        std::env::temp_dir().join(format!("furumusic-podcast-test-{}", uuid::Uuid::new_v4()));
    let config = AppConfig {
        agent_storage_dir: directory.to_string_lossy().into_owned(),
        ..AppConfig::default()
    };
    let media = cache_path(&config, episode.episode_id.as_str());
    tokio::fs::create_dir_all(media.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&media, b"saved episode").await.unwrap();
    sqlx::query("UPDATE furumusic__podcast_episode SET cached_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let range_request = Request::builder()
        .header("range", "bytes=0-4")
        .body(Body::empty())
        .unwrap();
    let response = super::media(
        &pool,
        &config,
        2,
        episode.episode_id.as_str(),
        &range_request,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 0-4/13");
    assert_eq!(
        response.into_body().into_bytes().await.unwrap().as_ref(),
        b"saved"
    );
    let invalid_request = Request::builder()
        .header("range", "bytes=900-1000")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        super::media(
            &pool,
            &config,
            2,
            episode.episode_id.as_str(),
            &invalid_request
        )
        .await
        .unwrap()
        .status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    assert!(
        super::media(
            &pool,
            &config,
            99,
            episode.episode_id.as_str(),
            &range_request
        )
        .await
        .is_err()
    );
    assert_eq!(
        list(&pool, 2, false).await.unwrap()["public"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let request = |value| serde_json::from_value::<Action>(value).unwrap();
    action(
        pool.clone(),
        config.clone(),
        1,
        false,
        request(json!({"action":"remove","id":podcast})),
    )
    .await
    .unwrap();
    assert_eq!(
        list(&pool, 2, false).await.unwrap()["mine"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(tokio::fs::try_exists(&media).await.unwrap());
    let first = action(
        pool.clone(),
        config.clone(),
        2,
        false,
        request(
            json!({"action":"progress","id":episode.episode_id,"explicit":true,"position_ms":5000}),
        ),
    )
    .await
    .unwrap();
    let second = action(
        pool.clone(),
        config.clone(),
        2,
        false,
        request(
            json!({"action":"progress","id":episode.episode_id,"explicit":true,"position_ms":0}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        second["generation"].as_i64().unwrap(),
        first["generation"].as_i64().unwrap() + 1
    );
    action(pool.clone(),config.clone(),2,false,request(json!({"action":"progress","id":episode.episode_id,"generation":first["generation"],"sequence":999,"position_ms":99999}))).await.unwrap();
    let position: i64 = sqlx::query_scalar("SELECT position_ms FROM furumusic__podcast_progress")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(position, 0);
    assert!(
        action(
            pool.clone(),
            config.clone(),
            2,
            false,
            request(json!({"action":"admin_settings","retention_days":7}))
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE furumusic__podcast_episode SET cached_at=now()-interval '20 days'")
        .execute(&pool)
        .await
        .unwrap();
    crate::podcasts::cleanup(&pool, &config).await.unwrap();
    assert!(tokio::fs::try_exists(&media).await.unwrap());
    action(
        pool.clone(),
        config.clone(),
        1,
        true,
        request(json!({"action":"admin_settings","retention_days":7})),
    )
    .await
    .unwrap();
    crate::podcasts::cleanup(&pool, &config).await.unwrap();
    assert!(!tokio::fs::try_exists(&media).await.unwrap());
    let state: (bool,i64)=sqlx::query_as("SELECT expired,(SELECT count(*) FROM furumusic__podcast_progress) FROM furumusic__podcast_episode").fetch_one(&pool).await.unwrap();
    assert_eq!(state, (true, 1));
    action(
        pool.clone(),
        config.clone(),
        1,
        true,
        request(json!({"action":"admin_delete","id":podcast})),
    )
    .await
    .unwrap();
    assert!(
        list(&pool, 2, false).await.unwrap()["mine"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    tokio::fs::remove_dir_all(&directory).await.unwrap();
}
