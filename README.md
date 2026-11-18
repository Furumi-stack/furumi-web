# Furumusic

**Your library. Your users. Your network.**

Furumusic is a self-hosted, multi-user music server with a full web player and
support for the Furumi federated network. It turns a collection of music files
into a shared library that is available from any modern browser, while every
user keeps their own playlists, likes, listening history, and playback state.

Music can be uploaded directly, imported from a `.torrent` file, or downloaded
from a magnet link. An optional AI-assisted import pipeline reads the available
tags and path information, reconstructs inconsistent metadata, finds artwork,
and organizes the result into artists, releases, and tracks. Uncertain matches
are kept for review instead of silently entering the library with bad data.

## Why Furumusic?

Furumusic is for a household, a small community, or anyone who wants one music
library without handing it to a subscription service. The server owns the
catalog and media files; the browser is only the player.

- one shared library with separate user accounts;
- a responsive web player with artists, releases, search, queue, and playlists;
- direct file uploads and imports from YouTube, torrent files, or magnet links;
- optional AI-assisted recognition and normalization of metadata;
- password login or OIDC/SSO with group grants and local administrator approval;
- public track, release, and queue links with scoped guest playback and downloads;
- optional federation without a central catalog or search service;
- trusted-device pairing, playback handoff, and synchronization between Furumi
  players;
- Last.fm scrobbling and similarity-based discovery when configured.

Furumusic does not include music. Import only media you are allowed to store and
share.

## Access and public sharing

OIDC identifies accounts by issuer and subject. Matching admin groups take
precedence over matching user groups, and either SSO grant takes precedence over
a local role. Without matching groups (including an empty group configuration),
a new account waits for approval in **Admin → Users**. Any instance administrator
can approve it as a user or administrator. The waiting page updates automatically.
On subsequent SSO logins the group grant is refreshed: if it disappears, only an
independently assigned local role preserves access. Existing password accounts
keep their local roles; existing OIDC-only accounts retain their last SSO grant
until the next login, without receiving an implicit local approval.

Sharing creates an unguessable `/share/{token}` link to a snapshot of track IDs,
including their order and repetitions. Guests and accounts awaiting approval can
play and download only the tracks in that snapshot, using the regular player.
They cannot browse the library, search, use recommendations, or write personal
history. Signing in from a share returns to that share; signing in from the root
shows only the approval page until access is granted. Track info offers a browser
download of the original audio file. The old radio implementation is removed.

Migration `m_0053_local_approval_and_public_shares` deletes the old share records.
Old track/release/playlist URL routes are removed, with no compatibility layer.
Link expiry, cleanup, and revocation are not part of this implementation.

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

## Importing music

Users can add audio files from the web interface or ask the server to download
selected files from a torrent. Both routes feed the same import pipeline, so a
library remains consistent regardless of where its files came from.

The importer combines embedded tags, filenames, folder structure, and existing
catalog context. With an OpenAI-compatible language model configured, it can
normalize artist and release names, separate featured artists, recover track
numbers and release types, and flag ambiguous results for a user to approve.
Cover art is taken from nearby image files or embedded artwork when available.

The inbox and permanent library are separate directories. New files are first
processed in the inbox and are moved into the organized library only after
their metadata has been accepted.

## Federation

Federation is optional. Independent Furumusic and Furumi players that use the
same Network ID discover one another as a logical network. Each peer keeps its
own library and can continue working alone.

When federation is enabled, local and remote artists, releases, and tracks can
appear in the same search and library views. Missing audio is streamed from an
available peer and can optionally be retained locally. Similarity search also
stays decentralized: embeddings and exact ranking remain on the instance that
owns the music.

Trusted-device pairing is separate from public catalog discovery. It connects
a user's own Furumi players so likes, playlists, playback state, and control can
move between approved devices.

## Build and run

Furumusic requires PostgreSQL and a Rust toolchain with Rust 2024 edition
support. Create an empty database, provide its connection URL, and start the
server:

```bash
export FURU_DATABASE_URL='postgresql://furumusic:password@127.0.0.1/furumusic'
cargo run --release --locked
```

Open <http://127.0.0.1:8000/admin/setup> to create the first administrator.
After setup, configure the inbox and library directories under **Admin →
Settings** before importing music.

To listen on another address or port:

```bash
cargo run --release --locked -- -l 0.0.0.0:8000
```

A Nix development shell is included for Linux and macOS:

```bash
nix develop
cargo run --locked
```

The repository also contains a multi-stage `Dockerfile` for building a small
runtime image. A deployment must provide PostgreSQL plus persistent, writable
volumes for the inbox and music library.

CI uses Rust 1.97.0 and cargo-chef 0.1.74 to cache compiled dependencies separately
from application sources. Buildx imports and exports intermediate layers through
`ultradesu/furumusic:buildcache` (`mode=max`). After the first build, changes to
source code, templates or prompts reuse the dependency layer when its recipe and
toolchain are unchanged. Both dependency and application builds use `--locked`.
Update the pinned build tools in `Dockerfile`; changing them rebuilds the cache.

## Admin statistics

The only admin interface is `/admin/v2`, also reachable from `/admin/`.
**Statistics** shows listening totals, a timeline, artist/track/release rankings,
and paginated history. Select an artist or track to see its listeners, then a
listener to see their history within the same filters. The **Users** list links
to each user's full statistics.

Periods use UTC. Counts come from the synchronized `listen_event` history: a
natural finish, half of a track, or four minutes qualifies as a play. Interrupted
sessions under five seconds are not retained. Short retained sessions contribute
to listened time but not play counts. The source filter separates this instance
from synchronized devices. Artist rankings use recorded artist names, including
featured credits; the same artist is counted once per event. Existing history
remains available when its local audio file is deleted. Library and service
cards show current instance totals, independently of the listening filters.

Migration `m_0051_add_listen_statistics_indexes` adds time and content indexes.
The old admin pages and `/admin/debug` are removed; first-run setup remains at
`/admin/setup`.

Regression checks:

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

## Metadata Repair

Admin → Library Tools provides a chat for editing the selected library resources.
Use **Show related**, then **Deselect all** to choose only the resources needed
for the task. Selecting a release includes its tracks; individual tracks can
then be unchecked to exclude them. Unselected artists remain read-only context
and their IDs can still be used in credits. Releases show their years; tracks
show their source release.

The assistant sees the complete selected scope and replaces the proposed plan
after every clarification. Expand **Proposed changes** in the chat to review or
edit it before applying. Plans support moving tracks, creating releases, merging
releases, and deleting artists, releases, or tracks. Release deletion requires
explicit moves or deletion of all its tracks. Artist deletion detaches credits
and requires all credited releases and tracks to be included.

Applied plans are transactional and can be undone from History. Metadata Repair
retains audio and artwork files for Undo. Undo refuses to overwrite subsequent
metadata, playback, or federation changes.

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

## Importing into playlists

The player's download manager offers **Add downloads to a playlist** for file
and folder uploads and torrents, as well as YouTube. Choose an existing owned
playlist or create one before starting; leaving the choice empty imports only
into the library. Selected audio tracks are appended after processing or manual
approval, including files resolved to tracks already in the library.

Migration `m_0052_add_import_playlist_destinations` persists the destination and
completion receipts. Retries and restarts retain the choice, avoid duplicates,
and do not re-add tracks subsequently removed by the user. A started batch keeps
its destination for its remaining files.

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

## Configuration

Most settings can be changed from the administration interface. Every setting
also has a `FURU_`-prefixed environment variable; environment values take
priority over values stored in PostgreSQL.

The settings needed for a useful first installation are:

| Setting | Purpose |
| --- | --- |
| `FURU_DATABASE_URL` | PostgreSQL connection URL; required to run the service |
| `FURU_AGENT_INBOX_DIR` | Temporary inbox for uploads and downloaded files |
| `FURU_AGENT_STORAGE_DIR` | Permanent, organized music library |
| `FURU_AGENT_ENABLED` | Enables the background metadata import pipeline |
| `FURU_AGENT_LLM_URL` | Base URL of an OpenAI-compatible model server |
| `FURU_AGENT_LLM_MODEL` | Model used to recognize and normalize metadata |
| `FURU_FEDERATION_ENABLED` | Publishes the library and enables peer discovery |
| `FURU_FEDERATION_NETWORK_ID` | Joins peers with the same value into one logical network |

AI recognition, federation, similarity search, Last.fm, and OIDC are optional.
A local password-authenticated server can be used without any of them.

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

The `lastfm_popularity` job defaults to **light** mode: it requests popularity
only for tracks without a completed lookup or with a changed title or primary
artist. Both matches (including zero counts) and explicit Last.fm “Track not
found” results are skipped on subsequent light runs. HTTP errors, timeouts,
invalid responses, and other API errors remain eligible for retry. A failed
full refresh also makes that track eligible for the next light run.

Choose **Full: refresh all visible tracks** in the task's admin parameters to
refresh existing statistics and retry previous misses. The options endpoint
`POST /admin/v2/api/jobs/lastfm_popularity/run-options` accepts
`{"refresh_existing": true}`; an empty object, scheduled runs, and the ordinary
run endpoint use light mode. Neither mode limits the number of tracks processed.
An authoritative miss clears current statistics while preserving their history.
The migration preserves existing popularity; legacy misses need one more lookup
because older versions did not persist them.

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
