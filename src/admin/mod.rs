mod v2;
pub mod views;

use std::sync::Arc;

use cot::App;
use cot::db::Database;
use cot::db::migrations::SyncDynMigration;
use cot::json::Json;
use cot::request::extractors::{Path, RequestForm, UrlQuery};
use cot::response::IntoResponse;
use cot::router::method::get;
use cot::router::{Route, Router};
use cot::session::Session;
use serde::Deserialize;

use crate::auth;
use crate::config::AppConfig;
use crate::i18n::I18n;
use crate::scheduler::{JobRegistry, SchedulerHandle};
use crate::user::User;
use views::{
    ArtistForm, CronForm, MetadataBackfillForm, OidcSettingsForm, ReleaseForm, ReviewApproveForm,
    ReviewsBulkForm, SetImageBody, SetupForm, UploadImageBody, UserForm,
};

#[derive(Debug, Deserialize)]
struct ReviewsQuery {
    status: Option<String>,
}

/// Build-time metadata baked in by `build.rs` and Cargo env vars.
#[derive(Debug)]
pub struct BuildInfo {
    pub pkg_name: &'static str,
    pub pkg_version: &'static str,
    pub profile: &'static str,
    pub target: &'static str,
    pub rustc_version: &'static str,
}

pub static BUILD_INFO: BuildInfo = BuildInfo {
    pkg_name: env!("CARGO_PKG_NAME"),
    pkg_version: env!("CARGO_PKG_VERSION"),
    profile: if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    },
    target: env!("FURU_TARGET"),
    rustc_version: env!("FURU_RUSTC_VERSION"),
};

pub struct AdminApp {
    config: Arc<AppConfig>,
    registry: Arc<JobRegistry>,
    scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
}

impl AdminApp {
    pub fn new(
        config: Arc<AppConfig>,
        registry: Arc<JobRegistry>,
        scheduler_handle: Arc<tokio::sync::OnceCell<Arc<SchedulerHandle>>>,
    ) -> Self {
        Self {
            config,
            registry,
            scheduler_handle,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SettingsQuery {
    saved: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PathId {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct PathName {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PathNameRunId {
    name: String,
    run_id: i64,
}

#[derive(Debug, Deserialize)]
struct ReleasesQuery {
    artist_id: Option<i64>,
}

impl App for AdminApp {
    fn name(&self) -> &'static str {
        "admin"
    }

    fn router(&self) -> Router {
        // Create a shared sqlx pool for admin routes that need it
        let pool_config = Arc::clone(&self.config);
        let pool: Arc<tokio::sync::OnceCell<sqlx::PgPool>> = Arc::new(tokio::sync::OnceCell::new());

        Router::with_urls([
            // -- Setup (first-run, no auth required) --------------------------
            Route::with_handler_and_name(
                "/setup",
                get(|i18n: I18n, db: Database| async move {
                    let count = User::count_all(&db).await.unwrap_or(1);
                    if count > 0 {
                        return Ok(auth::redirect("/admin/"));
                    }
                    views::setup_page(i18n, String::new())
                        .await?
                        .into_response()
                })
                .post(
                    |i18n: I18n, db: Database, session: Session,
                     form: RequestForm<SetupForm>| async move {
                        let count = User::count_all(&db).await.unwrap_or(1);
                        if count > 0 {
                            return Ok(auth::redirect("/admin/"));
                        }
                        views::setup_submit(i18n, &db, &session, form).await
                    },
                ),
                "admin_setup",
            ),
            // -- Admin v2 -----------------------------------------------------
            Route::with_handler_and_name(
                "/v2",
                |session: Session, db: Database, i18n: I18n| async move {
                    let count = User::count_all(&db).await.unwrap_or(0);
                    if count == 0 {
                        return Ok(auth::redirect("/admin/setup"));
                    }
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    v2::page(admin, i18n).await?.into_response()
                },
                "admin_v2",
            ),
            Route::with_handler_and_name(
                "/v2/api/dashboard",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let registry = Arc::clone(&self.registry);
                    get(move |session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        let registry = Arc::clone(&registry);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::dashboard(session, db, pg_pool, &registry).await
                        }
                    })
                },
                "admin_v2_dashboard",
            ),
            Route::with_handler_and_name(
                "/v2/api/reviews",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database,
                              query: UrlQuery<v2::ReviewsQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::reviews(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_v2_reviews",
            ),
            Route::with_handler_and_name(
                "/v2/api/reviews/bulk",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::BulkReviewsRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::bulk_reviews(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_reviews_bulk",
            ),
            Route::with_handler_and_name(
                "/v2/api/users",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database,
                              query: UrlQuery<v2::UsersQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::users(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_v2_users",
            ),
            Route::with_handler_and_name(
                "/v2/api/users/{id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database, path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::user_detail(session, db, pg_pool, path.0.id).await
                        }
                    })
                },
                "admin_v2_user_detail",
            ),
            Route::with_handler_and_name(
                "/v2/api/reviews/{id}/approve",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              path: Path<PathId>,
                              json: Json<v2::ReviewEditDto>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::approve_review(session, db, pg_pool, path.0.id, json).await
                            }
                        },
                    )
                },
                "admin_v2_review_approve",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    let registry = Arc::clone(&self.registry);
                    get(move |session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        let registry = Arc::clone(&registry);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::jobs(session, db, pg_pool, &registry).await
                        }
                    })
                },
                "admin_v2_jobs",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/metadata_backfill/run-options",
                cot::router::method::post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session,
                          db: Database,
                          json: Json<v2::MetadataBackfillRunRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::run_metadata_backfill(session, db, pg_pool, json).await
                        }
                    }
                }),
                "admin_v2_metadata_backfill_run_options",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/artwork_backfill/run-options",
                cot::router::method::post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session,
                          db: Database,
                          json: Json<v2::ArtworkBackfillRunRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::run_artwork_backfill(session, db, pg_pool, json).await
                        }
                    }
                }),
                "admin_v2_artwork_backfill_run_options",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/{name}/run",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>| {
                        let handle = Arc::clone(&handle);
                        async move { v2::run_job(session, db, &handle, &path.0.name).await }
                    }
                }),
                "admin_v2_job_run",
            ),
            Route::with_handler_and_name(
                "/v2/api/settings",
                get(move |session: Session, db: Database| async move {
                    v2::settings(session, db).await
                })
                .post(
                    move |session: Session,
                          db: Database,
                          json: Json<v2::UpdateSettingsRequest>| async move {
                        v2::update_settings(session, db, json).await
                    },
                ),
                "admin_v2_settings",
            ),
            Route::with_handler_and_name(
                "/v2/api/settings/probe",
                get(move |session: Session, db: Database| async move {
                    v2::settings_probe(session, db).await
                }),
                "admin_v2_settings_probe",
            ),
            Route::with_handler_and_name(
                "/v2/api/federation",
                get(move |session: Session, db: Database| async move {
                    v2::federation_status(session, db).await
                }),
                "admin_v2_federation_status",
            ),
            Route::with_handler_and_name(
                "/v2/api/similarity",
                get(move |session: Session, db: Database| async move {
                    v2::similarity_status(session, db).await
                }),
                "admin_v2_similarity_status",
            ),
            Route::with_handler_and_name(
                "/v2/api/similarity/clear",
                cot::router::method::post(move |session: Session, db: Database| async move {
                    v2::similarity_clear(session, db).await
                }),
                "admin_v2_similarity_clear",
            ),
            Route::with_handler_and_name(
                "/v2/api/federation/sync",
                cot::router::method::post(move |session: Session, db: Database| async move {
                    v2::federation_sync(session, db).await
                }),
                "admin_v2_federation_sync",
            ),
            Route::with_handler_and_name(
                "/v2/api/federation/ticket",
                get(move |session: Session, db: Database| async move {
                    v2::federation_ticket(session, db).await
                }),
                "admin_v2_federation_ticket",
            ),
            Route::with_handler_and_name(
                "/v2/api/federation/connect",
                cot::router::method::post(
                    move |session: Session,
                          db: Database,
                          json: Json<v2::FederationConnectRequest>| async move {
                        v2::federation_connect(session, db, json).await
                    },
                ),
                "admin_v2_federation_connect",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/{name}/toggle",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>| {
                        let handle = Arc::clone(&handle);
                        async move { v2::toggle_job(session, db, &handle, &path.0.name).await }
                    }
                }),
                "admin_v2_job_toggle",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/{name}/runs",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database, path: Path<PathName>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::job_runs(session, db, pg_pool, &path.0.name).await
                        }
                    })
                },
                "admin_v2_job_runs",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/{name}/runs/{run_id}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(
                        move |session: Session, db: Database, path: Path<PathNameRunId>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::job_run_detail(session, db, pg_pool, path.0.run_id).await
                            }
                        },
                    )
                },
                "admin_v2_job_run_detail",
            ),
            Route::with_handler_and_name(
                "/v2/api/library",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database,
                              query: UrlQuery<v2::LibraryQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::library(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_v2_library",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/item",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::UpdateLibraryItemRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::update_library_item(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_library_item",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/item/detail",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session,
                              db: Database,
                              query: UrlQuery<v2::LibraryItemDetailQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::library_item_detail(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_v2_library_item_detail",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/tracks/search",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session,
                              db: Database,
                              query: UrlQuery<v2::TrackSearchQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            v2::track_search(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_v2_library_tracks_search",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/item/image",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::SetLibraryImageRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::set_library_item_image(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_library_item_image",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/item/upload-image",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::UploadLibraryImageRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::upload_library_item_image(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_library_item_upload_image",
            ),
            Route::with_handler_and_name(
                "/v2/api/library/bulk",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::BulkLibraryRequest>| {
                            let pool = Arc::clone(&pool);
                            let pool_config = Arc::clone(&pool_config);
                            async move {
                                let pg_pool = pool
                                    .get_or_init(|| async {
                                        sqlx::postgres::PgPoolOptions::new()
                                            .max_connections(5)
                                            .connect(&pool_config.database_url)
                                            .await
                                            .expect("admin pool")
                                    })
                                    .await;
                                v2::bulk_library(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_library_bulk",
            ),
            // -- Dashboard ----------------------------------------------------
            Route::with_handler_and_name(
                "/",
                |session: Session, db: Database, i18n: I18n| async move {
                    let count = User::count_all(&db).await.unwrap_or(0);
                    if count == 0 {
                        return Ok::<cot::response::Response, cot::Error>(auth::redirect(
                            "/admin/setup",
                        ));
                    }
                    let _admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    let _ = i18n;
                    Ok::<cot::response::Response, cot::Error>(auth::redirect("/admin/v2"))
                },
                "admin_index",
            ),
            // -- Debug --------------------------------------------------------
            Route::with_handler_and_name(
                "/debug",
                {
                    let config = Arc::clone(&self.config);
                    move |session: Session, db: Database, i18n: I18n| {
                        let config = Arc::clone(&config);
                        async move {
                            let admin =
                                match auth::require_admin_or_redirect(&session, &db).await {
                                    Ok(u) => u,
                                    Err(resp) => return Ok(resp),
                                };
                            views::debug_handler(admin, i18n, &config, &db)
                                .await?
                                .into_response()
                        }
                    }
                },
                "admin_debug",
            ),
            // -- Settings -----------------------------------------------------
            Route::with_handler_and_name(
                "/settings",
                get({
                    let config = Arc::clone(&self.config);
                    move |session: Session, db: Database, i18n: I18n,
                          query: UrlQuery<SettingsQuery>| {
                        let config = Arc::clone(&config);
                        async move {
                            let admin =
                                match auth::require_admin_or_redirect(&session, &db).await {
                                    Ok(u) => u,
                                    Err(resp) => return Ok(resp),
                                };
                            let saved = query.0.saved.as_deref() == Some("1");
                            views::settings_handler(admin, i18n, &config, &db, saved)
                                .await?
                                .into_response()
                        }
                    }
                })
                .post({
                    let config = Arc::clone(&self.config);
                    move |session: Session, db: Database, i18n: I18n,
                          form: RequestForm<OidcSettingsForm>| {
                        let config = Arc::clone(&config);
                        async move {
                            let admin =
                                match auth::require_admin_or_redirect(&session, &db).await {
                                    Ok(u) => u,
                                    Err(resp) => return Ok(resp),
                                };
                            views::settings_submit(admin, i18n, &config, &db, form).await
                        }
                    }
                }),
                "admin_settings",
            ),
            // -- Settings probe (HTMX fragment) -----------------------------------
            Route::with_handler_and_name(
                "/settings/probe",
                {
                    let config = Arc::clone(&self.config);
                    move |session: Session, db: Database, i18n: I18n| {
                        let config = Arc::clone(&config);
                        async move {
                            let admin =
                                match auth::require_admin_or_redirect(&session, &db).await {
                                    Ok(u) => u,
                                    Err(resp) => return Ok(resp),
                                };
                            views::settings_probe_handler(admin, i18n, &config, &db)
                                .await?
                                .into_response()
                        }
                    }
                },
                "admin_settings_probe",
            ),
            // -- Users --------------------------------------------------------
            Route::with_handler_and_name(
                "/users",
                |session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::users_list(admin, i18n, &db).await?.into_response()
                },
                "admin_users",
            ),
            Route::with_handler_and_name(
                "/users/new",
                get(|session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::users_new(admin, i18n).await?.into_response()
                })
                .post(
                    |session: Session, db: Database, form: RequestForm<UserForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::users_create(admin, &db, form).await
                    },
                ),
                "admin_users_new",
            ),
            Route::with_handler_and_name(
                "/users/{id}/edit",
                get(
                    |session: Session, db: Database, i18n: I18n,
                     path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::users_edit(admin, i18n, &db, path.0.id)
                            .await?
                            .into_response()
                    },
                )
                .post(
                    |session: Session, db: Database, path: Path<PathId>,
                     form: RequestForm<UserForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::users_update(admin, &db, path.0.id, form).await
                    },
                ),
                "admin_users_edit",
            ),
            Route::with_handler_and_name(
                "/users/{id}/delete",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::users_delete(admin, &db, path.0.id).await
                    },
                ),
                "admin_users_delete",
            ),
            // -- Artists ------------------------------------------------------
            Route::with_handler_and_name(
                "/artists",
                |session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::artists_list(admin, i18n, &db).await?.into_response()
                },
                "admin_artists",
            ),
            Route::with_handler_and_name(
                "/artists/new",
                get(|session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::artists_new(admin, i18n).await?.into_response()
                })
                .post(
                    |session: Session, db: Database, form: RequestForm<ArtistForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_create(admin, &db, form).await
                    },
                ),
                "admin_artists_new",
            ),
            Route::with_handler_and_name(
                "/artists/{id}/edit",
                get(
                    |session: Session, db: Database, i18n: I18n,
                     path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_edit(admin, i18n, &db, path.0.id)
                            .await?
                            .into_response()
                    },
                )
                .post(
                    |session: Session, db: Database, path: Path<PathId>,
                     form: RequestForm<ArtistForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_update(admin, &db, path.0.id, form).await
                    },
                ),
                "admin_artists_edit",
            ),
            Route::with_handler_and_name(
                "/artists/{id}/delete",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_delete(admin, &db, path.0.id).await
                    },
                ),
                "admin_artists_delete",
            ),
            Route::with_handler_and_name(
                "/artists/{id}/available-covers",
                get(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_available_covers(admin, &db, path.0.id).await
                    },
                ),
                "admin_artists_available_covers",
            ),
            Route::with_handler_and_name(
                "/artists/{id}/set-image",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>,
                     json: Json<SetImageBody>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::artists_set_image(admin, &db, path.0.id, json.0).await
                    },
                ),
                "admin_artists_set_image",
            ),
            Route::with_handler_and_name(
                "/artists/{id}/upload-image",
                cot::router::method::post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session, db: Database, path: Path<PathId>,
                          json: Json<UploadImageBody>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new()
                                    .max_connections(3)
                                    .connect(&pool_config.database_url)
                                    .await
                                    .expect("admin pool")
                            }).await;
                            let (live_config, _) = AppConfig::load_with_db(&db).await;
                            views::artists_upload_image(admin, &db, pg_pool, &live_config, path.0.id, json.0).await
                        }
                    }
                }),
                "admin_artists_upload_image",
            ),
            // -- Releases -----------------------------------------------------
            Route::with_handler_and_name(
                "/releases",
                |session: Session, db: Database, i18n: I18n,
                 query: UrlQuery<ReleasesQuery>| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::releases_list(admin, i18n, &db, query.0.artist_id)
                        .await?
                        .into_response()
                },
                "admin_releases",
            ),
            Route::with_handler_and_name(
                "/releases/new",
                get(|session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::releases_new(admin, i18n, &db).await?.into_response()
                })
                .post(
                    |session: Session, db: Database,
                     form: RequestForm<ReleaseForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::releases_create(admin, &db, form).await
                    },
                ),
                "admin_releases_new",
            ),
            Route::with_handler_and_name(
                "/releases/{id}/edit",
                get(
                    |session: Session, db: Database, i18n: I18n,
                     path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::releases_edit(admin, i18n, &db, path.0.id)
                            .await?
                            .into_response()
                    },
                )
                .post(
                    |session: Session, db: Database, path: Path<PathId>,
                     form: RequestForm<ReleaseForm>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::releases_update(admin, &db, path.0.id, form).await
                    },
                ),
                "admin_releases_edit",
            ),
            {
                let pool = Arc::clone(&pool);
                let pool_config = Arc::clone(&pool_config);
                Route::with_handler_and_name(
                    "/releases/{id}/delete",
                    cot::router::method::post(move |session: Session, db: Database, path: Path<PathId>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool
                                .get_or_init(|| async {
                                    sqlx::postgres::PgPoolOptions::new()
                                        .max_connections(5)
                                        .connect(&pool_config.database_url)
                                        .await
                                        .expect("admin pool")
                                })
                                .await;
                            views::releases_delete(admin, &db, pg_pool, path.0.id).await
                        }
                    }),
                    "admin_releases_delete",
                )
            },
            // -- Media Files --------------------------------------------------
            Route::with_handler_and_name(
                "/media-files",
                |session: Session, db: Database, i18n: I18n| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::media_files_list(admin, i18n, &db).await?.into_response()
                },
                "admin_media_files",
            ),
            Route::with_handler_and_name(
                "/media-files/{id}/delete",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::media_files_delete(admin, &db, path.0.id).await
                    },
                ),
                "admin_media_files_delete",
            ),
            // -- Jobs ---------------------------------------------------------
            Route::with_handler_and_name(
                "/jobs",
                {
                    let registry = Arc::clone(&self.registry);
                    move |session: Session, db: Database, i18n: I18n| {
                        let registry = Arc::clone(&registry);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            views::jobs_list(admin, i18n, &db, &registry).await?.into_response()
                        }
                    }
                },
                "admin_jobs",
            ),
            Route::with_handler_and_name(
                "/jobs/metadata_backfill/run-options",
                cot::router::method::post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session, db: Database,
                          form: RequestForm<MetadataBackfillForm>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new()
                                    .max_connections(3)
                                    .connect(&pool_config.database_url)
                                    .await
                                    .expect("admin pool")
                            }).await;
                            views::metadata_backfill_run(admin, &db, pg_pool, form).await
                        }
                    }
                }),
                "admin_metadata_backfill_run",
            ),
            Route::with_handler_and_name(
                "/jobs/{name}/run",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>| {
                        let handle = Arc::clone(&handle);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            views::job_run_now(admin, &handle, &path.0.name).await
                        }
                    }
                }),
                "admin_job_run",
            ),
            Route::with_handler_and_name(
                "/jobs/{name}/toggle",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>| {
                        let handle = Arc::clone(&handle);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            views::job_toggle_enabled(admin, &db, &handle, &path.0.name).await
                        }
                    }
                }),
                "admin_job_toggle",
            ),
            Route::with_handler_and_name(
                "/jobs/{name}/cron",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>,
                          form: RequestForm<CronForm>| {
                        let handle = Arc::clone(&handle);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            views::job_update_cron(admin, &db, &handle, &path.0.name, form).await
                        }
                    }
                }),
                "admin_job_cron",
            ),
            Route::with_handler_and_name(
                "/jobs/{name}/runs/{run_id}",
                {
                    move |session: Session, db: Database, i18n: I18n,
                          path: Path<PathNameRunId>| {
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            views::job_run_detail(admin, i18n, &db, &path.0.name, path.0.run_id)
                                .await?
                                .into_response()
                        }
                    }
                },
                "admin_job_run_detail",
            ),
            Route::with_handler_and_name(
                "/jobs/{name}",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session, db: Database, i18n: I18n,
                          path: Path<PathName>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new()
                                    .max_connections(3)
                                    .connect(&pool_config.database_url)
                                    .await
                                    .expect("admin pool")
                            }).await;
                            views::job_detail(admin, i18n, &db, pg_pool, &path.0.name)
                                .await?
                                .into_response()
                        }
                    }
                },
                "admin_job_detail",
            ),
            // -- Reviews: clear -----------------------------------------------
            Route::with_handler_and_name(
                "/reviews/clear",
                cot::router::method::post(
                    |session: Session, db: Database,
                     query: UrlQuery<ReviewsQuery>| async move {
                        let admin =
                            match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                        views::reviews_clear(admin, &db, query.0.status.as_deref()).await
                    },
                ),
                "admin_reviews_clear",
            ),
            Route::with_handler_and_name(
                "/reviews/bulk",
                cot::router::method::post(
                    |session: Session, db: Database,
                     form: RequestForm<ReviewsBulkForm>| async move {
                        let admin =
                            match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                        views::reviews_bulk(admin, &db, form).await
                    },
                ),
                "admin_reviews_bulk",
            ),
            // -- Reviews ------------------------------------------------------
            Route::with_handler_and_name(
                "/reviews",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session, db: Database, i18n: I18n,
                          query: UrlQuery<ReviewsQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new()
                                    .max_connections(3)
                                    .connect(&pool_config.database_url)
                                    .await
                                    .expect("admin pool")
                            }).await;
                            views::reviews_list(admin, i18n, &db, pg_pool, query.0.status.as_deref())
                                .await?
                                .into_response()
                        }
                    }
                },
                "admin_reviews",
            ),
            Route::with_handler_and_name(
                "/reviews/{id}",
                |session: Session, db: Database, i18n: I18n,
                 path: Path<PathId>| async move {
                    let admin = match auth::require_admin_or_redirect(&session, &db).await {
                        Ok(u) => u,
                        Err(resp) => return Ok(resp),
                    };
                    views::review_detail(admin, i18n, &db, path.0.id)
                        .await?
                        .into_response()
                },
                "admin_review_detail",
            ),
            Route::with_handler_and_name(
                "/reviews/{id}/approve",
                cot::router::method::post({
                    let config = Arc::clone(&self.config);
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session, db: Database, path: Path<PathId>,
                          form: RequestForm<ReviewApproveForm>| {
                        let config = Arc::clone(&config);
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let admin = match auth::require_admin_or_redirect(&session, &db).await {
                                Ok(u) => u,
                                Err(resp) => return Ok(resp),
                            };
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new()
                                    .max_connections(3)
                                    .connect(&pool_config.database_url)
                                    .await
                                    .expect("admin pool")
                            }).await;
                            views::review_approve(admin, &config, &db, pg_pool, path.0.id, form).await
                        }
                    }
                }),
                "admin_review_approve",
            ),
            Route::with_handler_and_name(
                "/reviews/{id}/reject",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::review_reject(admin, &db, path.0.id).await
                    },
                ),
                "admin_review_reject",
            ),
            Route::with_handler_and_name(
                "/reviews/{id}/requeue",
                cot::router::method::post(
                    |session: Session, db: Database, path: Path<PathId>| async move {
                        let admin = match auth::require_admin_or_redirect(&session, &db).await {
                            Ok(u) => u,
                            Err(resp) => return Ok(resp),
                        };
                        views::review_requeue(admin, &db, path.0.id).await
                    },
                ),
                "admin_review_requeue",
            ),
        ])
    }

    fn migrations(&self) -> Vec<Box<SyncDynMigration>> {
        let mut all =
            cot::db::migrations::wrap_migrations(crate::config::db_migrations::MIGRATIONS);
        all.extend(cot::db::migrations::wrap_migrations(
            crate::user::db_migrations::MIGRATIONS,
        ));
        all.extend(cot::db::migrations::wrap_migrations(
            crate::music::db_migrations::MIGRATIONS,
        ));
        all.extend(cot::db::migrations::wrap_migrations(
            crate::scheduler::db_migrations::MIGRATIONS,
        ));
        all.extend(cot::db::migrations::wrap_migrations(
            crate::auth::db_migrations::MIGRATIONS,
        ));
        all
    }
}
