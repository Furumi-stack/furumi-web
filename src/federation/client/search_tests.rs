use super::*;
use music_dht::{ItemId, SecretKey};

fn item(kind: ItemKind, name: &str, owner_seed: u8) -> LibraryItem {
    let owner = SecretKey::from_bytes(&[owner_seed; 32]).public();
    LibraryItem {
        id: ItemId::derive(&owner, kind, name),
        owner,
        kind,
        name: name.into(),
        normalized_name: music_dht::normalize_name(name),
        artist_names: vec!["Саша Скул".into()],
        featured_artist_names: Vec::new(),
        release_title: None,
        release_type: None,
        year: Some(2014),
        content_id: None,
        track_number: None,
        disc_number: None,
        duration_seconds: None,
        revision: 1,
        deleted: false,
        updated_at_ms: 1,
    }
}

#[test]
fn legacy_release_key_includes_edition_and_normalizes_credit_set() {
    let key = ReleaseKeyDto::from_url(r#"{"normalized_title":"Рэп из-под стола (Remastered)","primary_artists":["Z","Гнойный","z"],"release_type":null,"year":2013}"#).unwrap();
    assert_eq!(key.normalized_title, "рэп из под стола remastered");
    assert_eq!(key.primary_artists, ["z", "гнойный"]);
    assert_eq!(key.release_type, None);
    assert!(ReleaseKeyDto::from_url("broken").is_none());
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn postgres_search_resolves_local_editions_and_preserves_sources() -> Result<()> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&std::env::var("FURU_FEDERATION_TEST_DATABASE_URL")?)
        .await?;
    sqlx::raw_sql("
        CREATE TEMP TABLE furumusic__artist(id bigint primary key,name text,name_sort text,image_file_id bigint,is_hidden bool);
        CREATE TEMP TABLE furumusic__release(id bigint primary key,title text,release_type text,year int,cover_file_id bigint,is_hidden bool);
        CREATE TEMP TABLE furumusic__release_artist(release_id bigint, artist_id bigint,position int);
        CREATE TEMP TABLE furumusic__track(id bigint primary key,title text,release_id bigint,year int,track_number int,disc_number int,cover_file_id bigint,duration_seconds float8,audio_file_id bigint,is_hidden bool);
        CREATE TEMP TABLE furumusic__track_artist(track_id bigint,artist_id bigint,role text,position int);
        CREATE TEMP TABLE furumusic__media_file(id bigint primary key,sha256_hash text);
        CREATE TEMP TABLE furumusic__federation_content_id_cache(media_file_id bigint,sha256_hash text,content_id text);
        CREATE TEMP TABLE furumusic__track_ref(id bigserial primary key,content_id text unique,local_track_id bigint unique,title text,release_title text,year int,duration_seconds float8,metadata_json jsonb,metadata_authority text,created_at text,updated_at text);
        CREATE TEMP TABLE furumusic__federation_track_source(track_ref_id bigint,owner_peer_id text,item_id text,last_seen_ms bigint,metadata_json jsonb,primary key(owner_peer_id,item_id));
        INSERT INTO furumusic__artist VALUES (1,'Саша Скул','саша скул',5,false),(2,'Гнойный','гнойный',null,false),(3,'Саша-Скул','саша-скул',null,true);
        INSERT INTO furumusic__release VALUES (442,'Вино из цикуты','album',2014,22,false),(443,'Вино из цикуты','album',2016,88,false),(444,'Вино из цикуты','single',2014,88,true),(1364,'Рэп из-под стола (Remastered)','album',2013,23,false),(1365,'Рэп из-под стола','album',2013,null,false);
        INSERT INTO furumusic__release_artist VALUES (442,1,0),(443,1,0),(444,1,0),(1364,2,0),(1365,2,0);
        INSERT INTO furumusic__track VALUES (17,'Вино из цикуты',442,null,7,1,null,177,10,false),(18,'Hidden',444,null,1,1,4,80,11,false);
        INSERT INTO furumusic__media_file VALUES (10,'v1'),(11,'v2');
        INSERT INTO furumusic__track_artist VALUES (17,1,'main',0),(17,2,'featuring',1);
        INSERT INTO furumusic__federation_content_id_cache VALUES (10,'v1','b3:'||repeat('1',64)),(11,'v2','b3:'||repeat('2',64));
        CREATE TEMP TABLE furumusic__lyrics(audio_sha256 text);
        INSERT INTO furumusic__lyrics VALUES('v1');
        ALTER TABLE furumusic__media_file ADD uploader_name text, ADD audio_format text DEFAULT 'flac',
            ADD audio_bitrate integer DEFAULT 950, ADD audio_sample_rate integer, ADD audio_bit_depth integer, ADD file_size_bytes bigint;
        ALTER TABLE furumusic__track ADD lastfm_listeners bigint DEFAULT 65, ADD lastfm_playcount bigint,
            ADD lastfm_rating float8 DEFAULT 8.5, ADD lastfm_updated_at text;
    ").execute(&pool).await?;
    let album = item(ItemKind::Release, "Вино из цикуты", 1);
    let album2 = item(ItemKind::Release, "Вино из цикуты", 2);
    let track = LibraryItem {
        release_title: Some(album.name.clone()),
        content_id: Some(format!("b3:{}", "1".repeat(64))),
        ..item(ItemKind::Track, "Peer copy", 1)
    };
    let track2 = LibraryItem {
        owner: album2.owner,
        ..track.clone()
    };
    let hidden = LibraryItem {
        content_id: Some(format!("b3:{}", "2".repeat(64))),
        ..item(ItemKind::Track, "Из", 3)
    };
    let artist = item(ItemKind::Artist, "Саша Скул", 1);
    let items = vec![track.clone(), album, track2, album2, artist, hidden];
    let catalogue = LocalCatalog::for_items(&pool, &items).await?;
    assert_eq!(catalogue.artist("саша скул").unwrap().id, 1);
    let mut key = ReleaseKeyDto::new("Вино из цикуты", &["Саша Скул".into()], Some(2014), None);
    assert_eq!(catalogue.release(&key).unwrap().id, 442);
    key.year = None;
    assert!(catalogue.release(&key).is_none(), "two possible years");
    key.year = Some(2014);
    key.release_type = Some("single".into());
    assert!(catalogue.release(&key).is_none(), "hidden edition");
    let remaster = item(ItemKind::Release, "Рэп из-под стола (Remastered)", 3);
    let remaster = LibraryItem {
        artist_names: vec!["Гнойный".into()],
        year: Some(2013),
        ..remaster
    };
    let catalog = LocalCatalog::for_items(&pool, &[remaster]).await?;
    let key=ReleaseKeyDto::from_url(r#"{"normalized_title":"рэп из под стола remastered","primary_artists":["гнойный"],"release_type":null,"year":2013}"#).unwrap();
    assert_eq!(catalog.release(&key).unwrap().id, 1364);
    sqlx::query(
        "INSERT INTO furumusic__track_ref(content_id,local_track_id) VALUES('previous-audio',17)",
    )
    .execute(&pool)
    .await?;
    let events = build_search_events(&pool, "test", "Вино из цикуты", &items).await?;
    let release = &events
        .iter()
        .find(|e| e.kind == "federation.release")
        .unwrap()
        .entity;
    assert_eq!(release["local_id"], 442);
    assert_eq!(release["cover_url"], "/api/player/cover/22/medium");
    assert_eq!(release["sources"].as_array().unwrap().len(), 2);
    let hit = &events
        .iter()
        .find(|e| e.kind == "federation.track")
        .unwrap()
        .entity;
    assert_eq!(hit["availability"]["local"]["track_id"], 17);
    assert_eq!(hit["metadata"]["artists"][0]["local_id"], 1);
    assert_eq!(hit["metadata"]["featured_artists"][0]["local_id"], 2);
    assert_eq!(hit["metadata"]["release"]["local_id"], 442);
    assert_eq!(hit["metadata"]["cover_url"], release["cover_url"]);
    assert_eq!(hit["local_track"]["id"], 17);
    assert_eq!(hit["local_track"]["content_id"], hit["key"]["content_id"]);
    assert_eq!(hit["local_track"]["audio_format"], "flac");
    assert_eq!(hit["local_track"]["lastfm_listeners"], 65);
    assert_eq!(hit["local_track"]["has_lyrics"], true);
    assert_eq!(
        hit["availability"]["federation"].as_array().unwrap().len(),
        2
    );
    assert_eq!(events.len(), 2);
    let events = build_search_events(&pool, "test", "Саша Скул", &items).await?;
    let artist = &events
        .iter()
        .find(|e| e.kind == "federation.artist")
        .unwrap()
        .entity;
    assert_eq!(artist["local_id"], 1);
    assert_eq!(artist["image_url"], "/api/player/cover/5/medium");
    let hidden = &events
        .iter()
        .find(|e| e.entity["metadata"]["title"] == "Из")
        .unwrap()
        .entity;
    assert!(hidden["availability"]["local"].is_null());
    assert!(hidden.get("local_track").is_none());
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM furumusic__track_ref),(SELECT count(*) FROM furumusic__federation_track_source)").fetch_one(&pool).await?;
    assert_eq!(
        counts,
        (3, 3),
        "repeat search upserts the same source identities"
    );
    let previous: Option<i64> = sqlx::query_scalar(
        "SELECT local_track_id FROM furumusic__track_ref WHERE content_id='previous-audio'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(previous, None);
    Ok(())
}
