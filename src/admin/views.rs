use cot::db::{Database, Model};
use cot::form::{Form, FormResult};
use cot::html::Html;
use cot::request::extractors::RequestForm;
use cot::response::IntoResponse;
use cot::session::Session;
use cot::{Body, Template};

use std::collections::HashMap;
use std::sync::Arc;

use super::BUILD_INFO;
use crate::agent;
use crate::auth::{self, AuthenticatedUser};
use crate::config::{AppConfig, ConfigEntry, ConfigSources};
use crate::i18n::{I18n, Translations};
use crate::music::{Artist, MediaFile, RELEASE_TYPES, Release, ReleaseArtist, Track, TrackArtist};
use crate::scheduler::{self, JobRegistry, JobRun, PendingReview, ScheduledJob};
use crate::user::User;

use crate::agent::AgentProbeResult;

/// A config entry for display in the unified debug table.
#[derive(Debug)]
pub struct ConfigDisplayEntry {
    pub key: String,
    pub env_var: String,
    pub value: String,
    pub default_value: String,
    pub source: &'static str,
}

/// Secret field names that should be redacted in the debug view.
const SECRET_FIELDS: &[&str] = &["database_url", "oidc_client_secret"];

fn is_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_FIELDS.iter().any(|s| lower.contains(s))
        || lower.contains("secret")
        || lower.contains("token")
}

fn redact(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        "********".into()
    }
}

#[derive(Debug, Template)]
#[template(path = "admin/debug.html")]
struct DebugTemplate<'a> {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    build: &'a super::BuildInfo,
    config_entries: Vec<ConfigDisplayEntry>,
    db_status: String,
}

fn config_display_entries(config: &AppConfig, sources: &ConfigSources) -> Vec<ConfigDisplayEntry> {
    let defaults = AppConfig::default();

    macro_rules! entry {
        ($field:ident, $value:expr, $default:expr) => {{
            let raw = $value;
            let default_raw = $default;
            let secret = is_secret(stringify!($field));
            let display = if secret { redact(&raw) } else { raw };
            let default_display = if secret {
                redact(&default_raw)
            } else {
                default_raw
            };
            ConfigDisplayEntry {
                key: stringify!($field).into(),
                env_var: format!("FURU_{}", stringify!($field).to_ascii_uppercase()),
                value: display,
                default_value: default_display,
                source: sources.$field.code(),
            }
        }};
    }

    vec![
        entry!(
            database_url,
            config.database_url.clone(),
            defaults.database_url.clone()
        ),
        entry!(
            oidc_issuer,
            config.oidc_issuer.clone(),
            defaults.oidc_issuer.clone()
        ),
        entry!(
            oidc_client_id,
            config.oidc_client_id.clone(),
            defaults.oidc_client_id.clone()
        ),
        entry!(
            oidc_client_secret,
            config.oidc_client_secret.clone(),
            defaults.oidc_client_secret.clone()
        ),
        entry!(
            log_level,
            config.log_level.clone(),
            defaults.log_level.clone()
        ),
        entry!(
            auth_password_enabled,
            config.auth_password_enabled.to_string(),
            defaults.auth_password_enabled.to_string()
        ),
        entry!(
            auth_sso_enabled,
            config.auth_sso_enabled.to_string(),
            defaults.auth_sso_enabled.to_string()
        ),
        entry!(
            oidc_button_text,
            config.oidc_button_text.clone(),
            defaults.oidc_button_text.clone()
        ),
        entry!(
            oidc_admin_groups,
            config.oidc_admin_groups.clone(),
            defaults.oidc_admin_groups.clone()
        ),
        entry!(
            oidc_user_groups,
            config.oidc_user_groups.clone(),
            defaults.oidc_user_groups.clone()
        ),
        entry!(
            swagger_enabled,
            config.swagger_enabled.to_string(),
            defaults.swagger_enabled.to_string()
        ),
        entry!(
            agent_enabled,
            config.agent_enabled.to_string(),
            defaults.agent_enabled.to_string()
        ),
        entry!(
            agent_inbox_dir,
            config.agent_inbox_dir.clone(),
            defaults.agent_inbox_dir.clone()
        ),
        entry!(
            agent_storage_dir,
            config.agent_storage_dir.clone(),
            defaults.agent_storage_dir.clone()
        ),
        entry!(
            agent_llm_url,
            config.agent_llm_url.clone(),
            defaults.agent_llm_url.clone()
        ),
        entry!(
            agent_llm_model,
            config.agent_llm_model.clone(),
            defaults.agent_llm_model.clone()
        ),
        entry!(
            agent_llm_auth,
            config.agent_llm_auth.clone(),
            defaults.agent_llm_auth.clone()
        ),
        entry!(
            agent_confidence_threshold,
            config.agent_confidence_threshold.to_string(),
            defaults.agent_confidence_threshold.to_string()
        ),
        entry!(
            agent_context_limit,
            config.agent_context_limit.to_string(),
            defaults.agent_context_limit.to_string()
        ),
        entry!(
            agent_concurrency,
            config.agent_concurrency.to_string(),
            defaults.agent_concurrency.to_string()
        ),
        entry!(
            lastfm_api_key,
            config.lastfm_api_key.clone(),
            defaults.lastfm_api_key.clone()
        ),
        entry!(
            lastfm_shared_secret,
            config.lastfm_shared_secret.clone(),
            defaults.lastfm_shared_secret.clone()
        ),
    ]
}

pub async fn debug_handler(
    admin: AuthenticatedUser,
    i18n: I18n,
    _startup_config: &AppConfig,
    db: &Database,
) -> cot::Result<Html> {
    let (config, sources) = AppConfig::load_with_db(db).await;

    let db_status = match db.raw("SELECT 1").await {
        Ok(_) => i18n.t.debug_db_connected.to_owned(),
        Err(e) => format!("{}: {e}", i18n.t.debug_db_error),
    };

    let template = DebugTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        build: &BUILD_INFO,
        config_entries: config_display_entries(&config, &sources),
        db_status,
    };
    Ok(Html::new(template.render()?))
}

// ---------------------------------------------------------------------------
// Settings page
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "admin/settings.html")]
struct SettingsTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    saved: bool,
    auth_password_enabled: bool,
    auth_password_enabled_source: &'static str,
    auth_sso_enabled: bool,
    auth_sso_enabled_source: &'static str,
    oidc_button_text: String,
    oidc_button_text_source: &'static str,
    oidc_issuer: String,
    oidc_issuer_source: &'static str,
    oidc_client_id: String,
    oidc_client_id_source: &'static str,
    oidc_client_secret: String,
    oidc_client_secret_source: &'static str,
    oidc_admin_groups: String,
    oidc_admin_groups_source: &'static str,
    oidc_user_groups: String,
    oidc_user_groups_source: &'static str,
    swagger_enabled: bool,
    swagger_enabled_source: &'static str,
    agent_enabled: bool,
    agent_enabled_source: &'static str,
    agent_inbox_dir: String,
    agent_inbox_dir_source: &'static str,
    agent_storage_dir: String,
    agent_storage_dir_source: &'static str,
    agent_llm_url: String,
    agent_llm_url_source: &'static str,
    agent_llm_model: String,
    agent_llm_model_source: &'static str,
    agent_llm_auth: String,
    agent_llm_auth_source: &'static str,
    agent_confidence_threshold: String,
    agent_confidence_threshold_source: &'static str,
    agent_context_limit: String,
    agent_context_limit_source: &'static str,
    agent_concurrency: String,
    agent_concurrency_source: &'static str,
    lastfm_api_key: String,
    lastfm_api_key_source: &'static str,
    lastfm_shared_secret: String,
    lastfm_shared_secret_source: &'static str,
}

pub async fn settings_handler(
    admin: AuthenticatedUser,
    i18n: I18n,
    _startup_config: &AppConfig,
    db: &Database,
    saved: bool,
) -> cot::Result<Html> {
    let (config, sources) = AppConfig::load_with_db(db).await;

    let template = SettingsTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        saved,
        auth_password_enabled: config.auth_password_enabled,
        auth_password_enabled_source: sources.auth_password_enabled.code(),
        auth_sso_enabled: config.auth_sso_enabled,
        auth_sso_enabled_source: sources.auth_sso_enabled.code(),
        oidc_button_text: config.oidc_button_text,
        oidc_button_text_source: sources.oidc_button_text.code(),
        oidc_issuer: config.oidc_issuer,
        oidc_issuer_source: sources.oidc_issuer.code(),
        oidc_client_id: config.oidc_client_id,
        oidc_client_id_source: sources.oidc_client_id.code(),
        oidc_client_secret: config.oidc_client_secret,
        oidc_client_secret_source: sources.oidc_client_secret.code(),
        oidc_admin_groups: config.oidc_admin_groups,
        oidc_admin_groups_source: sources.oidc_admin_groups.code(),
        oidc_user_groups: config.oidc_user_groups,
        oidc_user_groups_source: sources.oidc_user_groups.code(),
        swagger_enabled: config.swagger_enabled,
        swagger_enabled_source: sources.swagger_enabled.code(),
        agent_enabled: config.agent_enabled,
        agent_enabled_source: sources.agent_enabled.code(),
        agent_inbox_dir: config.agent_inbox_dir.clone(),
        agent_inbox_dir_source: sources.agent_inbox_dir.code(),
        agent_storage_dir: config.agent_storage_dir.clone(),
        agent_storage_dir_source: sources.agent_storage_dir.code(),
        agent_llm_url: config.agent_llm_url.clone(),
        agent_llm_url_source: sources.agent_llm_url.code(),
        agent_llm_model: config.agent_llm_model.clone(),
        agent_llm_model_source: sources.agent_llm_model.code(),
        agent_llm_auth: config.agent_llm_auth.clone(),
        agent_llm_auth_source: sources.agent_llm_auth.code(),
        agent_confidence_threshold: config.agent_confidence_threshold.to_string(),
        agent_confidence_threshold_source: sources.agent_confidence_threshold.code(),
        agent_context_limit: config.agent_context_limit.to_string(),
        agent_context_limit_source: sources.agent_context_limit.code(),
        agent_concurrency: config.agent_concurrency.to_string(),
        agent_concurrency_source: sources.agent_concurrency.code(),
        lastfm_api_key: config.lastfm_api_key.clone(),
        lastfm_api_key_source: sources.lastfm_api_key.code(),
        lastfm_shared_secret: config.lastfm_shared_secret.clone(),
        lastfm_shared_secret_source: sources.lastfm_shared_secret.code(),
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct OidcSettingsForm {
    auth_password_enabled: Option<String>,
    auth_sso_enabled: Option<String>,
    oidc_button_text: Option<String>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    oidc_client_secret: Option<String>,
    oidc_admin_groups: Option<String>,
    oidc_user_groups: Option<String>,
    swagger_enabled: Option<String>,
    agent_enabled: Option<String>,
    agent_inbox_dir: Option<String>,
    agent_storage_dir: Option<String>,
    agent_llm_url: Option<String>,
    agent_llm_model: Option<String>,
    agent_llm_auth: Option<String>,
    agent_confidence_threshold: Option<String>,
    agent_context_limit: Option<String>,
    agent_concurrency: Option<String>,
    lastfm_api_key: Option<String>,
    lastfm_shared_secret: Option<String>,
}

pub async fn settings_submit(
    _admin: AuthenticatedUser,
    _i18n: I18n,
    _startup_config: &AppConfig,
    db: &Database,
    form: RequestForm<OidcSettingsForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let pw_enabled = if data.auth_password_enabled.is_some() {
                "true"
            } else {
                "false"
            };
            let sso_enabled = if data.auth_sso_enabled.is_some() {
                "true"
            } else {
                "false"
            };
            let swagger = if data.swagger_enabled.is_some() {
                "true"
            } else {
                "false"
            };
            let agent_en = if data.agent_enabled.is_some() {
                "true"
            } else {
                "false"
            };
            let oidc_button_text = data.oidc_button_text.unwrap_or_default();
            let oidc_issuer = data.oidc_issuer.unwrap_or_default();
            let oidc_client_id = data.oidc_client_id.unwrap_or_default();
            let oidc_client_secret = data.oidc_client_secret.unwrap_or_default();
            let oidc_admin_groups = data.oidc_admin_groups.unwrap_or_default();
            let oidc_user_groups = data.oidc_user_groups.unwrap_or_default();
            let agent_inbox_dir = data.agent_inbox_dir.unwrap_or_default();
            let agent_storage_dir = data.agent_storage_dir.unwrap_or_default();
            let agent_llm_url = data.agent_llm_url.unwrap_or_default();
            let agent_llm_model = data.agent_llm_model.unwrap_or_default();
            let agent_llm_auth = data.agent_llm_auth.unwrap_or_default();
            let agent_confidence_threshold = data.agent_confidence_threshold.unwrap_or_default();
            let agent_context_limit = data.agent_context_limit.unwrap_or_default();
            let agent_concurrency = data.agent_concurrency.unwrap_or_default();
            let lastfm_api_key = data.lastfm_api_key.unwrap_or_default();
            let lastfm_shared_secret = data.lastfm_shared_secret.unwrap_or_default();
            let fields: [(&str, &str); 20] = [
                ("auth_password_enabled", pw_enabled),
                ("auth_sso_enabled", sso_enabled),
                ("oidc_button_text", &oidc_button_text),
                ("oidc_issuer", &oidc_issuer),
                ("oidc_client_id", &oidc_client_id),
                ("oidc_client_secret", &oidc_client_secret),
                ("oidc_admin_groups", &oidc_admin_groups),
                ("oidc_user_groups", &oidc_user_groups),
                ("swagger_enabled", swagger),
                ("agent_enabled", agent_en),
                ("agent_inbox_dir", &agent_inbox_dir),
                ("agent_storage_dir", &agent_storage_dir),
                ("agent_llm_url", &agent_llm_url),
                ("agent_llm_model", &agent_llm_model),
                ("agent_llm_auth", &agent_llm_auth),
                ("agent_confidence_threshold", &agent_confidence_threshold),
                ("agent_context_limit", &agent_context_limit),
                ("agent_concurrency", &agent_concurrency),
                ("lastfm_api_key", &lastfm_api_key),
                ("lastfm_shared_secret", &lastfm_shared_secret),
            ];
            for (key, value) in fields {
                let mut entry = ConfigEntry::new(key.to_owned(), value.to_owned());
                if let Err(e) = entry.save(db).await {
                    tracing::error!(key, error = %e, "failed to save config entry");
                    return Err(e.into());
                }
            }

            Ok(auth::redirect("/admin/settings?saved=1"))
        }
        FormResult::ValidationError(_ctx) => {
            tracing::warn!("settings form validation failed");
            Ok(auth::redirect("/admin/settings"))
        }
    }
}

// ---------------------------------------------------------------------------
// Agent probe fragment (loaded via HTMX)
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "admin/probe_fragment.html")]
struct ProbeFragmentTemplate {
    t: &'static Translations,
    agent_enabled: bool,
    agent_llm_url: String,
    agent_probe: AgentProbeResult,
}

pub async fn settings_probe_handler(
    _admin: AuthenticatedUser,
    i18n: I18n,
    _startup_config: &AppConfig,
    db: &Database,
) -> cot::Result<Html> {
    let (config, _sources) = AppConfig::load_with_db(db).await;

    let probe = if config.agent_enabled && !config.agent_llm_url.is_empty() {
        agent::probe_llm(
            &config.agent_llm_url,
            &config.agent_llm_model,
            &config.agent_llm_auth,
        )
        .await
    } else {
        AgentProbeResult::default()
    };

    let template = ProbeFragmentTemplate {
        t: i18n.t,
        agent_enabled: config.agent_enabled,
        agent_llm_url: config.agent_llm_url,
        agent_probe: probe,
    };
    Ok(Html::new(template.render()?))
}

// ---------------------------------------------------------------------------
// User management
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "admin/users.html")]
struct UsersTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    users: Vec<User>,
}

pub async fn users_list(admin: AuthenticatedUser, i18n: I18n, db: &Database) -> cot::Result<Html> {
    let users = User::list_all(db).await.unwrap_or_default();
    let template = UsersTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        users,
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Template)]
#[template(path = "admin/user_form.html")]
struct UserFormTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    is_edit: bool,
    form_user_id: i64,
    form_username: String,
    form_email: String,
    form_display_name: String,
    form_role: String,
}

pub async fn users_new(admin: AuthenticatedUser, i18n: I18n) -> cot::Result<Html> {
    let template = UserFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: false,
        form_user_id: 0,
        form_username: String::new(),
        form_email: String::new(),
        form_display_name: String::new(),
        form_role: "user".into(),
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct UserForm {
    username: String,
    email: String,
    display_name: String,
    password: String,
    role: String,
}

pub async fn users_create(
    _admin: AuthenticatedUser,
    db: &Database,
    form: RequestForm<UserForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let email = if data.email.is_empty() {
                None
            } else {
                Some(data.email.as_str())
            };
            let display_name = if data.display_name.is_empty() {
                None
            } else {
                Some(data.display_name.as_str())
            };
            User::create(
                db,
                &data.username,
                email,
                display_name,
                &data.password,
                &data.role,
            )
            .await
            .map_err(|e| cot::Error::internal(format!("failed to create user: {e}")))?;
            Ok(auth::redirect("/admin/users"))
        }
        FormResult::ValidationError(_) => Ok(auth::redirect("/admin/users/new")),
    }
}

pub async fn users_edit(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    user_id: i64,
) -> cot::Result<Html> {
    let target = User::get_by_id(db, user_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("user not found"))?;
    let template = UserFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: true,
        form_user_id: target.id_val(),
        form_username: target.username_str().to_owned(),
        form_email: target.email_str(),
        form_display_name: target.display_name_str(),
        form_role: target.role_str().to_owned(),
    };
    Ok(Html::new(template.render()?))
}

pub async fn users_update(
    _admin: AuthenticatedUser,
    db: &Database,
    user_id: i64,
    form: RequestForm<UserForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let mut target = User::get_by_id(db, user_id)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
                .ok_or_else(|| cot::Error::internal("user not found"))?;
            let email = if data.email.is_empty() {
                None
            } else {
                Some(data.email.as_str())
            };
            let display_name = if data.display_name.is_empty() {
                None
            } else {
                Some(data.display_name.as_str())
            };
            let new_password = if data.password.is_empty() {
                None
            } else {
                Some(data.password.as_str())
            };
            target
                .update_fields(
                    db,
                    &data.username,
                    email,
                    display_name,
                    new_password,
                    &data.role,
                )
                .await
                .map_err(|e| cot::Error::internal(format!("failed to update user: {e}")))?;
            Ok(auth::redirect("/admin/users"))
        }
        FormResult::ValidationError(_) => {
            Ok(auth::redirect(&format!("/admin/users/{user_id}/edit")))
        }
    }
}

pub async fn users_delete(
    _admin: AuthenticatedUser,
    db: &Database,
    user_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    User::delete_by_id(db, user_id)
        .await
        .map_err(|e| cot::Error::internal(format!("failed to delete user: {e}")))?;
    Ok(auth::redirect("/admin/users"))
}

// ---------------------------------------------------------------------------
// First-run setup page
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "admin/setup.html")]
struct SetupTemplate {
    t: &'static Translations,
    message: String,
}

pub async fn setup_page(i18n: I18n, message: String) -> cot::Result<Html> {
    let template = SetupTemplate { t: i18n.t, message };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm_password: String,
}

pub async fn setup_submit(
    i18n: I18n,
    db: &Database,
    session: &Session,
    form: RequestForm<SetupForm>,
) -> cot::Result<cot::response::Response> {
    let RequestForm(result) = form;
    let data = match result {
        FormResult::Ok(data) => data,
        FormResult::ValidationError(_) => {
            return setup_page(i18n, String::new()).await?.into_response();
        }
    };

    if data.password != data.confirm_password {
        let msg = i18n.t.setup_mismatch.to_owned();
        return setup_page(i18n, msg).await?.into_response();
    }

    let user = User::create(db, &data.username, None, None, &data.password, "admin")
        .await
        .map_err(|e| cot::Error::internal(format!("failed to create admin: {e}")))?;

    auth::login(session, user.id_val()).await?;
    Ok(auth::redirect("/admin/"))
}

// ---------------------------------------------------------------------------
// Artist management
// ---------------------------------------------------------------------------

/// Row for artist list with computed stats.
#[derive(Debug)]
pub struct ArtistRow {
    pub artist: Artist,
    pub release_count: u64,
    pub track_count: u64,
}

#[derive(Debug, Template)]
#[template(path = "admin/artists.html")]
struct ArtistsTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    rows: Vec<ArtistRow>,
}

pub async fn artists_list(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
) -> cot::Result<Html> {
    let artists = Artist::list_all(db).await.unwrap_or_default();
    let mut rows = Vec::with_capacity(artists.len());
    for artist in artists {
        let release_count = ReleaseArtist::count_by_artist(db, artist.id_val())
            .await
            .unwrap_or(0);
        let track_count = TrackArtist::count_by_artist(db, artist.id_val())
            .await
            .unwrap_or(0);
        rows.push(ArtistRow {
            artist,
            release_count,
            track_count,
        });
    }
    let template = ArtistsTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        rows,
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Template)]
#[template(path = "admin/artist_form.html")]
struct ArtistFormTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    is_edit: bool,
    form_artist_id: i64,
    form_name: String,
    current_image_url: Option<String>,
}

pub async fn artists_new(admin: AuthenticatedUser, i18n: I18n) -> cot::Result<Html> {
    let template = ArtistFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: false,
        form_artist_id: 0,
        form_name: String::new(),
        current_image_url: None,
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct ArtistForm {
    name: String,
}

pub async fn artists_create(
    _admin: AuthenticatedUser,
    db: &Database,
    form: RequestForm<ArtistForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            Artist::create(db, &data.name, None)
                .await
                .map_err(|e| cot::Error::internal(format!("failed to create artist: {e}")))?;
            Ok(auth::redirect("/admin/artists"))
        }
        FormResult::ValidationError(_) => Ok(auth::redirect("/admin/artists/new")),
    }
}

pub async fn artists_edit(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    artist_id: i64,
) -> cot::Result<Html> {
    let artist = Artist::get_by_id(db, artist_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("artist not found"))?;

    let current_image_url = match artist.image_file_id {
        Some(fid) => MediaFile::get_by_id(db, fid)
            .await
            .ok()
            .flatten()
            .map(|mf| format!("/api/player/cover/{}/large", mf.id_val())),
        None => None,
    };

    let template = ArtistFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: true,
        form_artist_id: artist.id_val(),
        form_name: artist.name_str().to_owned(),
        current_image_url,
    };
    Ok(Html::new(template.render()?))
}

pub async fn artists_update(
    _admin: AuthenticatedUser,
    db: &Database,
    artist_id: i64,
    form: RequestForm<ArtistForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let mut artist = Artist::get_by_id(db, artist_id)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
                .ok_or_else(|| cot::Error::internal("artist not found"))?;
            artist
                .update_name(db, &data.name)
                .await
                .map_err(|e| cot::Error::internal(format!("failed to update artist: {e}")))?;
            Ok(auth::redirect("/admin/artists"))
        }
        FormResult::ValidationError(_) => {
            Ok(auth::redirect(&format!("/admin/artists/{artist_id}/edit")))
        }
    }
}

pub async fn artists_delete(
    _admin: AuthenticatedUser,
    db: &Database,
    artist_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    Artist::delete_by_id(db, artist_id)
        .await
        .map_err(|e| cot::Error::internal(format!("failed to delete artist: {e}")))?;
    Ok(auth::redirect("/admin/artists"))
}

// ---------------------------------------------------------------------------
// Artist image endpoints
// ---------------------------------------------------------------------------

/// JSON response for available album covers for an artist.
#[derive(serde::Serialize)]
pub struct AvailableCover {
    pub media_file_id: i64,
    pub release_title: String,
    pub cover_url: String,
}

pub async fn artists_available_covers(
    _admin: AuthenticatedUser,
    db: &Database,
    artist_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    let links = ReleaseArtist::find_by_artist(db, artist_id)
        .await
        .unwrap_or_default();

    let mut covers: Vec<AvailableCover> = Vec::new();
    for link in &links {
        if let Ok(Some(release)) = Release::get_by_id(db, link.release_id()).await {
            if let Some(cover_fid) = release.cover_file_id {
                covers.push(AvailableCover {
                    media_file_id: cover_fid,
                    release_title: release.title_str().to_owned(),
                    cover_url: format!("/api/player/cover/{cover_fid}/medium"),
                });
            }
        }
    }

    let json = serde_json::to_string(&covers).unwrap_or_else(|_| "[]".into());
    let resp = cot::http::Response::builder()
        .status(cot::http::StatusCode::OK)
        .header(cot::http::header::CONTENT_TYPE, "application/json")
        .body(Body::fixed(json))
        .expect("valid response");
    Ok(resp)
}

#[derive(serde::Deserialize)]
pub struct SetImageBody {
    pub media_file_id: Option<i64>,
}

pub async fn artists_set_image(
    _admin: AuthenticatedUser,
    db: &Database,
    artist_id: i64,
    parsed: SetImageBody,
) -> cot::Result<cot::http::Response<Body>> {
    let mut artist = Artist::get_by_id(db, artist_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("artist not found"))?;

    // Validate media file exists when setting (not removing)
    if let Some(fid) = parsed.media_file_id {
        MediaFile::get_by_id(db, fid)
            .await
            .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
            .ok_or_else(|| cot::Error::internal("media file not found"))?;
    }

    artist
        .set_image_file_id(db, parsed.media_file_id)
        .await
        .map_err(|e| cot::Error::internal(format!("failed to set image: {e}")))?;

    let resp = cot::http::Response::builder()
        .status(cot::http::StatusCode::OK)
        .header(cot::http::header::CONTENT_TYPE, "application/json")
        .body(Body::fixed(r#"{"ok":true}"#))
        .expect("valid response");
    Ok(resp)
}

#[derive(serde::Deserialize)]
pub struct UploadImageBody {
    pub data: String,
    pub filename: String,
    pub mime_type: String,
}

pub async fn artists_upload_image(
    _admin: AuthenticatedUser,
    db: &Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    artist_id: i64,
    parsed: UploadImageBody,
) -> cot::Result<cot::http::Response<Body>> {
    let mut artist = Artist::get_by_id(db, artist_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("artist not found"))?;

    let storage_dir = &config.agent_storage_dir;
    if storage_dir.is_empty() {
        return Err(cot::Error::internal("agent_storage_dir is not configured"));
    }

    // Decode base64 image data
    use base64::Engine;
    let image_data = base64::engine::general_purpose::STANDARD
        .decode(&parsed.data)
        .map_err(|e| cot::Error::internal(format!("invalid base64: {e}")))?;

    // Build a CoverImage and reuse save_cover_to_storage
    let cover = crate::agent::cover_art::CoverImage {
        data: image_data,
        mime_type: parsed.mime_type.clone(),
        source: crate::agent::cover_art::CoverSource::FolderFile(std::path::PathBuf::from(
            &parsed.filename,
        )),
    };

    let cover_file_id = crate::agent::cover_art::save_cover_to_storage(
        db,
        pool,
        storage_dir,
        artist.name_str(),
        "__artist_image__",
        &cover,
    )
    .await
    .map_err(|e| cot::Error::internal(format!("failed to save image: {e}")))?;

    artist
        .set_image_file_id(db, Some(cover_file_id))
        .await
        .map_err(|e| cot::Error::internal(format!("failed to set image: {e}")))?;

    let resp = cot::http::Response::builder()
        .status(cot::http::StatusCode::OK)
        .header(cot::http::header::CONTENT_TYPE, "application/json")
        .body(Body::fixed(r#"{"ok":true}"#))
        .expect("valid response");
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Release management
// ---------------------------------------------------------------------------

/// Row for release list with resolved artist names.
#[derive(Debug)]
pub struct ReleaseRow {
    pub release: Release,
    pub artist_names: String,
}

#[derive(Debug, Template)]
#[template(path = "admin/releases.html")]
struct ReleasesTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    rows: Vec<ReleaseRow>,
    artists: Vec<Artist>,
    filter_artist_id: Option<i64>,
}

/// Build a map of artist_id → artist_name from a list of artists.
fn artist_name_map(artists: &[Artist]) -> HashMap<i64, String> {
    artists
        .iter()
        .map(|a| (a.id_val(), a.name_str().to_owned()))
        .collect()
}

/// Resolve artist names for a release, joined by ", ".
async fn resolve_artist_names(
    db: &Database,
    release_id: i64,
    names: &HashMap<i64, String>,
) -> String {
    let links = ReleaseArtist::find_by_release(db, release_id)
        .await
        .unwrap_or_default();
    let mut sorted = links;
    sorted.sort_by_key(|l| l.position);
    sorted
        .iter()
        .filter_map(|l| names.get(&l.artist_id()))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn releases_list(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    filter_artist_id: Option<i64>,
) -> cot::Result<Html> {
    let all_artists = Artist::list_all(db).await.unwrap_or_default();
    let names = artist_name_map(&all_artists);

    let releases = Release::list_all(db).await.unwrap_or_default();

    // If filtering by artist, find the set of release_ids for that artist
    let filtered_release_ids: Option<Vec<i64>> = match filter_artist_id {
        Some(aid) => {
            let links = ReleaseArtist::find_by_artist(db, aid)
                .await
                .unwrap_or_default();
            Some(links.iter().map(|l| l.release_id()).collect())
        }
        None => None,
    };

    let mut rows = Vec::new();
    for release in releases {
        if let Some(ref ids) = filtered_release_ids {
            if !ids.contains(&release.id_val()) {
                continue;
            }
        }
        let artist_names = resolve_artist_names(db, release.id_val(), &names).await;
        rows.push(ReleaseRow {
            release,
            artist_names,
        });
    }

    let template = ReleasesTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        rows,
        artists: all_artists,
        filter_artist_id,
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Template)]
#[template(path = "admin/release_form.html")]
struct ReleaseFormTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    is_edit: bool,
    form_release_id: i64,
    form_title: String,
    form_release_type: String,
    form_year: String,
    form_artist_ids: Vec<i64>,
    artists: Vec<Artist>,
    release_types: &'static [(&'static str, &'static str, &'static str)],
    lang_code: &'static str,
}

pub async fn releases_new(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
) -> cot::Result<Html> {
    let artists = Artist::list_all(db).await.unwrap_or_default();
    let template = ReleaseFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: false,
        form_release_id: 0,
        form_title: String::new(),
        form_release_type: "album".into(),
        form_year: String::new(),
        form_artist_ids: Vec::new(),
        artists,
        release_types: RELEASE_TYPES,
        lang_code: i18n.t.lang.code(),
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct ReleaseForm {
    title: String,
    release_type: String,
    year: String,
    artist_id: String, // comma-separated IDs or single ID
}

fn parse_artist_ids(raw: &str) -> Vec<i64> {
    raw.split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

pub async fn releases_create(
    _admin: AuthenticatedUser,
    db: &Database,
    form: RequestForm<ReleaseForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let year = data.year.trim().parse::<i32>().ok();
            let release = Release::create(db, &data.title, &data.release_type, year, None)
                .await
                .map_err(|e| cot::Error::internal(format!("failed to create release: {e}")))?;
            let artist_ids = parse_artist_ids(&data.artist_id);
            if !artist_ids.is_empty() {
                ReleaseArtist::set_artists(db, release.id_val(), &artist_ids)
                    .await
                    .map_err(|e| cot::Error::internal(format!("failed to link artists: {e}")))?;
            }
            Ok(auth::redirect("/admin/releases"))
        }
        FormResult::ValidationError(_) => Ok(auth::redirect("/admin/releases/new")),
    }
}

pub async fn releases_edit(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    release_id: i64,
) -> cot::Result<Html> {
    let release = Release::get_by_id(db, release_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("release not found"))?;

    let links = ReleaseArtist::find_by_release(db, release_id)
        .await
        .unwrap_or_default();
    let mut current_artist_ids: Vec<i64> = links.iter().map(|l| l.artist_id()).collect();
    current_artist_ids.sort();

    let artists = Artist::list_all(db).await.unwrap_or_default();

    let template = ReleaseFormTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        is_edit: true,
        form_release_id: release.id_val(),
        form_title: release.title_str().to_owned(),
        form_release_type: release.release_type_str().to_owned(),
        form_year: release.year_display(),
        form_artist_ids: current_artist_ids,
        artists,
        release_types: RELEASE_TYPES,
        lang_code: i18n.t.lang.code(),
    };
    Ok(Html::new(template.render()?))
}

pub async fn releases_update(
    _admin: AuthenticatedUser,
    db: &Database,
    release_id: i64,
    form: RequestForm<ReleaseForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    match result {
        FormResult::Ok(data) => {
            let mut release = Release::get_by_id(db, release_id)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
                .ok_or_else(|| cot::Error::internal("release not found"))?;
            let year = data.year.trim().parse::<i32>().ok();
            release
                .update_fields(db, &data.title, &data.release_type, year)
                .await
                .map_err(|e| cot::Error::internal(format!("failed to update release: {e}")))?;
            let artist_ids = parse_artist_ids(&data.artist_id);
            ReleaseArtist::set_artists(db, release_id, &artist_ids)
                .await
                .map_err(|e| cot::Error::internal(format!("failed to update artists: {e}")))?;
            Ok(auth::redirect("/admin/releases"))
        }
        FormResult::ValidationError(_) => Ok(auth::redirect(&format!(
            "/admin/releases/{release_id}/edit"
        ))),
    }
}

pub async fn releases_delete(
    _admin: AuthenticatedUser,
    db: &Database,
    pool: &sqlx::PgPool,
    release_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    let (config, _) = AppConfig::load_with_db(db).await;
    crate::library_cleanup::delete_releases(pool, &[release_id], &config.agent_storage_dir)
        .await
        .map_err(|e| cot::Error::internal(format!("failed to delete release: {e}")))?;
    Ok(auth::redirect("/admin/releases"))
}

// ===========================================================================
// Media Files
// ===========================================================================

#[derive(Debug)]
pub struct MediaFileRow {
    pub media_file: MediaFile,
    pub track_title: String,
}

#[derive(Debug, Template)]
#[template(path = "admin/media_files.html")]
struct MediaFilesTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    rows: Vec<MediaFileRow>,
}

pub async fn media_files_list(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
) -> cot::Result<Html> {
    let files = MediaFile::list_all(db).await.unwrap_or_default();
    let tracks = Track::list_all(db).await.unwrap_or_default();

    // Build a map of audio_file_id → track title
    let track_map: HashMap<i64, String> = tracks
        .iter()
        .map(|t| (t.audio_file_id, t.title.to_string()))
        .collect();

    let rows: Vec<MediaFileRow> = files
        .into_iter()
        .map(|mf| {
            let track_title = track_map.get(&mf.id_val()).cloned().unwrap_or_default();
            MediaFileRow {
                media_file: mf,
                track_title,
            }
        })
        .collect();

    let template = MediaFilesTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        rows,
    };
    Ok(Html::new(template.render()?))
}

pub async fn media_files_delete(
    _admin: AuthenticatedUser,
    db: &Database,
    file_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    MediaFile::delete_by_id(db, file_id)
        .await
        .map_err(|e| cot::Error::internal(format!("failed to delete media file: {e}")))?;
    Ok(auth::redirect("/admin/media-files"))
}

// ===========================================================================
// Jobs
// ===========================================================================

#[derive(Debug, Template)]
#[template(path = "admin/jobs.html")]
struct JobsTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    jobs: Vec<ScheduledJob>,
}

pub async fn jobs_list(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    registry: &JobRegistry,
) -> cot::Result<Html> {
    // Ensure all registered jobs exist in DB and remove orphans
    sync_registered_jobs(db, registry).await;

    let jobs = ScheduledJob::list_all(db).await.unwrap_or_default();
    let template = JobsTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        jobs,
    };
    Ok(Html::new(template.render()?))
}

/// Ensure the DB has a ScheduledJob row for every registered job and remove
/// rows for jobs that are no longer registered.
async fn sync_registered_jobs(db: &Database, registry: &JobRegistry) {
    for job in registry.all_jobs() {
        if let Err(e) =
            ScheduledJob::upsert(db, job.name(), job.description(), job.default_cron()).await
        {
            tracing::error!("failed to upsert scheduled job {}: {e}", job.name());
        }
    }
    if let Ok(all) = ScheduledJob::list_all(db).await {
        for sched_job in all {
            if registry.get(sched_job.name_str()).is_none() {
                tracing::warn!("Removing orphaned scheduled job '{}'", sched_job.name_str());
                let _ = ScheduledJob::delete_by_name(db, sched_job.name_str()).await;
            }
        }
    }
}

#[derive(Debug, Template)]
#[template(path = "admin/job_detail.html")]
struct JobDetailTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    job: ScheduledJob,
    runs: Vec<JobRun>,
}

#[derive(Debug, Form)]
pub struct MetadataBackfillForm {
    audio_bitrate: Option<String>,
    audio_sample_rate: Option<String>,
    audio_bit_depth: Option<String>,
    duration_seconds: Option<String>,
    local_genres: Option<String>,
    lastfm_tags: Option<String>,
    mode: Option<String>,
}

pub async fn job_detail(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    pool: &sqlx::PgPool,
    job_name: &str,
) -> cot::Result<Html> {
    let job = ScheduledJob::get_by_name(db, job_name)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("job not found"))?;

    let runs = JobRun::list_by_job(pool, job_name, 50)
        .await
        .unwrap_or_default();

    let template = JobDetailTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        job,
        runs,
    };
    Ok(Html::new(template.render()?))
}

pub async fn job_run_now(
    _admin: AuthenticatedUser,
    handle_cell: &Arc<tokio::sync::OnceCell<Arc<scheduler::SchedulerHandle>>>,
    job_name: &str,
) -> cot::Result<cot::http::Response<Body>> {
    if let Some(handle) = handle_cell.get() {
        match handle.trigger_job_now(job_name).await {
            Ok(_run_id) => {}
            Err(e) => {
                tracing::error!(?e, job_name, "manual job trigger failed");
            }
        }
    } else {
        tracing::error!(job_name, "scheduler not ready, cannot trigger job");
    }
    Ok(auth::redirect(&format!("/admin/jobs/{job_name}")))
}

pub async fn metadata_backfill_run(
    _admin: AuthenticatedUser,
    db: &Database,
    pool: &sqlx::PgPool,
    form: RequestForm<MetadataBackfillForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    let data = match result {
        FormResult::Ok(data) => data,
        FormResult::ValidationError(_) => {
            return Ok(auth::redirect("/admin/jobs/metadata_backfill"));
        }
    };

    let options = crate::jobs::metadata_backfill::MetadataBackfillOptions {
        audio_bitrate: data.audio_bitrate.is_some(),
        audio_sample_rate: data.audio_sample_rate.is_some(),
        audio_bit_depth: data.audio_bit_depth.is_some(),
        duration_seconds: data.duration_seconds.is_some(),
        local_genres: data.local_genres.is_some(),
        lastfm_tags: data.lastfm_tags.is_some(),
        musicbrainz_tags: true,
        overwrite: data.mode.as_deref() == Some("overwrite"),
    };

    let mut run = JobRun::create_running(db, "metadata_backfill", "manual")
        .await
        .map_err(|e| cot::Error::internal(format!("failed to create job run: {e}")))?;
    let run_id = run.id_val();
    let db = db.clone();
    let pool = pool.clone();
    let (live_config, _) = AppConfig::load_with_db(&db).await;

    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let ctx = scheduler::JobContext {
            config: Arc::new(live_config),
            db: db.clone(),
            pool: pool.clone(),
            run_id,
            registry: Arc::new(JobRegistry::new()),
        };
        let mut log = scheduler::JobLog::with_live_flush(pool.clone(), run_id);
        let result =
            crate::jobs::metadata_backfill::run_with_options(&ctx, &mut log, options).await;
        let duration_ms = start.elapsed().as_millis() as i64;
        match result {
            Ok(()) => {
                let _ = run.set_completed(&db, duration_ms, &log.output()).await;
            }
            Err(e) => {
                let _ = run
                    .set_failed(&db, duration_ms, &log.output(), &e.to_string())
                    .await;
            }
        }
    });

    Ok(auth::redirect(&format!(
        "/admin/jobs/metadata_backfill/runs/{run_id}"
    )))
}

pub async fn job_toggle_enabled(
    _admin: AuthenticatedUser,
    db: &Database,
    handle_cell: &Arc<tokio::sync::OnceCell<Arc<scheduler::SchedulerHandle>>>,
    job_name: &str,
) -> cot::Result<cot::http::Response<Body>> {
    if job_name == "metadata_backfill" {
        return Ok(auth::redirect("/admin/jobs/metadata_backfill"));
    }

    let job = ScheduledJob::get_by_name(db, job_name)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("job not found"))?;

    let new_enabled = !job.enabled;

    if let Some(handle) = handle_cell.get() {
        if let Err(e) = handle.toggle_job(job_name, new_enabled).await {
            tracing::error!(?e, job_name, new_enabled, "toggle_job failed");
        }
    } else {
        tracing::error!(job_name, "scheduler not ready, cannot toggle job");
    }

    Ok(auth::redirect("/admin/jobs"))
}

#[derive(Debug, Form)]
pub struct CronForm {
    cron_expression: String,
}

pub async fn job_update_cron(
    _admin: AuthenticatedUser,
    _db: &Database,
    handle_cell: &Arc<tokio::sync::OnceCell<Arc<scheduler::SchedulerHandle>>>,
    job_name: &str,
    form: RequestForm<CronForm>,
) -> cot::Result<cot::http::Response<Body>> {
    if job_name == "metadata_backfill" {
        return Ok(auth::redirect("/admin/jobs/metadata_backfill"));
    }

    let RequestForm(result) = form;
    if let FormResult::Ok(data) = result {
        if let Some(handle) = handle_cell.get() {
            if let Err(e) = handle.reschedule_job(job_name, &data.cron_expression).await {
                tracing::error!(?e, job_name, "reschedule_job failed");
            }
        } else {
            tracing::error!(job_name, "scheduler not ready, cannot update cron");
        }
    }
    Ok(auth::redirect(&format!("/admin/jobs/{job_name}")))
}

#[derive(Debug, Template)]
#[template(path = "admin/job_run_detail.html")]
struct JobRunDetailTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    run: JobRun,
    job_name: String,
}

pub async fn job_run_detail(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    _job_name: &str,
    run_id: i64,
) -> cot::Result<Html> {
    let run = JobRun::get_by_id(db, run_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("run not found"))?;

    let template = JobRunDetailTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        job_name: run.job_name.to_string(),
        run,
    };
    Ok(Html::new(template.render()?))
}

// ===========================================================================
// Reviews
// ===========================================================================

#[derive(Debug, Template)]
#[template(path = "admin/reviews.html")]
struct ReviewsTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    rows: Vec<ReviewListRow>,
    stats_map: HashMap<i64, scheduler::ProcessingStatsRow>,
    status_filter: String,
}

#[derive(Debug)]
struct ReviewListRow {
    review: PendingReview,
    display_input_path: String,
    media_tags: Vec<ReviewMediaTag>,
}

#[derive(Debug, Clone)]
struct ReviewMediaTag {
    label: String,
    kind: &'static str,
}

#[derive(Debug, sqlx::FromRow)]
struct ReviewMediaTagRow {
    sha256_hash: String,
    original_filename: String,
    file_size_bytes: i64,
    audio_format: Option<String>,
    audio_bitrate: Option<i32>,
    audio_sample_rate: Option<i32>,
    audio_bit_depth: Option<i32>,
}

fn compact_path_tail(path: &str, max_chars: usize) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    let segments = normalized.split('/').collect::<Vec<_>>();
    let filename = segments.last().copied().unwrap_or(normalized.as_str());
    let filename_len = filename.chars().count();
    if filename_len + 4 <= max_chars {
        return format!(".../{filename}");
    }

    if filename_len > max_chars {
        let suffix_len = max_chars.saturating_sub(3);
        let suffix = filename
            .chars()
            .skip(filename_len.saturating_sub(suffix_len))
            .collect::<String>();
        return format!("...{suffix}");
    }
    format!(".../{filename}")
}

fn context_sha256(review: &PendingReview) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(review.context_json_str()).ok()?;
    let sha = value.get("sha256")?.as_str()?.trim();
    let is_sha256 = sha.len() == 64 && sha.chars().all(|ch| ch.is_ascii_hexdigit());
    is_sha256.then(|| sha.to_ascii_lowercase())
}

fn file_extension(filename: &str) -> Option<String> {
    std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
}

fn size_display(bytes: i64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn review_tag(label: impl Into<String>, kind: &'static str) -> ReviewMediaTag {
    ReviewMediaTag {
        label: label.into(),
        kind,
    }
}

fn media_tags(row: &ReviewMediaTagRow) -> Vec<ReviewMediaTag> {
    let mut tags = Vec::new();
    if let Some(format) = row.audio_format.as_deref().filter(|s| !s.is_empty()) {
        tags.push(review_tag(format.to_ascii_lowercase(), "format"));
    } else if let Some(ext) = file_extension(&row.original_filename) {
        tags.push(review_tag(ext, "format"));
    }
    if let Some(bitrate) = row.audio_bitrate {
        tags.push(review_tag(format!("{bitrate} kbps"), "bitrate"));
    }
    if let Some(sample_rate) = row.audio_sample_rate {
        if sample_rate % 1000 == 0 {
            tags.push(review_tag(format!("{} kHz", sample_rate / 1000), "sample"));
        } else {
            tags.push(review_tag(
                format!("{:.1} kHz", sample_rate as f64 / 1000.0),
                "sample",
            ));
        }
    }
    if let Some(bit_depth) = row.audio_bit_depth {
        tags.push(review_tag(format!("{bit_depth}-bit"), "depth"));
    }
    tags.push(review_tag(size_display(row.file_size_bytes), "size"));
    tags
}

async fn review_media_tags(
    pool: &sqlx::PgPool,
    reviews: &[PendingReview],
) -> HashMap<String, Vec<ReviewMediaTag>> {
    let mut hashes = reviews
        .iter()
        .filter_map(context_sha256)
        .collect::<Vec<_>>();
    hashes.sort();
    hashes.dedup();
    if hashes.is_empty() {
        return HashMap::new();
    }

    let quoted = hashes
        .iter()
        .map(|hash| format!("'{hash}'"))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT sha256_hash::text AS sha256_hash, \
                original_filename::text AS original_filename, \
                file_size_bytes, \
                audio_format::text AS audio_format, \
                audio_bitrate, audio_sample_rate, audio_bit_depth \
         FROM furumusic__media_file \
         WHERE file_type = 'audio' AND sha256_hash IN ({quoted})"
    );

    match sqlx::query_as::<_, ReviewMediaTagRow>(&query)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|row| (row.sha256_hash.to_ascii_lowercase(), media_tags(&row)))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to load review media tags");
            HashMap::new()
        }
    }
}

pub async fn reviews_list(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    pool: &sqlx::PgPool,
    status: Option<&str>,
) -> cot::Result<Html> {
    let reviews = match status {
        Some(s) if !s.is_empty() => PendingReview::list_by_status(db, s)
            .await
            .unwrap_or_default(),
        _ => PendingReview::list_all(db).await.unwrap_or_default(),
    };

    let review_ids: Vec<i64> = reviews.iter().map(|r| r.id_val()).collect();
    let stats_map = scheduler::ProcessingStats::list_by_review_ids(pool, &review_ids)
        .await
        .unwrap_or_default();
    let media_tags = review_media_tags(pool, &reviews).await;
    let rows = reviews
        .into_iter()
        .map(|review| {
            let media_tags = context_sha256(&review)
                .and_then(|sha| media_tags.get(&sha).cloned())
                .unwrap_or_default();
            let display_input_path = compact_path_tail(review.input_path_str(), 80);
            ReviewListRow {
                review,
                display_input_path,
                media_tags,
            }
        })
        .collect();

    let template = ReviewsTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        rows,
        stats_map,
        status_filter: status.unwrap_or("").to_owned(),
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Template)]
#[template(path = "admin/review_detail.html")]
struct ReviewDetailTemplate {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    review: PendingReview,
    edit: ReviewEditFields,
    release_types: &'static [(&'static str, &'static str, &'static str)],
    lang_code: &'static str,
    context_pretty: String,
    result_pretty: String,
    error_message: String,
    stats: Option<scheduler::ProcessingStats>,
}

#[derive(Debug, Default)]
struct ReviewEditFields {
    title: String,
    artist: String,
    album: String,
    year: String,
    track_number: String,
    genre: String,
    featured_artists: String,
    release_type: String,
    notes: String,
}

#[derive(Debug, Form)]
pub struct ReviewApproveForm {
    title: String,
    artist: String,
    album: String,
    year: String,
    track_number: String,
    genre: String,
    featured_artists: String,
    release_type: String,
    notes: String,
}

fn optional_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn parse_optional_i32(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok()
}

fn parse_featured_artists(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn edit_fields_from_normalized(
    normalized: &crate::agent::dto::NormalizedFields,
) -> ReviewEditFields {
    ReviewEditFields {
        title: normalized.title.clone().unwrap_or_default(),
        artist: normalized.artist.clone().unwrap_or_default(),
        album: normalized.album.clone().unwrap_or_default(),
        year: normalized.year.map(|v| v.to_string()).unwrap_or_default(),
        track_number: normalized
            .track_number
            .map(|v| v.to_string())
            .unwrap_or_default(),
        genre: normalized.genre.clone().unwrap_or_default(),
        featured_artists: normalized.featured_artists.join(", "),
        release_type: normalized
            .release_type
            .clone()
            .unwrap_or_else(|| "album".to_owned()),
        notes: normalized.notes.clone().unwrap_or_default(),
    }
}

fn normalized_from_result_json(result_json: &str) -> crate::agent::dto::NormalizedFields {
    serde_json::from_str(result_json).unwrap_or_default()
}

fn normalized_from_review_form(form: &ReviewApproveForm) -> crate::agent::dto::NormalizedFields {
    crate::agent::dto::NormalizedFields {
        title: optional_trimmed(&form.title),
        artist: optional_trimmed(&form.artist),
        album: optional_trimmed(&form.album),
        year: parse_optional_i32(&form.year),
        track_number: parse_optional_i32(&form.track_number),
        genre: optional_trimmed(&form.genre),
        featured_artists: parse_featured_artists(&form.featured_artists),
        release_type: optional_trimmed(&form.release_type).or_else(|| Some("album".to_owned())),
        confidence: Some(1.0),
        notes: optional_trimmed(&form.notes),
    }
}

pub async fn review_detail(
    admin: AuthenticatedUser,
    i18n: I18n,
    db: &Database,
    review_id: i64,
) -> cot::Result<Html> {
    let review = PendingReview::get_by_id(db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("review not found"))?;

    let context_pretty = review
        .context_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        .unwrap_or_default();

    let result_pretty = review
        .result_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        .unwrap_or_default();

    let error_message = review.error_message_str().to_owned();

    let stats = scheduler::ProcessingStats::get_by_review_id(db, review_id)
        .await
        .unwrap_or(None);
    let normalized = normalized_from_result_json(review.result_json_str());
    let edit = edit_fields_from_normalized(&normalized);

    let template = ReviewDetailTemplate {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        review,
        edit,
        release_types: RELEASE_TYPES,
        lang_code: i18n.t.lang.code(),
        context_pretty,
        result_pretty,
        error_message,
        stats,
    };
    Ok(Html::new(template.render()?))
}

pub async fn review_approve(
    _admin: AuthenticatedUser,
    _config: &Arc<AppConfig>,
    db: &Database,
    pool: &sqlx::PgPool,
    review_id: i64,
    form: RequestForm<ReviewApproveForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let mut review = PendingReview::get_by_id(db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("review not found"))?;

    let RequestForm(form_result) = form;
    let normalized = match form_result {
        FormResult::Ok(data) => normalized_from_review_form(&data),
        FormResult::ValidationError(_) => {
            return Ok(auth::redirect(&format!("/admin/reviews/{review_id}")));
        }
    };
    let result_str = serde_json::to_string(&normalized)
        .map_err(|e| cot::Error::internal(format!("failed to serialize review fields: {e}")))?;
    review
        .set_result_json(db, result_str)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
    let context_str = review.context_json_str().to_owned();
    let input_path = review.input_path_str().to_owned();

    let context: serde_json::Value = serde_json::from_str(&context_str).unwrap_or_default();

    // Load live config from DB so admin-set values are used
    let (live_config, _) = AppConfig::load_with_db(db).await;

    // Look up the model name from processing stats (if LLM processed this review)
    let stats = scheduler::ProcessingStats::get_by_review_id(db, review_id)
        .await
        .unwrap_or(None);
    let model_name_str = stats.as_ref().map(|s| s.model_name.to_string());

    match crate::jobs::inbox_process::finalize_approved(
        db,
        pool,
        &live_config,
        &input_path,
        &normalized,
        &context,
        &live_config.agent_storage_dir,
        model_name_str.as_deref(),
    )
    .await
    {
        Ok(()) => {
            let _ = review.set_approved(db).await;
        }
        Err(e) => {
            tracing::error!(?e, "review approval failed");
            let _ = review.set_rejected(db).await;
        }
    }

    Ok(auth::redirect(&format!("/admin/reviews/{review_id}")))
}

#[derive(Debug, Form)]
pub struct ReviewsBulkForm {
    selected_ids: Option<String>,
    action: Option<String>,
    status_filter: Option<String>,
}

fn parse_review_ids(raw: &str) -> Vec<i64> {
    let mut ids = raw
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn reviews_redirect(status: Option<&str>) -> String {
    match status {
        Some(s) if !s.is_empty() => format!("/admin/reviews?status={s}"),
        _ => "/admin/reviews".to_owned(),
    }
}

pub async fn reviews_bulk(
    _admin: AuthenticatedUser,
    db: &Database,
    form: RequestForm<ReviewsBulkForm>,
) -> cot::Result<cot::http::Response<Body>> {
    let RequestForm(result) = form;
    let data = match result {
        FormResult::Ok(data) => data,
        FormResult::ValidationError(_) => return Ok(auth::redirect("/admin/reviews")),
    };

    let redirect_url = reviews_redirect(data.status_filter.as_deref());
    let ids = parse_review_ids(data.selected_ids.as_deref().unwrap_or_default());
    if ids.is_empty() {
        return Ok(auth::redirect(&redirect_url));
    }

    match data.action.as_deref() {
        Some("delete") => {
            PendingReview::delete_by_ids(db, &ids)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
        }
        Some("requeue") => {
            PendingReview::requeue_by_ids(db, &ids)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
        }
        _ => {}
    }

    Ok(auth::redirect(&redirect_url))
}

pub async fn review_reject(
    _admin: AuthenticatedUser,
    db: &Database,
    review_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    let mut review = PendingReview::get_by_id(db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("review not found"))?;
    let _ = review.set_rejected(db).await;
    Ok(auth::redirect(&format!("/admin/reviews/{review_id}")))
}

pub async fn reviews_clear(
    _admin: AuthenticatedUser,
    db: &Database,
    status: Option<&str>,
) -> cot::Result<cot::http::Response<Body>> {
    match status {
        Some(s) if !s.is_empty() => {
            PendingReview::delete_by_status(db, s)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
        }
        _ => {
            PendingReview::delete_all(db)
                .await
                .map_err(|e| cot::Error::internal(format!("db error: {e}")))?;
        }
    }
    let redirect_url = match status {
        Some(s) if !s.is_empty() => format!("/admin/reviews?status={s}"),
        _ => "/admin/reviews".to_owned(),
    };
    Ok(auth::redirect(&redirect_url))
}

pub async fn review_requeue(
    _admin: AuthenticatedUser,
    db: &Database,
    review_id: i64,
) -> cot::Result<cot::http::Response<Body>> {
    let mut review = PendingReview::get_by_id(db, review_id)
        .await
        .map_err(|e| cot::Error::internal(format!("db error: {e}")))?
        .ok_or_else(|| cot::Error::internal("review not found"))?;
    let _ = review.set_queued(db).await;
    Ok(auth::redirect(&format!("/admin/reviews/{review_id}")))
}
