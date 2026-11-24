use chrono::{Duration, Utc};
use cot::Body;
use cot::db::{Auto, Database, LimitedString, Model};
use cot::http::header::AUTHORIZATION;
use cot::request::RequestHead;
use cot::request::extractors::FromRequestHead;
use cot::response::IntoResponse;
use cot::session::Session;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::user::User;

// ---------------------------------------------------------------------------
// Role enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn code(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Session-based auth
// ---------------------------------------------------------------------------

const SESSION_USER_ID: &str = "user_id";
const SESSION_POST_LOGIN_REDIRECT: &str = "post_login_redirect";

#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub id: i64,
    pub name: String,
    pub role: Role,
}

fn authenticated_user_from_user(user: User) -> Option<AuthenticatedUser> {
    if !user.is_active() {
        return None;
    }
    let name = {
        let display = user.display_name_str();
        if display.is_empty() {
            user.username_str().to_owned()
        } else {
            display
        }
    };
    crate::metrics::record_active_user(user.id_val());
    Some(AuthenticatedUser {
        id: user.id_val(),
        name,
        role: user.role(),
    })
}

/// Return the signed-in identity, including an account awaiting approval.
pub async fn get_session_identity(session: &Session, db: &Database) -> Option<User> {
    checked_session_identity(session, db).await.ok().flatten()
}

/// Offline-capable routes must distinguish an expired login from a storage outage.
pub(crate) async fn checked_session_identity(
    session: &Session,
    db: &Database,
) -> cot::Result<Option<User>> {
    let Some(user_id) = session
        .get::<i64>(SESSION_USER_ID)
        .await
        .map_err(cot::Error::internal)?
    else {
        return Ok(None);
    };
    Ok(User::get_by_id(db, user_id).await?)
}

/// Return a session user only when access to the instance has been granted.
pub async fn get_session_user(session: &Session, db: &Database) -> Option<AuthenticatedUser> {
    get_session_identity(session, db)
        .await
        .and_then(authenticated_user_from_user)
}

// ---------------------------------------------------------------------------
// API bearer-token auth
// ---------------------------------------------------------------------------

const ACCESS_TOKEN_PREFIX: &str = "furu_at_";
const REFRESH_TOKEN_PREFIX: &str = "furu_rt_";
const MOBILE_EXCHANGE_CODE_PREFIX: &str = "furu_mx_";
const ACCESS_TOKEN_TTL_MINUTES: i64 = 15;
const REFRESH_TOKEN_TTL_DAYS: i64 = 60;
const MOBILE_EXCHANGE_CODE_TTL_MINUTES: i64 = 3;

#[derive(Debug, Clone, Default)]
pub struct AuthContext {
    bearer_token: Option<String>,
}

impl AuthContext {
    pub fn bearer_token(&self) -> Option<&str> {
        self.bearer_token.as_deref()
    }
}

impl FromRequestHead for AuthContext {
    async fn from_request_head(head: &RequestHead) -> cot::Result<Self> {
        let bearer_token = head
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_bearer_token)
            .map(str::to_owned);
        Ok(Self { bearer_token })
    }
}

fn parse_bearer_token(header: &str) -> Option<&str> {
    let header = header.trim();
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.len() > 512 {
        return None;
    }
    Some(token)
}

#[derive(Debug, Serialize)]
pub struct ApiTokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in_seconds: i64,
}

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct ApiSession {
    #[model(primary_key)]
    id: Auto<i64>,
    user_id: i64,
    device_name: Option<String>,
    access_token_hash: LimitedString<128>,
    refresh_token_hash: LimitedString<128>,
    access_expires_at: String,
    refresh_expires_at: String,
    created_at: String,
    last_used_at: Option<String>,
    revoked_at: Option<String>,
}

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct MobileExchangeCode {
    #[model(primary_key)]
    id: Auto<i64>,
    code_hash: LimitedString<128>,
    user_id: i64,
    created_at: String,
    expires_at: String,
    consumed_at: Option<String>,
}

impl ApiSession {
    pub async fn create_for_user(
        db: &Database,
        user_id: i64,
        device_name: Option<&str>,
    ) -> cot::db::Result<ApiTokenPair> {
        let tokens = fresh_token_pair();
        let now = now_iso();
        let mut session = Self {
            id: Auto::auto(),
            user_id,
            device_name: device_name.and_then(normalize_device_name),
            access_token_hash: LimitedString::new(&token_hash(&tokens.access_token)).unwrap(),
            refresh_token_hash: LimitedString::new(&token_hash(&tokens.refresh_token)).unwrap(),
            access_expires_at: access_expires_at(),
            refresh_expires_at: refresh_expires_at(),
            created_at: now.clone(),
            last_used_at: Some(now),
            revoked_at: None,
        };
        session.insert(db).await?;
        Ok(tokens)
    }

    async fn find_by_access_token(db: &Database, token: &str) -> cot::db::Result<Option<Self>> {
        let Ok(hash) = LimitedString::<128>::new(&token_hash(token)) else {
            return Ok(None);
        };
        cot::db::query!(ApiSession, $access_token_hash == hash)
            .get(db)
            .await
    }

    async fn find_by_refresh_token(db: &Database, token: &str) -> cot::db::Result<Option<Self>> {
        let Ok(hash) = LimitedString::<128>::new(&token_hash(token)) else {
            return Ok(None);
        };
        cot::db::query!(ApiSession, $refresh_token_hash == hash)
            .get(db)
            .await
    }

    fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    fn access_token_valid(&self) -> bool {
        !self.is_revoked() && self.access_expires_at > now_iso()
    }

    fn refresh_token_valid(&self) -> bool {
        !self.is_revoked() && self.refresh_expires_at > now_iso()
    }

    async fn rotate(&mut self, db: &Database) -> cot::db::Result<ApiTokenPair> {
        let tokens = fresh_token_pair();
        self.access_token_hash = LimitedString::new(&token_hash(&tokens.access_token)).unwrap();
        self.refresh_token_hash = LimitedString::new(&token_hash(&tokens.refresh_token)).unwrap();
        self.access_expires_at = access_expires_at();
        self.refresh_expires_at = refresh_expires_at();
        self.last_used_at = Some(now_iso());
        self.save(db).await?;
        Ok(tokens)
    }

    async fn revoke(&mut self, db: &Database) -> cot::db::Result<()> {
        if self.revoked_at.is_none() {
            self.revoked_at = Some(now_iso());
            self.save(db).await?;
        }
        Ok(())
    }
}

pub async fn create_api_session(
    db: &Database,
    user_id: i64,
    device_name: Option<&str>,
) -> cot::db::Result<ApiTokenPair> {
    ApiSession::create_for_user(db, user_id, device_name).await
}

pub async fn get_bearer_user(db: &Database, token: &str) -> Option<AuthenticatedUser> {
    let session = ApiSession::find_by_access_token(db, token).await.ok()??;
    if !session.access_token_valid() {
        return None;
    }
    let user = User::get_by_id(db, session.user_id).await.ok()??;
    authenticated_user_from_user(user)
}

pub async fn get_request_user(
    auth: &AuthContext,
    session: &Session,
    db: &Database,
) -> Option<AuthenticatedUser> {
    if let Some(token) = auth.bearer_token() {
        return get_bearer_user(db, token).await;
    }
    get_session_user(session, db).await
}

pub async fn refresh_api_session(
    db: &Database,
    refresh_token: &str,
) -> cot::db::Result<Option<ApiTokenPair>> {
    let Some(mut session) = ApiSession::find_by_refresh_token(db, refresh_token).await? else {
        return Ok(None);
    };
    if !session.refresh_token_valid() {
        session.revoke(db).await?;
        return Ok(None);
    }
    let Some(user) = User::get_by_id(db, session.user_id).await? else {
        session.revoke(db).await?;
        return Ok(None);
    };
    if !user.is_active() {
        session.revoke(db).await?;
        return Ok(None);
    }
    Ok(Some(session.rotate(db).await?))
}

pub async fn revoke_api_session(
    db: &Database,
    access_token: Option<&str>,
    refresh_token: Option<&str>,
) -> cot::db::Result<bool> {
    let mut session = if let Some(token) = access_token {
        ApiSession::find_by_access_token(db, token).await?
    } else {
        None
    };
    if session.is_none() {
        if let Some(token) = refresh_token {
            session = ApiSession::find_by_refresh_token(db, token).await?;
        }
    }
    let Some(mut session) = session else {
        return Ok(false);
    };
    session.revoke(db).await?;
    Ok(true)
}

impl MobileExchangeCode {
    pub async fn create_for_user(db: &Database, user_id: i64) -> cot::db::Result<String> {
        let code = random_token(MOBILE_EXCHANGE_CODE_PREFIX);
        let now = now_iso();
        let mut row = Self {
            id: Auto::auto(),
            code_hash: LimitedString::new(&token_hash(&code)).unwrap(),
            user_id,
            created_at: now,
            expires_at: mobile_exchange_code_expires_at(),
            consumed_at: None,
        };
        row.insert(db).await?;
        Ok(code)
    }

    async fn find_by_code(db: &Database, code: &str) -> cot::db::Result<Option<Self>> {
        let Ok(hash) = LimitedString::<128>::new(&token_hash(code)) else {
            return Ok(None);
        };
        cot::db::query!(MobileExchangeCode, $code_hash == hash)
            .get(db)
            .await
    }

    fn is_valid(&self) -> bool {
        self.consumed_at.is_none() && self.expires_at > now_iso()
    }

    async fn consume(&mut self, db: &Database) -> cot::db::Result<()> {
        self.consumed_at = Some(now_iso());
        self.save(db).await
    }
}

pub async fn create_mobile_exchange_code(db: &Database, user_id: i64) -> cot::db::Result<String> {
    MobileExchangeCode::create_for_user(db, user_id).await
}

pub async fn exchange_mobile_code_for_api_session(
    db: &Database,
    code: &str,
    device_name: Option<&str>,
) -> cot::db::Result<Option<(AuthenticatedUser, ApiTokenPair)>> {
    let Some(mut exchange_code) = MobileExchangeCode::find_by_code(db, code).await? else {
        return Ok(None);
    };
    if !exchange_code.is_valid() {
        return Ok(None);
    }
    let Some(user) = User::get_by_id(db, exchange_code.user_id).await? else {
        exchange_code.consume(db).await?;
        return Ok(None);
    };
    let Some(auth_user) = authenticated_user_from_user(user) else {
        exchange_code.consume(db).await?;
        return Ok(None);
    };
    exchange_code.consume(db).await?;
    let tokens = ApiSession::create_for_user(db, auth_user.id, device_name).await?;
    Ok(Some((auth_user, tokens)))
}

fn fresh_token_pair() -> ApiTokenPair {
    ApiTokenPair {
        access_token: random_token(ACCESS_TOKEN_PREFIX),
        refresh_token: random_token(REFRESH_TOKEN_PREFIX),
        token_type: "Bearer",
        expires_in_seconds: ACCESS_TOKEN_TTL_MINUTES * 60,
    }
}

fn random_token(prefix: &str) -> String {
    format!(
        "{prefix}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn normalize_device_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(255).collect())
}

fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn access_expires_at() -> String {
    (Utc::now() + Duration::minutes(ACCESS_TOKEN_TTL_MINUTES))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn refresh_expires_at() -> String {
    (Utc::now() + Duration::days(REFRESH_TOKEN_TTL_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn mobile_exchange_code_expires_at() -> String {
    (Utc::now() + Duration::minutes(MOBILE_EXCHANGE_CODE_TTL_MINUTES))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Return `Ok(user)` if the session belongs to an active admin, otherwise
/// `Err(response)` — a redirect to `/login` or a 403.
pub async fn require_admin_or_redirect(
    session: &Session,
    db: &Database,
) -> Result<AuthenticatedUser, cot::response::Response> {
    let Some(user) = get_session_user(session, db).await else {
        crate::metrics::record_authorization_denied("unauthenticated");
        return Err(redirect("/login"));
    };
    if user.role != Role::Admin {
        crate::metrics::record_authorization_denied("forbidden");
        return Err("Forbidden"
            .with_status(cot::http::StatusCode::FORBIDDEN)
            .into_response()
            .expect("valid response"));
    }
    Ok(user)
}

/// Insert user_id into the session and cycle the session ID.
pub async fn login(session: &Session, user_id: i64) -> cot::Result<()> {
    session
        .cycle_id()
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_USER_ID, user_id)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    crate::metrics::record_active_user(user_id);
    Ok(())
}

pub async fn remember_post_login_redirect(session: &Session, location: &str) -> cot::Result<()> {
    if let Some(location) = safe_internal_redirect(location) {
        session
            .insert(SESSION_POST_LOGIN_REDIRECT, location)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    Ok(())
}

pub async fn get_post_login_redirect(session: &Session) -> cot::Result<Option<String>> {
    let location: Option<String> = session
        .get(SESSION_POST_LOGIN_REDIRECT)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(location.and_then(|value| safe_internal_redirect(&value)))
}

pub async fn clear_post_login_redirect(session: &Session) -> cot::Result<()> {
    let _: Option<String> = session
        .remove(SESSION_POST_LOGIN_REDIRECT)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(())
}

fn safe_internal_redirect(location: &str) -> Option<String> {
    let location = location.trim();
    if !location.starts_with('/') || location.starts_with("//") {
        return None;
    }
    if location.bytes().any(|b| matches!(b, b'\r' | b'\n')) {
        return None;
    }
    Some(location.chars().take(2048).collect())
}

/// Flush (destroy) the session.
pub async fn logout(session: &Session) -> cot::Result<()> {
    session
        .flush()
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(())
}

/// Build a 303 See Other redirect response.
pub fn redirect(location: &str) -> cot::response::Response {
    cot::http::Response::builder()
        .status(cot::http::StatusCode::SEE_OTHER)
        .header(cot::http::header::LOCATION, location)
        .body(Body::fixed(""))
        .expect("valid response")
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

pub mod db_migrations {
    use cot::db::migrations::{self, Field, Operation, SyncDynMigration};
    use cot::db::{DatabaseField, Identifier, LimitedString};

    #[derive(Debug, Copy, Clone)]
    pub struct M0038CreateApiSession;

    impl migrations::Migration for M0038CreateApiSession {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0038_create_api_session";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0003_create_user",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__api_session"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("device_name"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("access_token_hash"),
                    <LimitedString<128> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("refresh_token_hash"),
                    <LimitedString<128> as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("access_expires_at"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("refresh_expires_at"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("created_at"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("last_used_at"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
                Field::new(
                    Identifier::new("revoked_at"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
            ])
            .build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_api_session_indexes(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX idx_api_session_access_token_hash \
                     ON furumusic__api_session (access_token_hash)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX idx_api_session_refresh_token_hash \
                     ON furumusic__api_session (refresh_token_hash)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX idx_api_session_user_id \
                     ON furumusic__api_session (user_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0039CreateApiSessionIndexes;

    impl migrations::Migration for M0039CreateApiSessionIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0039_create_api_session_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0038_create_api_session",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_api_session_indexes).build()];
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0040CreateMobileExchangeCode;

    impl migrations::Migration for M0040CreateMobileExchangeCode {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0040_create_mobile_exchange_code";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0039_create_api_session_indexes",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furumusic__mobile_exchange_code"))
            .fields(&[
                Field::new(Identifier::new("id"), <i64 as DatabaseField>::TYPE)
                    .primary_key()
                    .auto(),
                Field::new(
                    Identifier::new("code_hash"),
                    <LimitedString<128> as DatabaseField>::TYPE,
                ),
                Field::new(Identifier::new("user_id"), <i64 as DatabaseField>::TYPE),
                Field::new(
                    Identifier::new("created_at"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("expires_at"),
                    <String as DatabaseField>::TYPE,
                ),
                Field::new(
                    Identifier::new("consumed_at"),
                    <String as DatabaseField>::TYPE,
                )
                .set_null(true),
            ])
            .build()];
    }

    #[cot::db::migrations::migration_op]
    async fn create_mobile_exchange_code_indexes(
        ctx: migrations::MigrationContext<'_>,
    ) -> cot::db::Result<()> {
        ctx.db
            .raw(
                "CREATE UNIQUE INDEX idx_mobile_exchange_code_hash \
                     ON furumusic__mobile_exchange_code (code_hash)",
            )
            .await?;
        ctx.db
            .raw(
                "CREATE INDEX idx_mobile_exchange_code_user_id \
                     ON furumusic__mobile_exchange_code (user_id)",
            )
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0041CreateMobileExchangeCodeIndexes;

    impl migrations::Migration for M0041CreateMobileExchangeCodeIndexes {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0041_create_mobile_exchange_code_indexes";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0040_create_mobile_exchange_code",
            )];
        const OPERATIONS: &'static [Operation] =
            &[Operation::custom(create_mobile_exchange_code_indexes).build()];
    }

    pub const MIGRATIONS: &[&SyncDynMigration] = &[
        &M0038CreateApiSession,
        &M0039CreateApiSessionIndexes,
        &M0040CreateMobileExchangeCode,
        &M0041CreateMobileExchangeCodeIndexes,
    ];
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_roundtrip() {
        assert_eq!(Role::from_code("admin"), Some(Role::Admin));
        assert_eq!(Role::from_code("user"), Some(Role::User));
        assert_eq!(Role::from_code("other"), None);
        assert_eq!(Role::Admin.code(), "admin");
        assert_eq!(Role::User.code(), "user");
    }
}
