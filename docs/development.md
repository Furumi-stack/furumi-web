# Development

[Project overview](../README.md) · [Installation](setup.md) · [Admin UI conventions](admin-ui.md)

## Architecture

Furumusic is written in Rust on the
[Cot](https://cot.rs) web framework. PostgreSQL stores the catalog, accounts,
playlists, configuration, and background-job state. Audio inspection uses
Symphonia, torrent downloads use librqbit, and the shared `music-dht`/Frid
protocol stack provides decentralized catalog search, media transfer, and
connected-device synchronization.

The browser interface and JSON API are served by the same application. Import,
artwork, metadata enrichment, similarity indexing, and maintenance run as
durable background jobs rather than blocking playback requests.

## Build profiles

Development and test builds use basic optimization (`opt-level = 1`) for
dependencies to keep macOS unwind tables below the compact-unwind size limit.
Application code stays unoptimized; debug symbols, assertions and panic
unwinding remain enabled. The first build compiles dependencies with these
settings, then reuses them from Cargo's cache. Dependency stepping in a debugger
can be less exact because of optimization. Release settings are independent.

See the [Cargo profile override documentation](https://doc.rust-lang.org/cargo/reference/profiles.html#overrides) for dependency-specific settings.

## Container builds and CI

CI uses Rust 1.97.0 and cargo-chef 0.1.74 to cache compiled dependencies separately
from application sources. Buildx imports and exports intermediate layers through
`ultradesu/furumusic:buildcache` (`mode=max`). After the first build, changes to
source code, templates or prompts reuse the dependency layer when its recipe and
toolchain are unchanged. Both dependency and application builds use `--locked`.
Update the pinned build tools in `Dockerfile`; changing them rebuilds the cache.

## Contributing

Bug reports, design discussions, and patches are welcome. Before submitting a
change, run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Last.fm database tests use temporary tables on a disposable PostgreSQL database:

```bash
FURUMUSIC_LASTFM_TEST_DATABASE_URL=postgres://user:pass@localhost/test_db \
  cargo test lastfm_popularity -- --include-ignored
```

## Access and sharing tests

The access regression suite uses a disposable migrated database and a local OIDC
provider with signed tokens and PKCE:

```bash
node --test tests/*.test.cjs
FURU_ACCESS_TEST_DATABASE_URL=postgresql://... cargo test postgres_approval_survives_sso_changes_without_implicit_grants -- --ignored
# Fresh disposable instance on :18048, media root /tmp/furumi-access-media,
# PostgreSQL container furumi-access-test-pg, database furumi_access_test:
node tests/browser-access-sharing.cjs
```

The browser test requires Playwright (`PLAYWRIGHT_MODULE`) and Chrome
(`CHROME_PATH`), and uses local port 18049 for its OIDC provider. It checks
approval, SSO precedence and group removal, guest playback and downloads,
unauthorized track substitution, and desktop/mobile player behavior.

## Statistics tests

```bash
node --test tests/*.test.cjs
cargo test admin:: -- --skip postgres_statistics_drilldown_and_pagination
# Use a disposable PostgreSQL; this test creates connection-local temporary tables.
FURU_STATISTICS_TEST_DATABASE_URL=postgresql://... cargo test postgres_statistics_drilldown_and_pagination -- --ignored
```

`tests/browser-statistics.cjs` additionally exercises a fresh disposable instance
at `FURU_TEST_URL` (default `http://127.0.0.1:3018`) and seeds the disposable Docker
container `furumi-statistics-test-pg`. It requires Playwright (or
`PLAYWRIGHT_MODULE`) and Chrome (or `CHROME_PATH`). Never point this test at a
live service.

## Metadata Repair tests

```bash
node --test tests/admin_metadata_repair.test.cjs
cargo test admin::metadata_fix::tests
```

`tests/browser-metadata-repair.cjs` checks the chat and plan updates, a four-release
merge, deletion and restoration of tracks and artist credits, playlists, playback
queues, and undo conflicts. Run it only against a disposable instance and a
PostgreSQL container named `furumi-repair-test-pg`, with database
`furumi_repair_test`. Set `FURU_TEST_URL`, `FURU_REPAIR_TEST_PG`, `CHROME_PATH`, and
`PLAYWRIGHT_MODULE` as needed. It seeds `tests/metadata_repair_fixture.sql` after
creating the test administrator. Set `FURU_REPAIR_SKIP_SEED=1` for a seeded database.
With `FURU_REPAIR_TEST_MODEL=1`, it additionally checks the reported release-only
selection and full ordered tracklist against the configured model, applies and
undoes that plan, and verifies individual track exclusions.

## Import destination tests

```bash
node --test tests/import_playlists.test.cjs
# Disposable PostgreSQL; both tests use temporary tables.
FURU_IMPORT_TEST_DATABASE_URL=postgresql://... cargo test import_destinations -- --ignored
FURU_IMPORT_TEST_DATABASE_URL=postgresql://... cargo test torrent_playlist -- --ignored
```

`tests/browser-import-playlists.cjs` checks the file upload flow, database
reconciliation and the torrent picker on desktop and mobile. It requires a fresh
migrated instance with the import agent disabled at `FURU_TEST_URL` (default
`http://127.0.0.1:3019`), the disposable PostgreSQL container
`furumi-import-playlist-test-pg` with database `furumi_import_test`, and Playwright
and Chrome configured as above. It uses a local torrent preview fixture and
simulated imported tracks, without downloading external torrents.
