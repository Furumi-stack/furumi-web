# Lyrics

Lyrics are attached to exact audio bytes, not a mutable title or a library row.
PostgreSQL stores versions under the audio file's SHA-256 hash; the optional
federation API resolves the existing canonical `b3:<hex>` content ID to those
same bytes through the federation content cache. Identical files share texts.

Each version has an opaque ID, label, provenance, optional provider ID/language,
plain text and/or synchronized LRC. An explicit instrumental marker is also
supported. There are at most 16 versions per content item, with a 256 KiB limit
per text representation. Timed versions are returned first. The web player
selects the first version and lets the listener choose another without saving
that preference.

## Enrichment

Admin Settings → **Lyrics · LRCLIB** controls only automatic enrichment.
`FURU_LYRICS_ENRICHMENT_ENABLED` defaults to `true` and overrides the saved
setting. Turning it off stops external requests, including during an active
run; it does not hide or delete stored lyrics or disable manual editing.
LRCLIB receives track titles, artist names and the instance's request IP.

The `lyrics_backfill` job runs daily at 04:00 in the scheduler's timezone. It
can also be run or rescheduled in Admin → Jobs. A run processes pending audio
items in database batches of 200 until the backlog is exhausted or the service
requires a retry cooldown. An interrupted run resumes from saved progress on
the next run. It works independently of the AI agent setting.

LRCLIB's documented stable API has no multi-track batch endpoint. The job uses
one sequential `/api/search` request per uncached metadata signature, with an
application User-Agent, a timeout, a response-size bound and a 1.1 second
pause between requests. It accepts exact normalized title/artist matches with
duration differences of at most two seconds; among those it prefers valid LRC,
then the matching album and closer duration. Only the best useful result is
saved. Requests and provider search results are never sent from the browser.

Successful and empty searches are cached durably, shared across equal metadata
signatures. A completed content binding is skipped on later runs. Changing
title, main artist, album or rounded duration makes the content eligible again.
Network failures, malformed responses and HTTP errors remain retryable with
exponential backoff. A durable service cooldown stops the run on such errors;
`Retry-After` is honored with jitter. A transaction advisory lock prevents
overlapping enrichment runs. Cancelled runs release the lock, and completed
lookup results survive interruption before attachment to content.

Manual runs bypass saved global cooldown and individual retry delays, while
still skipping completed searches. A fresh API failure stops the run and saves
backoff for subsequent automatic runs; request throttling remains in effect.

## Manual editing and playback

The track editor in Admin → Library supports adding, editing and removing
versions, plain text, LRC, language labels and instrumental markers. Changes
are local; nothing is published to LRCLIB. An edited provider version becomes
manual and is protected from background overwrite. Untouched provider versions
retain provenance. “Retry LRCLIB lookup” clears the lookup cache for the
bound signature; the next enabled run retries without deleting manual texts.

The lyrics button in desktop/mobile playback opens a window-filling view that
follows track changes. LRC lines highlight and scroll with playback and seeking;
plain lyrics remain readable as text. Clicking a timed line seeks only when
viewing the currently playing track. Track info links to an independent,
smaller preview, and track lists show a lyrics icon beside popularity.
Missing lyrics leave the view open for the next track. Public share links use
the same stored texts, restricted to visible tracks in that share.

## APIs and compatibility

- Authenticated web: `GET /api/player/lyrics/{track_id}` returns
  `{ "content_id": "b3:…" | null, "versions": […] }`.
- Public share: `GET /api/player/shares/{token}/lyrics/{track_id}` has the same
  membership and visibility checks as shared audio.
- Federated web playback: `GET /api/player/federation/tracks/lyrics` accepts
  `owner` and `content_id`, fetching from that peer's optional lyrics protocol.
- Frid: `music_dht::lyrics` defines ALPN `furumi/lyrics/1`, capability `lyrics: 1`,
  DTOs, validation and bounded JSON read/write helpers. Each direction carries
  one JSON document terminated by EOF. Requests include protocol version and
  canonical audio content ID; responses echo both and return ordered versions.
  An empty list means unavailable. Clients must check the echoed identity.

Lyrics never change audio, catalog, device-sync or DHT wire layouts. Servers
advertise the capability explicitly; importing the Frid library does not imply
serving lyrics. Old clients ignore it. New clients treat unsupported ALPNs as
unavailable, keeping ordinary playback working. TUI integration is a separate
step; it needs no LRCLIB client or enrichment task.

Provider reference: <https://lrclib.net/docs.txt>.

## Validation

- `node --test tests/*.test.cjs`
- `cargo test lyrics`
- Against disposable PostgreSQL:
  `FURU_LYRICS_TEST_DATABASE_URL=… cargo test postgres_cache_retry_content_binding_and_manual_versions -- --include-ignored`
- Browser fixtures: set `FURU_PODCAST_RENDER_FIXTURE=1`, run
  `cargo test renders_podcast_navigation_and_views`, then
  `node tests/browser-lyrics.cjs` with `PLAYWRIGHT_MODULE`/`CHROME_PATH` if needed.
