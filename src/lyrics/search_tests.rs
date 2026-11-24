use super::*;
use sqlx::Connection;

#[test]
#[ignore = "requires cached E5 and the fixture prepared by tests/lyrics_rank_corpus.py"]
fn lyrics_ranking_quality() {
    use std::collections::BTreeMap;
    #[derive(serde::Deserialize)]
    struct Example {
        text: String,
        queries: BTreeMap<String, String>,
    }
    let path = std::env::var("FURU_LYRICS_RANK_EVAL").unwrap();
    let examples: Vec<Example> =
        serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    let labeled = examples
        .iter()
        .filter(|example| !example.queries.is_empty())
        .count();
    assert!(labeled > 0);
    let mut corpus = Corpus::default();
    let mut passages = BTreeMap::new();
    let mut queries = Vec::new();
    for example in examples {
        let hash = blake3::hash(example.text.as_bytes()).to_hex().to_string();
        let chunks = chunks(&example.text);
        for chunk in &chunks {
            passages
                .entry(chunk.hash.clone())
                .or_insert_with(|| format!("passage: {}", chunk.tokens.join(" ")));
        }
        for (kind, query) in example.queries {
            queries.push((kind, query.to_lowercase(), hash.clone()));
        }
        corpus.docs.insert(
            hash.clone(),
            Arc::new(Document {
                hash,
                id: "original".into(),
                stamp: String::new(),
                chunks,
            }),
        );
    }

    let mut model = load_model(&crate::lyrics::model_files::directory()).unwrap();
    let (hashes, inputs): (Vec<_>, Vec<_>) = passages.into_iter().unzip();
    println!(
        "{} documents, {} chunks, {} queries",
        corpus.docs.len(),
        inputs.len(),
        queries.len()
    );
    let vectors = model.embed(inputs, Some(BATCH)).unwrap();
    corpus
        .vectors
        .write()
        .unwrap()
        .extend(hashes.into_iter().zip(vectors.into_iter().map(Arc::new)));
    let inputs: Vec<_> = queries
        .iter()
        .map(|(_, query, _)| format!("query: {query}"))
        .collect();
    let vectors = model.embed(inputs, Some(BATCH)).unwrap();
    let mut positions = BTreeMap::<String, Vec<usize>>::new();
    for ((kind, query, expected), vector) in queries.into_iter().zip(vectors) {
        let hits = rank(&corpus, &query, Some(&vector));
        let position = hits
            .iter()
            .position(|hit| hit.hash == expected)
            .map_or(0, |index| index + 1);
        positions.entry(kind).or_default().push(position);
    }
    assert_eq!(positions.len(), 4);
    for (kind, ranks) in positions {
        assert_eq!(ranks.len(), labeled);
        let top1 = ranks.iter().filter(|&&r| r == 1).count();
        let top3 = ranks.iter().filter(|&&r| (1..=3).contains(&r)).count();
        println!(
            "{kind}: top1={top1}/{labeled} top3={top3}/{labeled} ranks={ranks:?} (0 = missing)"
        );
        if kind.ends_with("_quote") {
            assert_eq!(top1, labeled, "verbatim quotes should rank first in {kind}");
        }
    }
}

fn manager() -> Arc<Manager> {
    Arc::new(Manager::default())
}

#[tokio::test]
async fn search_during_model_preparation_falls_back_without_opening_a_model() {
    let manager = manager();
    let error = manager.embed(vec!["query: snow".into()]).await.unwrap_err();
    assert!(error.to_string().contains("not ready"));
    assert!(manager.model_dir.get().is_none());
}

#[test]
fn delayed_database_setting_cannot_restart_an_index_disabled_by_the_admin() {
    let manager = manager();
    let snapshot = manager.enabled.subscribe();
    manager.set_enabled(false, None);
    assert!(!manager.set_enabled(true, Some(&snapshot)));
    assert!(!manager.status().enabled);
    assert_eq!(manager.status().phase, "paused");
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL and cached E5 model"]
async fn postgres_index_restart_edits_visibility_and_translations() {
    let url = std::env::var("FURU_LYRICS_TEST_DATABASE_URL").unwrap();
    let mut admin = sqlx::PgConnection::connect(&url).await.unwrap();
    let schema = format!(
        "lyrics_search_test_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    );
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let search_path = format!("SET search_path TO {schema}");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let sql = search_path.clone();
            Box::pin(async move {
                sqlx::query(&sql).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE furumusic__config_entry(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO furumusic__config_entry VALUES ('lyrics_indexing_enabled', 'false');
        CREATE TABLE furumusic__release(id BIGINT PRIMARY KEY,is_hidden BOOLEAN NOT NULL DEFAULT false);
        CREATE TABLE furumusic__media_file(id BIGINT PRIMARY KEY,sha256_hash TEXT NOT NULL);
        CREATE TABLE furumusic__track(id BIGINT PRIMARY KEY,release_id BIGINT,audio_file_id BIGINT,is_hidden BOOLEAN NOT NULL DEFAULT false);
        INSERT INTO furumusic__release(id) VALUES(1);
        INSERT INTO furumusic__media_file VALUES(1,'first'),(2,'other');
        INSERT INTO furumusic__track(id,release_id,audio_file_id) VALUES(1,1,1),(2,1,2),(3,1,1);").execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!("schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("search_schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let first = manager();
    let mut settings = first.enabled.subscribe();
    let config = crate::config::AppConfig::default();
    first
        .worker_step(&pool, &config, &mut settings)
        .await
        .unwrap();
    assert!(
        first.model_dir.get().is_none(),
        "honour a persisted pause at startup"
    );
    sqlx::query(
        "UPDATE furumusic__config_entry SET value='true' WHERE key='lyrics_indexing_enabled'",
    )
    .execute(&pool)
    .await
    .unwrap();
    first
        .worker_step(&pool, &config, &mut settings)
        .await
        .unwrap();
    assert!(
        first.model_dir.get().is_some(),
        "prepare the model even with no lyrics yet"
    );
    for (hash, id, plain, lrc) in [
        (
            "first",
            "original",
            Some("Просыпется снег на могилы, и больше не будет весны."),
            None,
        ),
        (
            "first",
            "timed",
            None,
            Some("[00:01]Просыпется снег на могилы, и больше не будет весны."),
        ),
        (
            "first",
            "translation",
            Some("The snow will fall on graves, and spring will never return."),
            None,
        ),
        (
            "other",
            "original",
            Some("Summer sunshine by the ocean, we are dancing in the sand."),
            None,
        ),
    ] {
        sqlx::query("INSERT INTO furumusic__lyrics(audio_sha256,id,label,source,plain_text,synced_lrc) VALUES($1,$2,$2,'manual',$3,$4)").bind(hash).bind(id).bind(plain).bind(lrc).execute(&pool).await.unwrap();
    }
    first.refresh(&pool, true).await.unwrap();
    let lexical = first
        .search(&pool, "просыпется снег на могилы", 10)
        .await
        .unwrap();
    assert_eq!(lexical.hits[0].track_id, 1);
    assert_eq!(
        lexical.hits.len(),
        2,
        "two audio-bound track identities, one hit each"
    );
    assert_eq!(
        first.status().texts,
        3,
        "plain and timed text share one document recipe"
    );
    assert_eq!(
        first.status().ready_chunks,
        0,
        "direct matches work before inference"
    );
    while !first.pending.lock().unwrap().is_empty() {
        first.index_batch(&pool).await.unwrap();
    }
    let ready = first.status().ready_chunks;
    assert!(ready > 0);
    assert_eq!(first.status().chunks, ready);
    assert!(first.pending.lock().unwrap().is_empty());
    let restarted = manager();
    restarted.refresh(&pool, true).await.unwrap();
    assert_eq!(restarted.status().ready_chunks, ready);
    assert!(
        restarted.pending.lock().unwrap().is_empty(),
        "restart does not schedule cached fragments"
    );
    restarted.enabled.send_replace(false);
    let semantic = restarted
        .search(&pool, "snow covering a cemetery, spring never comes", 10)
        .await
        .unwrap();
    assert!(semantic.semantic_error.is_none());
    assert_eq!(semantic.hits[0].track_id, 1);
    let translation = restarted
        .search(&pool, "spring will never return", 10)
        .await
        .unwrap();
    assert_eq!(translation.hits[0].version_id, "translation");
    sqlx::query("UPDATE furumusic__lyrics SET plain_text='Completely replaced text',synced_lrc=NULL,updated_at=now() WHERE audio_sha256='first'").execute(&pool).await.unwrap();
    let stale = restarted
        .search(&pool, "просыпется снег на могилы", 10)
        .await
        .unwrap();
    assert!(
        stale.hits.iter().all(|h| h.track_id == 2),
        "old snapshot cannot return edited versions"
    );
    restarted.refresh(&pool, true).await.unwrap();
    assert!(
        restarted.status().ready_chunks < restarted.status().chunks,
        "edited text needs new vectors"
    );
    sqlx::query("UPDATE furumusic__track SET is_hidden=true WHERE id IN (1,3)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        restarted
            .search(&pool, "Completely replaced text", 10)
            .await
            .unwrap()
            .hits
            .iter()
            .all(|h| h.track_id == 2)
    );
    sqlx::query("UPDATE furumusic__release SET is_hidden=true")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        restarted
            .search(&pool, "summer", 10)
            .await
            .unwrap()
            .hits
            .is_empty()
    );
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__lyrics_embedding")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored, ready as i64,
        "pausing and editing retain cached embeddings"
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut admin)
        .await
        .unwrap();
}
