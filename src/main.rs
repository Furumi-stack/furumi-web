mod admin;
mod agent;
mod api;
mod auth;
mod config;
mod federation;
mod i18n;
mod jobs;
mod lastfm;
mod local_uploads;
mod media_paths;
mod metrics;
mod music;
mod oidc;
mod player;
mod scheduler;
mod similarity;
mod torrents;
mod user;
mod youtube;

use std::sync::Arc;

use cot::auth::PasswordVerificationResult;
use cot::cli::CliMetadata;
use cot::common_types::Password;
use cot::config::{
    DatabaseConfig, MiddlewareConfig, ProjectConfig, SameSite, SessionMiddlewareConfig,
    SessionStoreConfig, SessionStoreTypeConfig,
};
use cot::db::Database;
use cot::form::{Form, FormResult};
use cot::html::Html;
use cot::middleware::SessionMiddleware;
use cot::project::RegisterAppsContext;
use cot::request::extractors::{Path, RequestForm, UrlQuery};
use cot::response::IntoResponse;
use cot::router::method::get;
use cot::router::{Route, Router};
use cot::session::Session;
use cot::static_files::StaticFilesMiddleware;
use cot::{App, AppBuilder, Body, Project, Template};
use serde::Deserialize;

use crate::config::AppConfig;
use crate::i18n::{I18n, Translations};
use crate::scheduler::{JobRegistry, SchedulerHandle};
use crate::user::User;

// ---------------------------------------------------------------------------
// Build the job registry
// ---------------------------------------------------------------------------

fn build_registry() -> Arc<JobRegistry> {
    let mut registry = JobRegistry::new();
    registry.register(jobs::inbox_discover::InboxDiscoverJob);
    registry.register(jobs::inbox_process::InboxProcessJob);
    registry.register(jobs::inbox_process::FileProcessJob);
    registry.register(jobs::archive_cleanup::ArchiveCleanupJob);
    registry.register(jobs::artwork_backfill::ArtworkBackfillJob);
    registry.register(jobs::metadata_backfill::MetadataBackfillJob);
    registry.register(jobs::lastfm_popularity::LastfmPopularityJob);
    registry.register(jobs::lastfm_scrobble::LastfmScrobbleJob);
    Arc::new(registry)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct IndexQuery {
    track: Option<i64>,
    release: Option<i64>,
    playlist_share: Option<String>,
}

async fn index(
    session: Session,
    db: Database,
    i18n: I18n,
    UrlQuery(query): UrlQuery<IndexQuery>,
) -> cot::Result<cot::response::Response> {
    let _user = match auth::get_session_user(&session, &db).await {
        Some(u) => u,
        None => {
            if let Some(location) = share_query_redirect(&query) {
                auth::remember_post_login_redirect(&session, &location).await?;
            }
            return Ok(auth::redirect("/login"));
        }
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    let template = player::PlayerPageTemplate {
        t: i18n.t,
        downloads_enabled: config.downloads_enabled,
        torrent_downloads_enabled: config.downloads_enabled && config.torrent_downloads_enabled,
        youtube_downloads_enabled: config.downloads_enabled && config.youtube_downloads_enabled,
    };
    Html::new(template.render()?).into_response()
}

fn share_query_redirect(query: &IndexQuery) -> Option<String> {
    if let Some(track_id) = query.track.filter(|id| *id > 0) {
        return Some(format!("/?track={track_id}"));
    }
    if let Some(release_id) = query.release.filter(|id| *id > 0) {
        return Some(format!("/?release={release_id}"));
    }
    let token = query.playlist_share.as_deref()?.trim();
    if is_share_token(token) {
        Some(format!("/?playlist_share={token}"))
    } else {
        None
    }
}

fn is_share_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Deserialize)]
struct SetLangQuery {
    lang: String,
    next: Option<String>,
}

async fn set_lang(
    UrlQuery(query): UrlQuery<SetLangQuery>,
) -> cot::Result<cot::http::Response<Body>> {
    let lang = i18n::Lang::from_code(&query.lang).unwrap_or(i18n::Lang::En);
    let next = query.next.as_deref().unwrap_or("/");

    let response = cot::http::Response::builder()
        .status(cot::http::StatusCode::SEE_OTHER)
        .header(cot::http::header::LOCATION, next)
        .header(cot::http::header::SET_COOKIE, i18n::lang_cookie(lang))
        .body(Body::fixed(""))
        .expect("valid response");

    Ok(response)
}

// ---------------------------------------------------------------------------
// Login page
// ---------------------------------------------------------------------------

#[derive(Debug, Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    t: &'static Translations,
    auth_password_enabled: bool,
    auth_sso_enabled: bool,
    oidc_button_text: String,
    message: String,
}

async fn login_page_handler(
    i18n: I18n,
    _startup_config: &AppConfig,
    db: Database,
    message: String,
) -> cot::Result<Html> {
    let (config, _) = AppConfig::load_with_db(&db).await;
    let template = LoginTemplate {
        t: i18n.t,
        auth_password_enabled: config.auth_password_enabled,
        auth_sso_enabled: config.auth_sso_enabled,
        oidc_button_text: config.oidc_button_text,
        message,
    };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct LoginQuery {
    error: Option<String>,
}

#[derive(Deserialize)]
struct SharePathId {
    id: i64,
}

#[derive(Deserialize)]
struct SharePathToken {
    token: String,
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

async fn logout_handler(session: Session) -> cot::Result<cot::response::Response> {
    auth::logout(&session).await?;
    Ok(auth::redirect("/login"))
}

async fn metrics_handler(
    config: Arc<AppConfig>,
    pool: Arc<tokio::sync::OnceCell<sqlx::PgPool>>,
) -> cot::Result<cot::http::Response<Body>> {
    if config.database_url.is_empty() {
        return Ok(cot::http::Response::builder()
            .status(cot::http::StatusCode::SERVICE_UNAVAILABLE)
            .header(cot::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")
            .body(Body::fixed("furumusic_metrics_unavailable 1\n"))
            .expect("valid response"));
    }
    let pg_pool = pool
        .get_or_init(|| async {
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&config.database_url)
                .await
                .expect("metrics pool")
        })
        .await;
    let body = metrics::render(pg_pool, &config).await;
    Ok(cot::http::Response::builder()
        .status(cot::http::StatusCode::OK)
        .header(cot::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::fixed(body))
        .expect("valid response"))
}

async fn share_track_handler(
    session: Session,
    db: Database,
    Path(path): Path<SharePathId>,
) -> cot::Result<cot::response::Response> {
    let location = if path.id > 0 {
        format!("/?track={}", path.id)
    } else {
        "/".to_string()
    };
    if auth::get_session_user(&session, &db).await.is_none() {
        auth::remember_post_login_redirect(&session, &location).await?;
        return Ok(auth::redirect("/login"));
    }
    Ok(auth::redirect(&location))
}

async fn share_release_handler(
    session: Session,
    db: Database,
    Path(path): Path<SharePathId>,
) -> cot::Result<cot::response::Response> {
    let location = if path.id > 0 {
        format!("/?release={}", path.id)
    } else {
        "/".to_string()
    };
    if auth::get_session_user(&session, &db).await.is_none() {
        auth::remember_post_login_redirect(&session, &location).await?;
        return Ok(auth::redirect("/login"));
    }
    Ok(auth::redirect(&location))
}

async fn share_playlist_handler(
    session: Session,
    db: Database,
    Path(path): Path<SharePathToken>,
) -> cot::Result<cot::response::Response> {
    let token = path.token.trim();
    let location = if is_share_token(token) {
        format!("/?playlist_share={token}")
    } else {
        "/".to_string()
    };
    if auth::get_session_user(&session, &db).await.is_none() {
        auth::remember_post_login_redirect(&session, &location).await?;
        return Ok(auth::redirect("/login"));
    }
    Ok(auth::redirect(&location))
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct FuruApp {
    config: Arc<AppConfig>,
    pool: Arc<tokio::sync::OnceCell<sqlx::PgPool>>,
}

impl App for FuruApp {
    fn name(&self) -> &'static str {
        env!("CARGO_PKG_NAME")
    }

    fn router(&self) -> Router {
        Router::with_urls([
            Route::with_handler_and_name(
                "/admin",
                get(|| async { Ok::<_, cot::Error>(auth::redirect("/admin/")) }),
                "admin_redirect",
            ),
            Route::with_handler_and_name(
                "/swagger",
                get(|| async { Ok::<_, cot::Error>(auth::redirect("/swagger/")) }),
                "swagger_redirect",
            ),
            Route::with_handler_and_name(
                "/",
                |session: Session, db: Database, i18n: I18n, query: UrlQuery<IndexQuery>| async move {
                    index(session, db, i18n, query).await
                },
                "index",
            ),
            Route::with_handler_and_name(
                "/share/track/{id}",
                get(share_track_handler),
                "share_track",
            ),
            Route::with_handler_and_name(
                "/share/release/{id}",
                get(share_release_handler),
                "share_release",
            ),
            Route::with_handler_and_name(
                "/share/playlist/{token}",
                get(share_playlist_handler),
                "share_playlist",
            ),
            Route::with_handler_and_name(
                "/metrics",
                get({
                    let config = Arc::clone(&self.config);
                    let pool = Arc::clone(&self.pool);
                    move || {
                        let config = Arc::clone(&config);
                        let pool = Arc::clone(&pool);
                        async move { metrics_handler(config, pool).await }
                    }
                }),
                "metrics",
            ),
            Route::with_handler_and_name(
                "/login",
                get({
                    let config = Arc::clone(&self.config);
                    move |i18n: I18n, db: Database, query: UrlQuery<LoginQuery>| {
                        let config = Arc::clone(&config);
                        async move {
                            // No users at all → redirect to first-run setup
                            if User::count_all(&db).await.unwrap_or(0) == 0 {
                                return Ok(auth::redirect("/admin/setup"));
                            }
                            let message = query.0.error.unwrap_or_default();
                            login_page_handler(i18n, &config, db, message)
                                .await?
                                .into_response()
                        }
                    }
                })
                .post({
                    let config = Arc::clone(&self.config);
                    move |i18n: I18n,
                          db: Database,
                          session: Session,
                          form: RequestForm<LoginForm>| {
                        let config = Arc::clone(&config);
                        async move {
                            let RequestForm(result) = form;
                            let data = match result {
                                FormResult::Ok(data) => data,
                                FormResult::ValidationError(_) => {
                                    metrics::record_auth_attempt(
                                        "password",
                                        "failure",
                                        "validation_error",
                                    );
                                    let msg = i18n.t.login_invalid.to_owned();
                                    return login_page_handler(i18n, &config, db, msg)
                                        .await?
                                        .into_response();
                                }
                            };

                            let (live_config, _) = AppConfig::load_with_db(&db).await;
                            if !live_config.auth_password_enabled {
                                metrics::record_auth_attempt("password", "failure", "disabled");
                                let msg = i18n.t.login_disabled.to_owned();
                                return login_page_handler(i18n, &config, db, msg)
                                    .await?
                                    .into_response();
                            }

                            // Try to authenticate
                            if let Ok(Some(user)) = User::get_by_username(&db, &data.username).await
                            {
                                if let Some(hash) = user.password_ref() {
                                    let password = Password::new(&data.password);
                                    match hash.verify(&password) {
                                        PasswordVerificationResult::Ok
                                        | PasswordVerificationResult::OkObsolete(_) => {
                                            let redirect_to =
                                                auth::get_post_login_redirect(&session)
                                                    .await?
                                                    .unwrap_or_else(|| "/".to_string());
                                            auth::login(&session, user.id_val()).await?;
                                            auth::clear_post_login_redirect(&session).await?;
                                            metrics::record_auth_attempt(
                                                "password", "success", "ok",
                                            );
                                            metrics::record_session_created("password");
                                            return Ok(auth::redirect(&redirect_to));
                                        }
                                        PasswordVerificationResult::Invalid => {}
                                    }
                                }
                            }

                            metrics::record_auth_attempt("password", "failure", "bad_credentials");
                            let msg = i18n.t.login_invalid.to_owned();
                            login_page_handler(i18n, &config, db, msg)
                                .await?
                                .into_response()
                        }
                    }
                }),
                "login",
            ),
            Route::with_handler_and_name("/logout", get(logout_handler), "logout"),
            Route::with_handler_and_name("/set-lang", set_lang, "set_lang"),
            Route::with_handler_and_name(
                "/auth/oidc/start",
                get(oidc::oidc_start_handler),
                "oidc_start",
            ),
            Route::with_handler_and_name(
                "/auth/oidc/callback",
                get(oidc::oidc_callback_handler),
                "oidc_callback",
            ),
            Route::with_handler_and_name(
                "/auth/mobile/oidc/start",
                get(oidc::oidc_mobile_start_handler),
                "mobile_oidc_start",
            ),
            Route::with_handler_and_name(
                "/auth/mobile/oidc/callback",
                get(oidc::oidc_mobile_callback_handler),
                "mobile_oidc_callback",
            ),
        ])
    }
}

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

struct FuruProject {
    app_config: Arc<AppConfig>,
    registry: Arc<JobRegistry>,
    scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
}

impl Project for FuruProject {
    fn cli_metadata(&self) -> CliMetadata {
        CliMetadata {
            description: concat!(
                env!("CARGO_PKG_DESCRIPTION"),
                "\n\n",
                "CONFIGURATION\n",
                "  All settings are available as FURU_-prefixed environment variables.\n",
                "  Priority: env var > DB override > compiled default.\n",
                "\n",
                "  Database (required for most features):\n",
                "    FURU_DATABASE_URL    PostgreSQL connection URL\n",
                "                         Example: postgres://user:pass@localhost/furumusic\n",
                "\n",
                "  Server:\n",
                "    FURU_LOG_LEVEL       Tracing filter (default: info)\n",
                "\n",
                "  Authentication:\n",
                "    FURU_AUTH_PASSWORD_ENABLED  Enable password login (default: true)\n",
                "    FURU_AUTH_SSO_ENABLED       Enable SSO/OIDC login (default: false)\n",
                "    FURU_OIDC_ISSUER            OIDC issuer URL\n",
                "    FURU_OIDC_CLIENT_ID         OIDC client ID\n",
                "    FURU_OIDC_CLIENT_SECRET      OIDC client secret\n",
                "    FURU_OIDC_BUTTON_TEXT        SSO button label (default: Sign in with SSO)\n",
                "    FURU_OIDC_ADMIN_GROUPS       OIDC groups that grant admin role\n",
                "    FURU_OIDC_USER_GROUPS        OIDC groups allowed to access the service\n",
                "\n",
                "  API:\n",
                "    FURU_SWAGGER_ENABLED   Enable Swagger UI at /swagger/ (default: false)\n",
                "\n",
                "QUICK START\n",
                "  export FURU_DATABASE_URL=postgres://user:pass@localhost/furumusic\n",
                "  furumusic run",
            ),
            ..cot::cli::metadata!()
        }
    }

    fn config(&self, _config_name: &str) -> cot::Result<ProjectConfig> {
        let mut builder = ProjectConfig::builder();
        builder.debug(cfg!(debug_assertions));

        if !self.app_config.database_url.is_empty() {
            builder.database(
                DatabaseConfig::builder()
                    .url(self.app_config.database_url.as_str())
                    .build(),
            );
            builder.middlewares(
                MiddlewareConfig::builder()
                    .session(
                        SessionMiddlewareConfig::builder()
                            .secure(false)
                            .same_site(SameSite::Lax)
                            .store(
                                SessionStoreConfig::builder()
                                    .store_type(SessionStoreTypeConfig::Database)
                                    .build(),
                            )
                            .build(),
                    )
                    .build(),
            );
        }

        Ok(builder.build())
    }

    fn middlewares(
        &self,
        handler: cot::project::RootHandlerBuilder,
        context: &cot::project::MiddlewareContext,
    ) -> cot::project::RootHandler {
        handler
            .middleware(metrics::MetricsLayer)
            .middleware(StaticFilesMiddleware::from_context(context))
            .middleware(SessionMiddleware::from_context(context))
            .build()
    }

    fn register_apps(&self, apps: &mut AppBuilder, _context: &RegisterAppsContext) {
        // Spawn the scheduler in background — it runs independently of HTTP
        // requests.  The OnceCell ensures it starts exactly once.
        let sched_cell = Arc::clone(&self.scheduler_handle);
        let sched_config = Arc::clone(&self.app_config);
        let sched_registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let _ = sched_cell
                .get_or_init(|| async {
                    match scheduler::start_scheduler(&sched_config, sched_registry).await {
                        Ok(handle) => handle,
                        Err(e) => {
                            tracing::error!("Failed to start scheduler: {e:#}");
                            panic!("scheduler failed to start: {e}");
                        }
                    }
                })
                .await;
        });

        // Join the federation at boot when it was left enabled (the settings
        // live in the config KV table; changes apply live from the admin).
        let fed_config = Arc::clone(&self.app_config);
        tokio::spawn(async move {
            federation::handle().boot(&fed_config).await;
        });

        // Embedding calculation is an independent, server-wide background
        // service. It remains useful locally when federation is disabled.
        let similarity_config = Arc::clone(&self.app_config);
        tokio::spawn(async move {
            similarity::handle().boot(&similarity_config).await;
        });

        apps.register(cot::session::db::SessionApp::new());
        apps.register_with_views(
            FuruApp {
                config: Arc::clone(&self.app_config),
                pool: Arc::new(tokio::sync::OnceCell::new()),
            },
            "",
        );
        apps.register_with_views(
            admin::AdminApp::new(
                Arc::clone(&self.app_config),
                Arc::clone(&self.registry),
                Arc::clone(&self.scheduler_handle),
            ),
            "/admin",
        );
        apps.register_with_views(api::ApiApp, "/api");
        apps.register_with_views(
            player::PlayerApp::new(
                Arc::clone(&self.app_config),
                Arc::clone(&self.scheduler_handle),
            ),
            "/api/player",
        );
        if self.app_config.swagger_enabled {
            apps.register_with_views(cot::openapi::swagger_ui::SwaggerUi::new(), "/swagger");
        }
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[cot::main]
fn main() -> impl Project {
    let app_config = Arc::new(AppConfig::load());

    // Initialise tracing subscriber with the configured log level.
    // FURU_LOG_LEVEL (or the default "info") is parsed as an EnvFilter
    // directive, so values like "debug", "warn,furumusic=trace" all work.
    let filter =
        tracing_subscriber::EnvFilter::try_new(&app_config.log_level).unwrap_or_else(|e| {
            eprintln!(
                "WARNING: invalid FURU_LOG_LEVEL {:?}: {e}; falling back to \"info\"",
                app_config.log_level,
            );
            tracing_subscriber::EnvFilter::new("info")
        });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tracing::info!("loaded config: {:?}", app_config);

    let registry = build_registry();

    FuruProject {
        app_config,
        registry,
        scheduler_handle: Arc::new(tokio::sync::OnceCell::new()),
    }
}
