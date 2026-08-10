/// Application-level configuration for furumusic.
///
/// Every field is available both as a `FURU_`-prefixed environment variable
/// and through the admin UI.  The resolution order is:
///
///   env var  >  DB override  >  compiled default
///
/// Adding a new field to [`AppConfig`] automatically makes it settable via
/// the `FURU_<FIELD_NAME>` env var thanks to the [`impl_env_overrides`] macro.
use std::collections::HashMap;

use cot::db::migrations::{self, Field, Operation, SyncDynMigration};
use cot::db::{Database, DatabaseField, Identifier, LimitedString, Model};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ConfigSource — tracks where each field's effective value came from
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Default,
    Database,
    Env,
}

impl ConfigSource {
    pub fn code(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Database => "database",
            Self::Env => "env",
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigEntry — DB model for the furu__config table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[cot::db::model]
pub struct ConfigEntry {
    #[model(primary_key)]
    key: String,
    value: String,
}

impl ConfigEntry {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

pub mod db_migrations {
    use super::*;

    #[derive(Debug, Copy, Clone)]
    pub struct M0001CreateConfig;

    impl migrations::Migration for M0001CreateConfig {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0001_create_config";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] = &[];
        const OPERATIONS: &'static [Operation] = &[Operation::create_model()
            .table_name(Identifier::new("furu__config"))
            .fields(&[
                Field::new(
                    Identifier::new("key"),
                    <LimitedString<255> as DatabaseField>::TYPE,
                )
                .primary_key()
                .set_null(<LimitedString<255> as DatabaseField>::NULLABLE),
                Field::new(Identifier::new("value"), <String as DatabaseField>::TYPE)
                    .set_null(<String as DatabaseField>::NULLABLE),
            ])
            .build()];
    }

    // -- M0002: rename furu__config → furumusic__config_entry ---------------

    #[cot::db::migrations::migration_op]
    async fn rename_config_table(ctx: migrations::MigrationContext<'_>) -> cot::db::Result<()> {
        ctx.db
            .raw("ALTER TABLE furu__config RENAME TO furumusic__config_entry")
            .await?;
        Ok(())
    }

    #[derive(Debug, Copy, Clone)]
    pub struct M0002RenameConfigTable;

    impl migrations::Migration for M0002RenameConfigTable {
        const APP_NAME: &'static str = "furumusic";
        const MIGRATION_NAME: &'static str = "m_0002_rename_config_table";
        const DEPENDENCIES: &'static [migrations::MigrationDependency] =
            &[migrations::MigrationDependency::migration(
                "furumusic",
                "m_0001_create_config",
            )];
        const OPERATIONS: &'static [Operation] = &[Operation::custom(rename_config_table).build()];
    }

    pub const MIGRATIONS: &[&SyncDynMigration] = &[&M0001CreateConfig, &M0002RenameConfigTable];
}

// ---------------------------------------------------------------------------
// ConfigSources — parallel struct tracking the source of each field
// ---------------------------------------------------------------------------

pub struct ConfigSources {
    pub database_url: ConfigSource,
    pub oidc_issuer: ConfigSource,
    pub oidc_client_id: ConfigSource,
    pub oidc_client_secret: ConfigSource,
    pub log_level: ConfigSource,
    pub auth_password_enabled: ConfigSource,
    pub auth_sso_enabled: ConfigSource,
    pub oidc_button_text: ConfigSource,
    pub oidc_admin_groups: ConfigSource,
    pub oidc_user_groups: ConfigSource,
    pub swagger_enabled: ConfigSource,
    pub agent_enabled: ConfigSource,
    pub agent_inbox_dir: ConfigSource,
    pub agent_storage_dir: ConfigSource,
    pub agent_llm_url: ConfigSource,
    pub agent_llm_model: ConfigSource,
    pub agent_llm_auth: ConfigSource,
    pub agent_confidence_threshold: ConfigSource,
    pub agent_context_limit: ConfigSource,
    pub agent_concurrency: ConfigSource,
    pub lastfm_api_key: ConfigSource,
    pub lastfm_shared_secret: ConfigSource,
    pub federation_enabled: ConfigSource,
    pub federation_network_id: ConfigSource,
    pub federation_save_on_listen: ConfigSource,
    pub similarity_enabled: ConfigSource,
    pub similarity_model: ConfigSource,
    pub similarity_profile: ConfigSource,
    pub similarity_workers: ConfigSource,
}

impl Default for ConfigSources {
    fn default() -> Self {
        Self {
            database_url: ConfigSource::Default,
            oidc_issuer: ConfigSource::Default,
            oidc_client_id: ConfigSource::Default,
            oidc_client_secret: ConfigSource::Default,
            log_level: ConfigSource::Default,
            auth_password_enabled: ConfigSource::Default,
            auth_sso_enabled: ConfigSource::Default,
            oidc_button_text: ConfigSource::Default,
            oidc_admin_groups: ConfigSource::Default,
            oidc_user_groups: ConfigSource::Default,
            swagger_enabled: ConfigSource::Default,
            agent_enabled: ConfigSource::Default,
            agent_inbox_dir: ConfigSource::Default,
            agent_storage_dir: ConfigSource::Default,
            agent_llm_url: ConfigSource::Default,
            agent_llm_model: ConfigSource::Default,
            agent_llm_auth: ConfigSource::Default,
            agent_confidence_threshold: ConfigSource::Default,
            agent_context_limit: ConfigSource::Default,
            agent_concurrency: ConfigSource::Default,
            lastfm_api_key: ConfigSource::Default,
            lastfm_shared_secret: ConfigSource::Default,
            federation_enabled: ConfigSource::Default,
            federation_network_id: ConfigSource::Default,
            federation_save_on_listen: ConfigSource::Default,
            similarity_enabled: ConfigSource::Default,
            similarity_model: ConfigSource::Default,
            similarity_profile: ConfigSource::Default,
            similarity_workers: ConfigSource::Default,
        }
    }
}

// ---------------------------------------------------------------------------
// Env-var helper
// ---------------------------------------------------------------------------

/// Read a single env var with the `FURU_` prefix, returning `None` when the
/// variable is absent and logging a warning when it is present but cannot be
/// parsed.
fn env_override<T: std::str::FromStr>(field: &str) -> Option<T> {
    let key = format!("FURU_{}", field.to_ascii_uppercase());
    match std::env::var(&key) {
        Ok(val) => match val.parse::<T>() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!("ignoring invalid value for {key}: {val:?}");
                None
            }
        },
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Macro: generates apply_env_overrides + apply_env_overrides_tracked
// ---------------------------------------------------------------------------

/// Generates two methods on [`AppConfig`]:
///
/// - `apply_env_overrides`: overwrites fields from `FURU_*` env vars (no source tracking).
/// - `apply_env_overrides_tracked`: same but also marks sources as [`ConfigSource::Env`].
macro_rules! impl_env_overrides {
    ($($field:ident),* $(,)?) => {
        impl AppConfig {
            /// Apply `FURU_*` environment variable overrides to self.
            pub fn apply_env_overrides(&mut self) {
                $(
                    if let Some(v) = env_override(stringify!($field)) {
                        self.$field = v;
                    }
                )*
            }

            /// Apply `FURU_*` environment variable overrides and record sources.
            pub fn apply_env_overrides_tracked(&mut self, sources: &mut ConfigSources) {
                $(
                    if let Some(v) = env_override(stringify!($field)) {
                        self.$field = v;
                        sources.$field = ConfigSource::Env;
                    }
                )*
            }
        }
    };
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// PostgreSQL connection URL.
    pub database_url: String,
    /// OIDC issuer URL.
    pub oidc_issuer: String,
    /// OIDC client ID.
    pub oidc_client_id: String,
    /// OIDC client secret.
    pub oidc_client_secret: String,
    /// Tracing log level filter (e.g. "info", "debug", "warn,furumusic=debug").
    pub log_level: String,
    /// Whether password-based login is enabled.
    pub auth_password_enabled: bool,
    /// Whether SSO (OIDC) login is enabled.
    pub auth_sso_enabled: bool,
    /// Label shown on the SSO login button.
    pub oidc_button_text: String,
    /// Comma-separated list of OIDC group names that grant admin role.
    pub oidc_admin_groups: String,
    /// Comma-separated list of OIDC group names that are allowed to use the service.
    pub oidc_user_groups: String,
    /// Whether the Swagger UI is served at /swagger/.
    pub swagger_enabled: bool,
    /// Whether the AI agent background loop is enabled.
    pub agent_enabled: bool,
    /// Directory to scan for incoming audio files.
    pub agent_inbox_dir: String,
    /// Directory for organized permanent storage.
    pub agent_storage_dir: String,
    /// LLM API URL (OpenAI-compatible).
    pub agent_llm_url: String,
    /// LLM model name.
    pub agent_llm_model: String,
    /// LLM Authorization header value (e.g. "Bearer sk-...").
    pub agent_llm_auth: String,
    /// Confidence threshold for auto-approval (0.0–1.0).
    pub agent_confidence_threshold: f64,
    /// LLM context window size in tokens. Chat history resets when approaching this limit.
    pub agent_context_limit: u64,
    /// Number of files to process in parallel via the LLM.
    pub agent_concurrency: u64,
    /// Last.fm API key for weekly popularity enrichment.
    pub lastfm_api_key: String,
    /// Last.fm shared secret for authenticated scrobbling calls.
    pub lastfm_shared_secret: String,
    /// Whether this server participates in the furumi federation (publishes
    /// its library into the shared DHT and serves audio to peers).
    pub federation_enabled: bool,
    /// Federation network id — the shared secret every peer of the network
    /// uses to find the others.
    pub federation_network_id: String,
    /// Whether a federated track requested for playback is imported into the
    /// shared local library. This is a server-wide administrator policy.
    pub federation_save_on_listen: bool,
    /// Whether local embedding calculation and similarity search are enabled.
    pub similarity_enabled: bool,
    /// Embedding model selected by the administrator.
    pub similarity_model: String,
    /// Audio preprocessing profile selected by the administrator.
    pub similarity_profile: String,
    /// Maximum number of concurrent CPU embedding workers.
    pub similarity_workers: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            oidc_issuer: String::new(),
            oidc_client_id: String::new(),
            oidc_client_secret: String::new(),
            log_level: "info".into(),
            auth_password_enabled: true,
            auth_sso_enabled: false,
            oidc_button_text: "Sign in with SSO".into(),
            oidc_admin_groups: String::new(),
            oidc_user_groups: String::new(),
            swagger_enabled: false,
            agent_enabled: false,
            agent_inbox_dir: String::new(),
            agent_storage_dir: String::new(),
            agent_llm_url: "http://localhost:8080".into(),
            agent_llm_model: "default".into(),
            agent_llm_auth: String::new(),
            agent_confidence_threshold: 0.85,
            agent_context_limit: 8192,
            agent_concurrency: 2,
            lastfm_api_key: String::new(),
            lastfm_shared_secret: String::new(),
            federation_enabled: false,
            federation_network_id: String::new(),
            federation_save_on_listen: false,
            similarity_enabled: false,
            similarity_model: "discogs-effnet-bsdynamic-1".into(),
            similarity_profile: "furumi-full-track-v1".into(),
            similarity_workers: std::thread::available_parallelism()
                .map(|count| (count.get() / 2).clamp(1, 4) as u64)
                .unwrap_or(1),
        }
    }
}

// Register every field that should be overridable via FURU_* env vars.
impl_env_overrides!(
    database_url,
    oidc_issuer,
    oidc_client_id,
    oidc_client_secret,
    log_level,
    auth_password_enabled,
    auth_sso_enabled,
    oidc_button_text,
    oidc_admin_groups,
    oidc_user_groups,
    swagger_enabled,
    agent_enabled,
    agent_inbox_dir,
    agent_storage_dir,
    agent_llm_url,
    agent_llm_model,
    agent_llm_auth,
    agent_confidence_threshold,
    agent_context_limit,
    agent_concurrency,
    lastfm_api_key,
    lastfm_shared_secret,
    federation_enabled,
    federation_network_id,
    federation_save_on_listen,
    similarity_enabled,
    similarity_model,
    similarity_profile,
    similarity_workers,
);

impl AppConfig {
    fn normalize_host_paths(&mut self) {
        self.agent_inbox_dir = crate::media_paths::resolve_config_path(&self.agent_inbox_dir);
        self.agent_storage_dir = crate::media_paths::resolve_config_path(&self.agent_storage_dir);
    }

    /// Build config: start from defaults, then overlay env vars.
    /// Used at startup before the DB is available (to get `database_url`).
    pub fn load() -> Self {
        let mut cfg = Self::default();
        cfg.apply_env_overrides();
        cfg.apply_startup_db_overrides();
        cfg.apply_env_overrides();
        cfg.normalize_host_paths();
        cfg
    }

    fn apply_startup_db_overrides(&mut self) {
        if self.database_url.is_empty() {
            return;
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            tracing::warn!("skipping startup DB config load from inside an existing Tokio runtime");
            return;
        }

        let database_url = self.database_url.clone();
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::warn!("failed to create runtime for startup DB config load");
            return;
        };

        let result = runtime.block_on(async move {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url)
                .await?;
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM furumusic__config_entry WHERE key = 'swagger_enabled'",
            )
            .fetch_optional(&pool)
            .await
        });

        match result {
            Ok(Some(value)) => match value.parse::<bool>() {
                Ok(value) => self.swagger_enabled = value,
                Err(_) => tracing::warn!("ignoring invalid DB config value for swagger_enabled"),
            },
            Ok(None) => {}
            Err(err) => tracing::warn!("failed to read startup DB config overrides: {err}"),
        }
    }

    /// Build config with full 3-layer resolution (default → DB → env) and
    /// track the source of each field.
    pub async fn load_with_db(db: &Database) -> (Self, ConfigSources) {
        let mut cfg = Self::default();
        let mut sources = ConfigSources::default();
        cfg.apply_db_overrides(db, &mut sources).await;
        cfg.apply_env_overrides_tracked(&mut sources);
        cfg.normalize_host_paths();
        (cfg, sources)
    }

    /// Query all rows from `furu__config` and overlay matching fields.
    async fn apply_db_overrides(&mut self, db: &Database, sources: &mut ConfigSources) {
        let rows = match ConfigEntry::objects().all(db).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to read furu__config: {e}");
                return;
            }
        };

        let map: HashMap<String, String> = rows
            .into_iter()
            .map(|entry| (entry.key.to_string(), entry.value))
            .collect();

        macro_rules! apply_db_field {
            ($field:ident) => {
                if let Some(val) = map.get(stringify!($field)) {
                    match val.parse() {
                        Ok(v) => {
                            self.$field = v;
                            sources.$field = ConfigSource::Database;
                        }
                        Err(_) => {
                            tracing::warn!(
                                "ignoring invalid DB config value for {}: {:?}",
                                stringify!($field),
                                val,
                            );
                        }
                    }
                }
            };
        }

        apply_db_field!(database_url);
        apply_db_field!(oidc_issuer);
        apply_db_field!(oidc_client_id);
        apply_db_field!(oidc_client_secret);
        apply_db_field!(log_level);
        apply_db_field!(auth_password_enabled);
        apply_db_field!(auth_sso_enabled);
        apply_db_field!(oidc_button_text);
        apply_db_field!(oidc_admin_groups);
        apply_db_field!(oidc_user_groups);
        apply_db_field!(swagger_enabled);
        apply_db_field!(agent_enabled);
        apply_db_field!(agent_inbox_dir);
        apply_db_field!(agent_storage_dir);
        apply_db_field!(agent_llm_url);
        apply_db_field!(agent_llm_model);
        apply_db_field!(agent_llm_auth);
        apply_db_field!(agent_confidence_threshold);
        apply_db_field!(agent_context_limit);
        apply_db_field!(agent_concurrency);
        apply_db_field!(lastfm_api_key);
        apply_db_field!(lastfm_shared_secret);
        apply_db_field!(federation_enabled);
        apply_db_field!(federation_network_id);
        apply_db_field!(federation_save_on_listen);
        apply_db_field!(similarity_enabled);
        apply_db_field!(similarity_model);
        apply_db_field!(similarity_profile);
        apply_db_field!(similarity_workers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = AppConfig::default();
        assert!(cfg.database_url.is_empty());
        assert_eq!(cfg.log_level, "info");
        assert!(!cfg.similarity_enabled);
        assert_eq!(cfg.similarity_model, crate::similarity::DEFAULT_MODEL_ID);
        assert_eq!(
            cfg.similarity_profile,
            crate::similarity::DEFAULT_PROFILE_ID
        );
        assert!((1..=4).contains(&cfg.similarity_workers));
    }

    #[test]
    fn resolves_relative_media_paths_from_working_dir() {
        let expected = std::env::current_dir()
            .unwrap()
            .join("media")
            .join("uploads")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            crate::media_paths::resolve_config_path("media/uploads"),
            expected
        );
    }

    #[test]
    fn keeps_absolute_windows_media_paths() {
        assert_eq!(
            crate::media_paths::resolve_config_path(r"C:\Users\ab\repos\furumusic\media\uploads"),
            "C:/Users/ab/repos/furumusic/media/uploads"
        );
    }

    // SAFETY: environment-mutating tests take ENV_LOCK before changing vars.
    unsafe fn set(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }
    unsafe fn unset(k: &str) {
        unsafe { std::env::remove_var(k) };
    }

    #[test]
    fn env_override_string_field() {
        let _guard = lock_env();
        unsafe {
            set("FURU_OIDC_ISSUER", "https://example.com");
        }
        let cfg = AppConfig::load();
        assert_eq!(cfg.oidc_issuer, "https://example.com");
        unsafe {
            unset("FURU_OIDC_ISSUER");
        }
    }

    #[test]
    fn env_override_bool_field() {
        let _guard = lock_env();
        unsafe {
            set("FURU_AUTH_SSO_ENABLED", "true");
        }
        let cfg = AppConfig::load();
        assert!(cfg.auth_sso_enabled);
        unsafe {
            unset("FURU_AUTH_SSO_ENABLED");
        }
    }

    #[test]
    fn source_tracking_env() {
        let _guard = lock_env();
        unsafe {
            set("FURU_OIDC_ISSUER", "https://tracked.example.com");
        }
        let mut cfg = AppConfig::default();
        let mut sources = ConfigSources::default();
        cfg.apply_env_overrides_tracked(&mut sources);
        assert_eq!(cfg.oidc_issuer, "https://tracked.example.com");
        assert_eq!(sources.oidc_issuer, ConfigSource::Env);
        assert_eq!(sources.database_url, ConfigSource::Default);
        unsafe {
            unset("FURU_OIDC_ISSUER");
        }
    }

    #[test]
    fn config_source_codes() {
        assert_eq!(ConfigSource::Default.code(), "default");
        assert_eq!(ConfigSource::Database.code(), "database");
        assert_eq!(ConfigSource::Env.code(), "env");
    }
}
