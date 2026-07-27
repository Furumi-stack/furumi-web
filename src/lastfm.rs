use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use md5::{Digest, Md5};
use reqwest::Client;
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::AppConfig;

const LASTFM_API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const MAX_BATCH_SIZE: i64 = 50;
const MAX_ATTEMPTS: i32 = 8;

#[derive(Debug, Clone)]
pub struct LastfmCredentials {
    api_key: String,
    shared_secret: String,
}

#[derive(Debug, Clone)]
pub struct LastfmSession {
    pub username: String,
    pub session_key: String,
}

#[derive(Debug, Clone)]
pub struct LastfmTrackPayload {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<i32>,
    pub duration_seconds: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct LastfmScrobblePayload {
    pub track: LastfmTrackPayload,
    pub timestamp: i64,
}

#[derive(Debug)]
pub struct LastfmApiError {
    pub code: Option<i32>,
    pub message: String,
}

impl std::fmt::Display for LastfmApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "Last.fm API error {code}: {}", self.message),
            None => write!(f, "Last.fm API error: {}", self.message),
        }
    }
}

impl std::error::Error for LastfmApiError {}

impl LastfmApiError {
    fn new(code: Option<i32>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn is_invalid_session(&self) -> bool {
        self.code == Some(9)
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self.code, Some(11 | 16 | 29) | None)
    }
}

#[derive(Debug, Default)]
pub struct ScrobbleProcessSummary {
    pub considered: u64,
    pub sent: u64,
    pub failed: u64,
    pub blocked: u64,
    pub skipped: u64,
}

pub fn is_configured(config: &AppConfig) -> bool {
    !config.lastfm_api_key.trim().is_empty() && !config.lastfm_shared_secret.trim().is_empty()
}

impl LastfmCredentials {
    pub fn from_config(config: &AppConfig) -> Option<Self> {
        let api_key = config.lastfm_api_key.trim();
        let shared_secret = config.lastfm_shared_secret.trim();
        if api_key.is_empty() || shared_secret.is_empty() {
            return None;
        }
        Some(Self {
            api_key: api_key.to_owned(),
            shared_secret: shared_secret.to_owned(),
        })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

pub struct LastfmClient {
    client: Client,
    credentials: LastfmCredentials,
}

impl LastfmClient {
    pub fn new(credentials: LastfmCredentials) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent(format!(
                "furumusic-lastfm-scrobbler/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(15))
            .build()
            .context("failed to build Last.fm HTTP client")?;
        Ok(Self {
            client,
            credentials,
        })
    }

    pub async fn get_session(&self, token: &str) -> Result<LastfmSession, LastfmApiError> {
        let params = self.signed_params(
            "auth.getSession",
            None,
            vec![("token".to_string(), token.to_string())],
        );
        let body = self.post(params).await?;
        let response: AuthSessionResponse = serde_json::from_str(&body)
            .map_err(|err| LastfmApiError::new(None, err.to_string()))?;
        if let Some(code) = response.error {
            return Err(LastfmApiError::new(
                Some(code),
                response
                    .message
                    .unwrap_or_else(|| "authentication failed".to_string()),
            ));
        }
        let Some(session) = response.session else {
            return Err(LastfmApiError::new(
                None,
                "Last.fm auth response did not include a session",
            ));
        };
        Ok(LastfmSession {
            username: session.name,
            session_key: session.key,
        })
    }

    pub async fn update_now_playing(
        &self,
        session_key: &str,
        track: &LastfmTrackPayload,
    ) -> Result<(), LastfmApiError> {
        let mut extra = vec![
            ("artist".to_string(), track.artist.clone()),
            ("track".to_string(), track.track.clone()),
        ];
        push_optional(&mut extra, "album", track.album.as_deref());
        push_optional(&mut extra, "albumArtist", track.album_artist.as_deref());
        push_optional_i32(&mut extra, "trackNumber", track.track_number);
        push_optional_i32(&mut extra, "duration", track.duration_seconds);

        let params = self.signed_params("track.updateNowPlaying", Some(session_key), extra);
        let body = self.post(params).await?;
        check_lastfm_error(&body)
    }

    pub async fn scrobble_batch(
        &self,
        session_key: &str,
        scrobbles: &[LastfmScrobblePayload],
    ) -> Result<(), LastfmApiError> {
        let mut extra = Vec::new();
        for (index, scrobble) in scrobbles.iter().take(MAX_BATCH_SIZE as usize).enumerate() {
            let suffix = format!("[{index}]");
            extra.push((format!("artist{suffix}"), scrobble.track.artist.clone()));
            extra.push((format!("track{suffix}"), scrobble.track.track.clone()));
            extra.push((format!("timestamp{suffix}"), scrobble.timestamp.to_string()));
            push_optional(
                &mut extra,
                &format!("album{suffix}"),
                scrobble.track.album.as_deref(),
            );
            push_optional(
                &mut extra,
                &format!("albumArtist{suffix}"),
                scrobble.track.album_artist.as_deref(),
            );
            push_optional_i32(
                &mut extra,
                &format!("trackNumber{suffix}"),
                scrobble.track.track_number,
            );
            push_optional_i32(
                &mut extra,
                &format!("duration{suffix}"),
                scrobble.track.duration_seconds,
            );
        }

        let params = self.signed_params("track.scrobble", Some(session_key), extra);
        let body = self.post(params).await?;
        check_lastfm_error(&body)
    }

    fn signed_params(
        &self,
        method: &str,
        session_key: Option<&str>,
        extra: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        let mut params = BTreeMap::new();
        params.insert("api_key".to_string(), self.credentials.api_key.clone());
        params.insert("method".to_string(), method.to_string());
        if let Some(session_key) = session_key {
            params.insert("sk".to_string(), session_key.to_string());
        }
        for (key, value) in extra {
            params.insert(key, value);
        }

        let mut signature_input = String::new();
        for (key, value) in &params {
            signature_input.push_str(key);
            signature_input.push_str(value);
        }
        signature_input.push_str(&self.credentials.shared_secret);
        let digest = Md5::digest(signature_input.as_bytes());
        let api_sig = format!("{digest:x}");

        let mut out = params.into_iter().collect::<Vec<_>>();
        out.push(("api_sig".to_string(), api_sig));
        out.push(("format".to_string(), "json".to_string()));
        out
    }

    async fn post(&self, params: Vec<(String, String)>) -> Result<String, LastfmApiError> {
        let response = self
            .client
            .post(LASTFM_API_URL)
            .form(&params)
            .send()
            .await
            .map_err(|err| LastfmApiError::new(None, err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| LastfmApiError::new(None, err.to_string()))?;
        if !status.is_success() {
            if let Some(err) = parse_error(&body) {
                return Err(err);
            }
            return Err(LastfmApiError::new(None, format!("HTTP {status}: {body}")));
        }
        Ok(body)
    }
}

pub async fn process_pending_scrobbles(
    pool: &PgPool,
    config: &AppConfig,
    user_id: Option<i64>,
    limit_per_user: i64,
) -> anyhow::Result<ScrobbleProcessSummary> {
    let Some(credentials) = LastfmCredentials::from_config(config) else {
        return Ok(ScrobbleProcessSummary::default());
    };
    let client = LastfmClient::new(credentials)?;
    let user_ids = pending_user_ids(pool, user_id).await?;
    let mut summary = ScrobbleProcessSummary::default();

    for uid in user_ids {
        let rows = fetch_pending_scrobbles(pool, uid, limit_per_user.min(MAX_BATCH_SIZE)).await?;
        if rows.is_empty() {
            continue;
        }
        summary.considered += rows.len() as u64;

        let mut ids = Vec::new();
        let mut attempt_rows = Vec::new();
        let mut payloads = Vec::new();
        for row in &rows {
            match row.payload() {
                Some(payload) => {
                    ids.push(row.id);
                    attempt_rows.push((row.id, row.attempt_count));
                    payloads.push(payload);
                }
                None => {
                    mark_row_failed(pool, row.id, "track has no primary Last.fm artist").await?;
                    summary.skipped += 1;
                }
            }
        }
        if ids.is_empty() {
            continue;
        }

        match client.scrobble_batch(&rows[0].session_key, &payloads).await {
            Ok(()) => {
                mark_rows_sent(pool, &ids).await?;
                clear_account_error(pool, uid).await?;
                summary.sent += ids.len() as u64;
            }
            Err(err) if err.is_invalid_session() => {
                mark_account_reauth_required(pool, uid, &err.to_string()).await?;
                mark_rows_blocked(pool, &ids, &err.to_string()).await?;
                summary.blocked += ids.len() as u64;
            }
            Err(err) => {
                mark_rows_retry_or_failed(pool, &attempt_rows, &err).await?;
                summary.failed += ids.len() as u64;
                if err.code == Some(29) {
                    break;
                }
            }
        }
    }

    Ok(summary)
}

#[derive(Debug, Deserialize)]
struct AuthSessionResponse {
    session: Option<AuthSession>,
    error: Option<i32>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthSession {
    name: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: Option<i32>,
    message: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingScrobbleRow {
    id: i64,
    started_at: i64,
    duration_seconds: i32,
    attempt_count: i32,
    session_key: String,
    title: String,
    artist_name: Option<String>,
    album_title: Option<String>,
    album_artist_name: Option<String>,
    track_number: Option<i32>,
}

impl PendingScrobbleRow {
    fn payload(&self) -> Option<LastfmScrobblePayload> {
        let artist = self
            .artist_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?
            .to_string();
        Some(LastfmScrobblePayload {
            track: LastfmTrackPayload {
                artist,
                track: self.title.clone(),
                album: non_empty(self.album_title.as_deref()),
                album_artist: non_empty(self.album_artist_name.as_deref()),
                track_number: self.track_number,
                duration_seconds: Some(self.duration_seconds),
            },
            timestamp: self.started_at,
        })
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn push_optional(params: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        params.push((key.to_string(), value.to_string()));
    }
}

fn push_optional_i32(params: &mut Vec<(String, String)>, key: &str, value: Option<i32>) {
    if let Some(value) = value {
        params.push((key.to_string(), value.to_string()));
    }
}

fn check_lastfm_error(body: &str) -> Result<(), LastfmApiError> {
    if let Some(err) = parse_error(body) {
        return Err(err);
    }
    Ok(())
}

fn parse_error(body: &str) -> Option<LastfmApiError> {
    let envelope: ErrorEnvelope = serde_json::from_str(body).ok()?;
    envelope
        .error
        .map(|code| LastfmApiError::new(Some(code), envelope.message.unwrap_or_default()))
}

async fn pending_user_ids(pool: &PgPool, user_id: Option<i64>) -> anyhow::Result<Vec<i64>> {
    if let Some(user_id) = user_id {
        return Ok(vec![user_id]);
    }
    let rows = sqlx::query_scalar::<_, i64>(
        r#"SELECT DISTINCT o.user_id
             FROM furumusic__lastfm_scrobble_outbox o
             JOIN furumusic__lastfm_account a ON a.user_id = o.user_id
            WHERE o.status IN ('pending', 'retry')
              AND a.reauth_required = false
            ORDER BY o.user_id"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn fetch_pending_scrobbles(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<PendingScrobbleRow>> {
    let rows = sqlx::query_as::<_, PendingScrobbleRow>(
        r#"SELECT o.id,
                  o.started_at,
                  o.duration_seconds,
                  o.attempt_count,
                  a.session_key::text AS session_key,
                  COALESCE(o.track_title, t.title::text) AS title,
                  COALESCE(o.album_title, r.title::text) AS album_title,
                  t.track_number,
                  COALESCE(o.artist_name, (
                    SELECT ar.name::text
                      FROM furumusic__track_artist ta
                      JOIN furumusic__artist ar ON ar.id = ta.artist_id
                     WHERE ta.track_id = t.id AND ta.role <> 'featuring'
                     ORDER BY ta.position
                     LIMIT 1
                  )) AS artist_name,
                  (
                    SELECT ar.name::text
                      FROM furumusic__release_artist ra
                      JOIN furumusic__artist ar ON ar.id = ra.artist_id
                     WHERE ra.release_id = r.id
                     ORDER BY ra.position
                     LIMIT 1
                  ) AS album_artist_name
             FROM furumusic__lastfm_scrobble_outbox o
             JOIN furumusic__lastfm_account a ON a.user_id = o.user_id
             LEFT JOIN furumusic__track t ON t.id = o.track_id
             LEFT JOIN furumusic__release r ON r.id = t.release_id
            WHERE o.user_id = $1
              AND o.status IN ('pending', 'retry')
              AND a.reauth_required = false
            ORDER BY o.created_at, o.id
            LIMIT $2"#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn clear_account_error(pool: &PgPool, user_id: i64) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__lastfm_account
              SET last_error = NULL,
                  reauth_required = false,
                  updated_at = $2
            WHERE user_id = $1"#,
    )
    .bind(user_id)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_account_reauth_required(
    pool: &PgPool,
    user_id: i64,
    error: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__lastfm_account
              SET reauth_required = true,
                  last_error = $2,
                  updated_at = $3
            WHERE user_id = $1"#,
    )
    .bind(user_id)
    .bind(error)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_rows_sent(pool: &PgPool, ids: &[i64]) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let now = now_iso();
    sqlx::query(
        r#"UPDATE furumusic__lastfm_scrobble_outbox
              SET status = 'sent',
                  updated_at = $2,
                  scrobbled_at = $2,
                  last_error = NULL
            WHERE id = ANY($1)"#,
    )
    .bind(ids)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_row_failed(pool: &PgPool, id: i64, error: &str) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__lastfm_scrobble_outbox
              SET status = 'failed',
                  attempt_count = attempt_count + 1,
                  last_error = $2,
                  updated_at = $3
            WHERE id = $1"#,
    )
    .bind(id)
    .bind(error)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_rows_blocked(pool: &PgPool, ids: &[i64], error: &str) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r#"UPDATE furumusic__lastfm_scrobble_outbox
              SET status = 'blocked',
                  attempt_count = attempt_count + 1,
                  last_error = $2,
                  updated_at = $3
            WHERE id = ANY($1)"#,
    )
    .bind(ids)
    .bind(error)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_rows_retry_or_failed(
    pool: &PgPool,
    rows: &[(i64, i32)],
    error: &LastfmApiError,
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    let next_status = if error.is_retryable()
        && rows
            .iter()
            .any(|(_, attempt_count)| attempt_count + 1 < MAX_ATTEMPTS)
    {
        "retry"
    } else {
        "failed"
    };
    sqlx::query(
        r#"UPDATE furumusic__lastfm_scrobble_outbox
              SET status = CASE
                    WHEN attempt_count + 1 >= $2 THEN 'failed'
                    ELSE $3
                  END,
                  attempt_count = attempt_count + 1,
                  last_error = $4,
                  updated_at = $5
            WHERE id = ANY($1)"#,
    )
    .bind(&ids)
    .bind(MAX_ATTEMPTS)
    .bind(next_status)
    .bind(error.to_string())
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
