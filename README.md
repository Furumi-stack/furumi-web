# furumusic

Furumusic can join the decentralized Furumi federation while remaining a
complete local web player. Optional similarity search stores versioned audio
embeddings in PostgreSQL and uses signed two-level LSH summaries in a separate
DHT to discover compatible peers without a central recommendation index. The
shared `music-dht` layer owns routing and wire compatibility; model inference
and exact cosine ranking stay local to each instance.

Reusable web-app boilerplate: auth, OIDC/SSO, admin panel, user management, i18n, PostgreSQL.

Built with Rust ([cot](https://cot.rs) framework).

## Quick start

```bash
export FURU_DATABASE_URL=postgresql://user:pass@localhost/furumusic
cargo run
# Open http://localhost:8000/admin/setup to create the first admin account
```

## Project structure

```
Cargo.toml                  Project manifest and dependencies
build.rs                    Captures rustc version + target at compile time
src/
  main.rs                   Entrypoint; HTTP router, login/logout handlers, tracing init
  config.rs                 3-tier config system (default → DB → env); FURU_* env vars
  auth.rs                   Session auth, Role enum (Admin/User), login/logout/guards
  user.rs                   User + OidcLink DB models, CRUD, password hashing, migrations
  oidc.rs                   OIDC/SSO flow: discovery, PKCE, token exchange, user provisioning
  i18n/
    mod.rs                  Language resolution (cookie → Accept-Language → default), extractor
    phrases.rs              All UI strings in English and Russian (translations! macro)
  api/
    mod.rs                  JSON API endpoints (mounted at /api), session-based auth
  admin/
    mod.rs                  Admin sub-app router: dashboard, settings, users, debug, setup
    views.rs                Admin page handlers and templates
templates/
  base.html                 Root HTML layout with lang/title blocks
  login.html                Login page (password + optional SSO button)
  admin/
    layout.html             Admin sidebar/nav wrapper
    index.html              Admin dashboard
    debug.html              Build info + config table (with secret redaction)
    settings.html           OIDC and auth settings form
    setup.html              First-run admin account creation
    users.html              User list
    user_form.html          User create/edit form
```

## Architecture

### Config system (`src/config.rs`)

Every setting lives in `AppConfig` and is resolved in three layers:

1. **Compiled default** — `AppConfig::default()`
2. **Database override** — rows in the `furumusic__config_entry` table
3. **Environment variable** — `FURU_<FIELD_NAME>` (highest priority)

`ConfigSources` tracks where each field's effective value came from (shown in the admin debug page).

**To add a new config field:**

1. Add the field to `AppConfig` struct
2. Set its default in `AppConfig::default()`
3. Add the field to `ConfigSources` struct and its `Default` impl
4. Add it to the `impl_env_overrides!(…)` invocation
5. Add an `apply_db_field!()` call in `apply_db_overrides`
6. Add an `entry!()` line in `admin/views.rs → config_display_entries()`

### Auth (`src/auth.rs`)

Session-based authentication with two roles:

- **`Role::Admin`** — full access to admin panel
- **`Role::User`** — standard user

Key functions:
- `login(session, user_id)` — sets session, cycles session ID
- `logout(session)` — flushes session
- `get_session_user(session, db)` — returns `AuthenticatedUser` if active
- `require_admin_or_redirect(session, db)` — guard that returns 403 or redirects to `/login`

### OIDC/SSO (`src/oidc.rs`)

Full OpenID Connect authorization code flow with PKCE:

1. `GET /auth/oidc/start` — discovers provider, builds auth URL, stores CSRF/nonce/PKCE in session, redirects to IdP
2. `GET /auth/oidc/callback` — validates CSRF, exchanges code for tokens, verifies ID token, provisions user

Provider metadata is cached for 1 hour and invalidated when OIDC config changes.

**Group access and role mapping:** The `oidc_user_groups` config field lists OIDC group names (comma-separated) allowed to access the service. When it is set, users outside both `oidc_user_groups` and `oidc_admin_groups` are denied before provisioning/login. The `oidc_admin_groups` config field lists OIDC group names that grant the admin role. Groups are extracted from the `groups` claim in the ID token JWT payload.

**User provisioning order:**
1. Find existing `OidcLink` by issuer+sub → update claims, update role
2. Find existing `User` by email → create OidcLink, update role
3. Create new user (no password) + OidcLink

Stale links (pointing to deleted users) are cleaned up automatically.

### User model (`src/user.rs`)

Two database models:

- **`User`** — id, username (unique), password (optional for OIDC-only), email, display_name, avatar_url, role, is_active
- **`OidcLink`** — id, user_id, issuer, sub, email, name, avatar_url; unique index on (issuer, sub)

Migrations: M0003 (User table), M0004 (OidcLink table), M0005 (OidcLink indexes).

### i18n (`src/i18n/`)

Compile-time bilingual UI (English + Russian).

- `translations!` macro in `phrases.rs` generates a `Translations` struct with static `EN` and `RU` instances
- Language resolution: `furu_lang` cookie → `Accept-Language` header → English default
- `I18n` is a cot request extractor — handlers receive it automatically
- `set_lang` endpoint (`/set-lang?lang=ru&next=/`) sets the cookie

### API (`src/api/`)

JSON API mounted at `/api`. Uses the same session cookie as HTML pages — works automatically for same-origin frontend requests (no CORS, no tokens needed).

Helpers in `api/mod.rs`:
- `json_ok(value)` — 200 with `application/json`
- `json_error(status, message)` — error response as `{"error": "..."}`

| Route | Method | Description |
|-------|--------|-------------|
| `/api/me` | GET | Current user (id, name, role) or 401 |

**Swagger UI** is available at `/swagger/` when `FURU_SWAGGER_ENABLED=true`. The OpenAPI spec is auto-generated from handler types.

To add a new API endpoint:
1. Define request/response structs with `#[derive(Serialize, JsonSchema)]`
2. Write an async handler, return `Json(response).into_response()`
3. Add a `Route::with_api_handler_and_name(…, api_get(handler), …)` in `ApiApp::router()`
4. The endpoint appears automatically in Swagger UI

### Admin panel (`src/admin/`)

Mounted at `/admin`. All routes (except `/admin/setup`) require `Role::Admin`.

| Route | Purpose |
|-------|---------|
| `/admin/setup` | First-run: create initial admin (only works when zero users exist) |
| `/admin/` | Dashboard |
| `/admin/debug` | Build info, config values with sources, DB connectivity |
| `/admin/settings` | OIDC config, auth toggles (saved to DB config table) |
| `/admin/users` | User list |
| `/admin/users/new` | Create user |
| `/admin/users/{id}/edit` | Edit user |
| `/admin/users/{id}/delete` | Delete user (POST) |

## How to extend

### 1. Add a config field

See [Config system](#config-system-srcconfigrs) above — 6 locations to update.

### 2. Add a database model

1. Define a struct with `#[cot::db::model]` in a new or existing file
2. Write a migration struct implementing `cot::db::migrations::Migration`
3. Register the migration in the `AdminApp::migrations()` method in `src/admin/mod.rs`

### 3. Add a page

1. Create a template in `templates/`
2. Write a handler function that returns `Html`
3. Add a `Route::with_handler_and_name(…)` in the appropriate `router()` method
4. If admin-only, wrap with `require_admin_or_redirect`

### 4. Add a translation

Add a line to the `translations!` macro in `src/i18n/phrases.rs`:

```rust
my_key: "English text", "Русский текст";
```

Access it in handlers/templates as `i18n.t.my_key` (or `t.my_key` in templates).

### 5. Add an API endpoint

Same as adding a page, but return a JSON response instead of `Html`. The `json` feature is enabled in Cargo.toml.

## Environment variables

All prefixed with `FURU_`. Priority: env var > DB override > compiled default.

| Variable | Description | Default |
|----------|-------------|---------|
| `FURU_DATABASE_URL` | PostgreSQL connection URL | *(empty — required)* |
| `FURU_LOG_LEVEL` | Tracing filter (e.g. `info`, `debug`, `warn,furumusic=trace`) | `info` |
| `FURU_AUTH_PASSWORD_ENABLED` | Enable password login | `true` |
| `FURU_AUTH_SSO_ENABLED` | Enable SSO/OIDC login | `false` |
| `FURU_OIDC_ISSUER` | OIDC issuer URL | *(empty)* |
| `FURU_OIDC_CLIENT_ID` | OIDC client ID | *(empty)* |
| `FURU_OIDC_CLIENT_SECRET` | OIDC client secret | *(empty)* |
| `FURU_OIDC_BUTTON_TEXT` | SSO button label | `Sign in with SSO` |
| `FURU_OIDC_ADMIN_GROUPS` | Comma-separated OIDC groups that grant admin | *(empty)* |
| `FURU_OIDC_USER_GROUPS` | Comma-separated OIDC groups allowed to access the service. Empty means any authenticated SSO user is allowed. | *(empty)* |
| `FURU_SWAGGER_ENABLED` | Serve Swagger UI at `/swagger/` | `false` |
