use super::*;
use sqlx::Connection;

#[test]
fn run_options_default_to_light_and_require_explicit_full_mode() {
    assert!(!LastfmPopularityOptions::default().refresh_existing);
    let light: LastfmPopularityOptions = serde_json::from_str("{}").unwrap();
    assert!(!light.refresh_existing);
    let full: LastfmPopularityOptions =
        serde_json::from_str(r#"{"refresh_existing":true}"#).unwrap();
    assert!(full.refresh_existing);
    assert!(
        serde_json::from_str::<LastfmPopularityOptions>(r#"{"refresh_existing":"true"}"#).is_err()
    );
    assert!(
        serde_json::from_str::<LastfmPopularityOptions>(r#"{"refresh_exising":true}"#).is_err()
    );
}

#[test]
fn only_explicit_catalog_misses_are_cacheable() {
    for code in [6, 7] {
        for message in ["Track not found", "Track not found.", " track NOT found "] {
            let body = serde_json::json!({"error": code, "message": message}).to_string();
            assert_eq!(
                interpret_response(StatusCode::OK, body.as_bytes()).unwrap(),
                LookupOutcome::NotFound
            );
        }
    }
    for (code, message) in [
        (6, "Invalid parameters"),
        (6, "Track parameter is required"),
        (7, "Invalid resource specified"),
        (8, "Operation failed"),
        (11, "Service Offline"),
        (16, "Temporary error"),
        (29, "Track not found"),
    ] {
        let body = serde_json::json!({"error": code, "message": message}).to_string();
        assert!(interpret_response(StatusCode::OK, body.as_bytes()).is_err());
    }
    for status in [
        StatusCode::NOT_FOUND,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        // Even a JSON not-found message cannot turn an HTTP error into a miss.
        assert!(interpret_response(status, br#"{"error":6,"message":"Track not found"}"#).is_err());
    }
}

#[test]
fn invalid_or_incomplete_popularity_is_retryable_and_zero_is_valid() {
    for body in [
        "not json",
        "{}",
        r#"{"track":null}"#,
        r#"{"track":{}}"#,
        r#"{"track":{"listeners":"1"}}"#,
        r#"{"track":{"listeners":"bad","playcount":"2"}}"#,
        r#"{"track":{"listeners":"1","playcount":"-1"}}"#,
        r#"{"track":{"listeners":"1","playcount":"9223372036854775808"}}"#,
        r#"{"track":{"listeners":null,"playcount":"2"}}"#,
    ] {
        assert!(
            interpret_response(StatusCode::OK, body.as_bytes()).is_err(),
            "{body}"
        );
    }
    assert_eq!(
        interpret_response(
            StatusCode::OK,
            br#"{"track":{"listeners":"0","playcount":"0"}}"#
        )
        .unwrap(),
        LookupOutcome::Found {
            listeners: 0,
            playcount: 0
        }
    );
    assert_eq!(popularity_rating(0, 0), 0.0);
    assert!(popularity_rating(10, 100) > popularity_rating(1, 10));
}

#[test]
fn global_errors_stop_the_run_without_caching_absence() {
    assert!(LookupError::Http(StatusCode::TOO_MANY_REQUESTS).should_stop_run());
    for code in [10, 11, 26, 29] {
        assert!(
            LookupError::Api {
                code,
                message: String::new()
            }
            .should_stop_run()
        );
    }
    assert!(!LookupError::Http(StatusCode::INTERNAL_SERVER_ERROR).should_stop_run());
    assert!(!LookupError::InvalidResponse("missing track".into()).should_stop_run());
}

// These tests use temporary tables on isolated connections, never application
// tables. Run explicitly against a disposable PostgreSQL instance:
// FURUMUSIC_LASTFM_TEST_DATABASE_URL=... cargo test lastfm_popularity -- --include-ignored
async fn database_fixture() -> PgConnection {
    let url = std::env::var("FURUMUSIC_LASTFM_TEST_DATABASE_URL")
        .expect("set FURUMUSIC_LASTFM_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let mut conn = PgConnection::connect(&url).await.unwrap();
    sqlx::raw_sql(
        r#"
        CREATE TEMP TABLE furumusic__track (
            id BIGINT PRIMARY KEY, title TEXT NOT NULL,
            is_hidden BOOLEAN NOT NULL DEFAULT false,
            lastfm_listeners BIGINT, lastfm_playcount BIGINT,
            lastfm_rating DOUBLE PRECISION, lastfm_updated_at VARCHAR(32)
        );
        CREATE TEMP TABLE furumusic__artist (id BIGINT PRIMARY KEY, name TEXT NOT NULL);
        CREATE TEMP TABLE furumusic__track_artist (
            track_id BIGINT, artist_id BIGINT, role TEXT, position INTEGER
        );
        CREATE TEMP TABLE furumusic__track_popularity_history (
            id BIGSERIAL PRIMARY KEY, track_id BIGINT NOT NULL, source VARCHAR(32) NOT NULL,
            listeners BIGINT NOT NULL, playcount BIGINT NOT NULL,
            rating DOUBLE PRECISION NOT NULL, fetched_at VARCHAR(32) NOT NULL
        );
        INSERT INTO furumusic__artist VALUES (1, 'Artist'), (2, 'Guest'), (3, 'Other');
        INSERT INTO furumusic__track (id, title, lastfm_rating, lastfm_listeners, lastfm_playcount)
            VALUES (1, 'Legacy zero', 0, 0, 0), (2, 'New', NULL, NULL, NULL),
                   (3, 'Hidden', NULL, NULL, NULL), (4, 'No artist', NULL, NULL, NULL);
        UPDATE furumusic__track SET is_hidden = true WHERE id = 3;
        INSERT INTO furumusic__track_artist VALUES
            (1, 1, 'main', 0), (2, 1, 'main', 0), (3, 1, 'main', 0), (2, 2, 'featuring', -1);
    "#,
    )
    .execute(&mut conn)
    .await
    .unwrap();
    // Exercise precisely the same prepared statements as the real migration.
    for statement in include_str!("../music/lastfm_popularity_lookup.sql").split(';') {
        if !statement.trim().is_empty() {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
    }
    conn
}

async fn candidates(conn: &mut PgConnection, full: bool) -> Vec<TrackLookupRow> {
    fetch_candidates(
        conn,
        0,
        i64::MAX,
        LastfmPopularityOptions {
            refresh_existing: full,
        },
    )
    .await
    .unwrap()
}

fn ids(rows: &[TrackLookupRow]) -> Vec<i64> {
    rows.iter().map(|row| row.id).collect()
}

#[tokio::test]
#[ignore = "requires FURUMUSIC_LASTFM_TEST_DATABASE_URL"]
async fn database_modes_misses_retries_and_metadata_changes() {
    let mut conn = database_fixture().await;
    assert_eq!(ids(&candidates(&mut conn, false).await), [2, 4]);
    assert_eq!(ids(&candidates(&mut conn, true).await), [1, 2, 4]);
    let new = candidates(&mut conn, false).await.remove(0);
    assert_eq!(new.artist_name.as_deref(), Some("Artist"));
    assert!(
        save_outcome(&mut conn, &new, &LookupOutcome::NotFound)
            .await
            .unwrap()
    );
    assert_eq!(ids(&candidates(&mut conn, false).await), [4]);
    assert_eq!(ids(&candidates(&mut conn, true).await), [1, 2, 4]);
    // A failed full refresh must become eligible for light mode again.
    mark_pending(&mut conn, &[new.id]).await.unwrap();
    assert_eq!(ids(&candidates(&mut conn, false).await), [2, 4]);
    let found = LookupOutcome::Found {
        listeners: 5,
        playcount: 10,
    };
    assert!(save_outcome(&mut conn, &new, &found).await.unwrap());
    assert_eq!(ids(&candidates(&mut conn, false).await), [4]);
    // Unlike a network error, a confirmed miss replaces old current statistics.
    assert!(
        save_outcome(&mut conn, &new, &LookupOutcome::NotFound)
            .await
            .unwrap()
    );
    let rating: Option<f64> =
        sqlx::query_scalar("SELECT lastfm_rating FROM furumusic__track WHERE id=2")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(rating, None);
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__track_popularity_history")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(history, 1);
    sqlx::query("UPDATE furumusic__artist SET name='Renamed' WHERE id=1")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(ids(&candidates(&mut conn, false).await), [1, 2, 4]);
    // A response fetched before a concurrent rename cannot mark the new identity checked.
    assert!(!save_outcome(&mut conn, &new, &found).await.unwrap());
    let renamed = candidates(&mut conn, false).await.remove(1);
    assert!(save_outcome(&mut conn, &renamed, &found).await.unwrap());
    sqlx::query("UPDATE furumusic__track SET title='New title' WHERE id=2")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(ids(&candidates(&mut conn, false).await), [1, 2, 4]);
    assert!(!save_outcome(&mut conn, &renamed, &found).await.unwrap());
    let retitled = candidates(&mut conn, false).await.remove(1);
    assert!(save_outcome(&mut conn, &retitled, &found).await.unwrap());
    sqlx::query("UPDATE furumusic__track_artist SET artist_id=3 WHERE track_id=2 AND role='main'")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(ids(&candidates(&mut conn, false).await), [1, 2, 4]);
    assert!(!save_outcome(&mut conn, &retitled, &found).await.unwrap());
    let relinked = candidates(&mut conn, false).await.remove(1);
    assert_eq!(relinked.artist_name.as_deref(), Some("Other"));
    assert!(save_outcome(&mut conn, &relinked, &found).await.unwrap());
    sqlx::query("DELETE FROM furumusic__track_artist WHERE track_id=2 AND role='main'")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(candidates(&mut conn, false).await[1].artist_name, None);
}

#[tokio::test]
#[ignore = "requires FURUMUSIC_LASTFM_TEST_DATABASE_URL"]
async fn database_pagination_does_not_limit_runs_or_repeat_failures() {
    let mut conn = database_fixture().await;
    sqlx::query("INSERT INTO furumusic__track (id, title) SELECT n, 'Track ' || n FROM generate_series(5, 1105) n")
        .execute(&mut conn).await.unwrap();
    sqlx::query("INSERT INTO furumusic__track_artist SELECT id, 1, 'main', 0 FROM furumusic__track WHERE id>=5")
        .execute(&mut conn).await.unwrap();
    let mut after_id = 0;
    let mut seen = Vec::new();
    loop {
        let page = fetch_candidates(
            &mut conn,
            after_id,
            1105,
            LastfmPopularityOptions::default(),
        )
        .await
        .unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= TRACK_PAGE_SIZE as usize);
        for row in page {
            after_id = row.id;
            // Simulate failed requests: all candidates remain pending in the DB.
            mark_pending(&mut conn, &[row.id]).await.unwrap();
            seen.push(row.id);
        }
    }
    assert_eq!(seen.len(), 1103);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(seen.last(), Some(&1105));
    let bounded = fetch_candidates(&mut conn, 1000, 1010, LastfmPopularityOptions::default())
        .await
        .unwrap();
    assert_eq!(ids(&bounded), (1001..=1010).collect::<Vec<_>>());
}

#[tokio::test]
#[ignore = "requires FURUMUSIC_LASTFM_TEST_DATABASE_URL"]
async fn database_result_and_history_are_atomic() {
    let mut conn = database_fixture().await;
    let track = candidates(&mut conn, false).await.remove(0);
    sqlx::query("ALTER TABLE furumusic__track_popularity_history ADD CHECK (playcount > 100)")
        .execute(&mut conn)
        .await
        .unwrap();
    assert!(
        save_outcome(
            &mut conn,
            &track,
            &LookupOutcome::Found {
                listeners: 5,
                playcount: 10
            }
        )
        .await
        .is_err()
    );
    let state: (bool, Option<f64>) = sqlx::query_as(
        "SELECT lastfm_checked_at IS NULL, lastfm_rating FROM furumusic__track WHERE id=2",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(state, (true, None));
}

#[tokio::test]
#[ignore = "requires FURUMUSIC_LASTFM_TEST_DATABASE_URL"]
async fn database_interrupted_full_refresh_remains_pending_without_losing_statistics() {
    let mut conn = database_fixture().await;
    let page = candidates(&mut conn, true).await;
    let track_ids = ids(&page);
    mark_pending(&mut conn, &track_ids).await.unwrap();
    assert_eq!(track_ids, vec![1, 2, 4]);
    assert_eq!(ids(&candidates(&mut conn, false).await), track_ids);
    let state: (bool, Option<f64>) = sqlx::query_as(
        "SELECT lastfm_checked_at IS NULL, lastfm_rating FROM furumusic__track WHERE id=1",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(state, (true, Some(0.0)));
}

#[tokio::test]
#[ignore = "requires FURUMUSIC_LASTFM_TEST_DATABASE_URL"]
async fn database_lock_is_shared_by_light_and_full_runs() {
    let url = std::env::var("FURUMUSIC_LASTFM_TEST_DATABASE_URL").unwrap();
    let mut first = PgConnection::connect(&url).await.unwrap();
    let mut second = PgConnection::connect(&url).await.unwrap();
    assert!(try_lock(&mut first).await.unwrap());
    assert!(!try_lock(&mut second).await.unwrap());
    first.close().await.unwrap();
    assert!(try_lock(&mut second).await.unwrap());
}
