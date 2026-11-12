mod management;
mod metadata_fix;
mod setup;
mod statistics;
mod v2;

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
use setup::SetupForm;

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
struct PathId {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct PathStringId {
    id: String,
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
                    setup::setup_page(i18n, String::new())
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
                        setup::setup_submit(i18n, &db, &session, form).await
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
                "/v2/api/statistics",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database,
                              query: UrlQuery<statistics::StatisticsQuery>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async {
                                sqlx::postgres::PgPoolOptions::new().max_connections(5)
                                    .connect(&pool_config.database_url).await.expect("admin pool")
                            }).await;
                            statistics::statistics(session, db, pg_pool, query.0).await
                        }
                    })
                },
                "admin_statistics",
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
                "/v2/api/users/manage",
                cot::router::method::post(management::manage_user),
                "admin_manage_user",
            ),
            Route::with_handler_and_name(
                "/v2/api/jobs/{name}/schedule",
                cot::router::method::post({
                    let handle = Arc::clone(&self.scheduler_handle);
                    move |session: Session, db: Database, path: Path<PathName>,
                          json: Json<management::ScheduleRequest>| {
                        let handle = Arc::clone(&handle);
                        async move {
                            management::schedule(session, db, &handle, &path.0.name, json).await
                        }
                    }
                }),
                "admin_job_schedule",
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
                "/v2/api/jobs/lastfm_popularity/run-options",
                cot::router::method::post({
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    move |session: Session,
                          db: Database,
                          json: Json<v2::LastfmPopularityRunRequest>| {
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
                            v2::run_lastfm_popularity(session, db, pg_pool, json).await
                        }
                    }
                }),
                "admin_v2_lastfm_popularity_run_options",
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
                "/v2/api/settings/youtube-cookies",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::UploadYoutubeCookieFileRequest>| {
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
                                v2::upload_youtube_cookie_file(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_youtube_cookie_upload",
            ),
            Route::with_handler_and_name(
                "/v2/api/settings/youtube-cookies/{id}",
                cot::router::method::delete(
                    move |session: Session, db: Database, path: Path<PathStringId>| async move {
                        v2::delete_youtube_cookie_file(session, db, &path.0.id).await
                    },
                ),
                "admin_v2_youtube_cookie_delete",
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
                                v2::job_run_detail(session, db, pg_pool, &path.0.name, path.0.run_id).await
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
            Route::with_handler_and_name(
                "/v2/api/library/releases/merge",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(
                        move |session: Session,
                              db: Database,
                              json: Json<v2::MergeReleasesRequest>| {
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
                                v2::merge_releases(session, db, pg_pool, json).await
                            }
                        },
                    )
                },
                "admin_v2_library_releases_merge",
            ),
            Route::with_handler_and_name(
                "/v2/api/metadata-fix/expand",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(move |session: Session, db: Database, json: Json<metadata_fix::ExpandRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async { sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&pool_config.database_url).await.expect("admin pool") }).await;
                            metadata_fix::expand(session, db, pg_pool, json).await
                        }
                    })
                },
                "admin_v2_metadata_fix_expand",
            ),
            Route::with_handler_and_name(
                "/v2/api/metadata-fix/plan",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(move |session: Session, db: Database, json: Json<metadata_fix::PlanRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async { sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&pool_config.database_url).await.expect("admin pool") }).await;
                            metadata_fix::propose(session, db, pg_pool, json).await
                        }
                    })
                },
                "admin_v2_metadata_fix_plan",
            ),
            Route::with_handler_and_name(
                "/v2/api/metadata-fix/apply",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(move |session: Session, db: Database, json: Json<metadata_fix::ApplyRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async { sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&pool_config.database_url).await.expect("admin pool") }).await;
                            metadata_fix::apply(session, db, pg_pool, json).await
                        }
                    })
                },
                "admin_v2_metadata_fix_apply",
            ),
            Route::with_handler_and_name(
                "/v2/api/metadata-fix/history",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    get(move |session: Session, db: Database| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async { sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&pool_config.database_url).await.expect("admin pool") }).await;
                            metadata_fix::history(session, db, pg_pool).await
                        }
                    })
                },
                "admin_v2_metadata_fix_history",
            ),
            Route::with_handler_and_name(
                "/v2/api/metadata-fix/undo",
                {
                    let pool = Arc::clone(&pool);
                    let pool_config = Arc::clone(&pool_config);
                    cot::router::method::post(move |session: Session, db: Database, json: Json<metadata_fix::UndoRequest>| {
                        let pool = Arc::clone(&pool);
                        let pool_config = Arc::clone(&pool_config);
                        async move {
                            let pg_pool = pool.get_or_init(|| async { sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&pool_config.database_url).await.expect("admin pool") }).await;
                            metadata_fix::undo(session, db, pg_pool, json).await
                        }
                    })
                },
                "admin_v2_metadata_fix_undo",
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
        all.extend(cot::db::migrations::wrap_migrations(
            crate::youtube::db_migrations::MIGRATIONS,
        ));
        all
    }
}

/// Start an options-based run for the admin interface. Ordinary scheduler
/// entry points continue to call Job::run with light-mode defaults.
async fn start_lastfm_popularity(
    db: &Database,
    pool: &sqlx::PgPool,
    options: crate::jobs::lastfm_popularity::LastfmPopularityOptions,
) -> cot::Result<i64> {
    use crate::scheduler::{JobContext, JobLog, JobRun};

    let mut run = JobRun::create_running(db, "lastfm_popularity", "manual")
        .await
        .map_err(|err| cot::Error::internal(format!("failed to create job run: {err}")))?;
    let run_id = run.id_val();
    let (live_config, _) = AppConfig::load_with_db(db).await;
    let db = db.clone();
    let pool = pool.clone();
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let ctx = JobContext {
            config: Arc::new(live_config),
            db: db.clone(),
            pool: pool.clone(),
            run_id,
            registry: Arc::new(JobRegistry::new()),
        };
        let mut log = JobLog::with_live_flush(pool, run_id);
        let result =
            crate::jobs::lastfm_popularity::run_with_options(&ctx, &mut log, options).await;
        let duration_ms = start.elapsed().as_millis() as i64;
        match result {
            Ok(()) => {
                let _ = run.set_completed(&db, duration_ms, &log.output()).await;
            }
            Err(err) => {
                let _ = run
                    .set_failed(&db, duration_ms, &log.output(), &err.to_string())
                    .await;
            }
        }
    });
    Ok(run_id)
}
