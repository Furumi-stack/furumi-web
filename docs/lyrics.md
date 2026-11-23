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

Admin Settings → **Lyrics** has separate controls for automatic enrichment and
local search indexing.
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
Malformed search records are skipped individually. If an incomplete response
yields no usable match, or the request fails for an individual track, that
signature is deferred for retry with exponential backoff and the run advances
to the next track. Connection failures and HTTP 429/503 persist a service-wide
cooldown and pause the run. Numeric `Retry-After` delays receive jitter.
A transaction advisory lock prevents overlapping runs. Cancelled runs release
the lock, and completed lookup results survive interruption before attachment.

Manual runs bypass saved global cooldown and individual retry delays, while
still skipping completed searches. A deferred track is eligible again on the
next manual run. Request throttling remains in effect.

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

## Local lyrics search (0.13.1)

The icon on the left of the search field switches between ordinary search and
lyrics search. Lyrics mode uses the same artist/release cards, track rows,
version groups and playback actions. It returns tracks found through their
lyrics, plus those tracks' artists and releases, in order of their best matching
track. It does not display lyrics excerpts or change the lyrics viewer.
Enter searches immediately; otherwise lyrics search waits 1.8 seconds after the
last input (ordinary search retains its 300 ms delay). Changing text, navigating
or switching modes cancels pending requests and discards stale responses. Lyrics
mode is local only; it never opens a federation search stream.

`FURU_LYRICS_INDEXING_ENABLED` defaults to `true`, with a live toggle in the
Lyrics settings block. A single background worker checks for new or edited
versions approximately every 30 seconds and continuously drains the backlog in
batches of four. Disabling it pauses new calculations and retains stored
embeddings; lexical and semantic retrieval remain usable. This setting is
independent of LRCLIB enrichment. Admin shows the profile, phase, unique texts,
completed/total chunks, vector bytes and the latest error.

All saved versions, including translations and LRC-only versions, participate.
Instrumentals and hidden tracks/releases are excluded. Text is split into
overlapping windows of at most 240 Unicode characters, overlapping by roughly
60 characters and adjusted at word boundaries. Timestamp tags are removed;
NFKC, lowercase, `ё`/`е` folding and alphanumeric tokens define shared chunk
identities. Identical normalized chunks reuse a vector, across versions and
tracks. Vectors are persisted as 384 little-endian float32 values under a
versioned **lyrics** profile, separately from audio vectors. Profile changes
create a fresh derived index. Cached vectors survive restarts and source edits.
Source version timestamps and visibility are rechecked before returning tracks,
so removed or edited text cannot be returned from an older memory snapshot.
Rolling replicas serialize indexing batches with a PostgreSQL advisory lock.

Inference uses Rust `fastembed`/ONNX Runtime on CPU (two inference threads).
The official `intfloat/multilingual-e5-small` FP32 ONNX model and tokenizer are
pinned to revision `614241f622f53c4eeff9890bdc4f31cfecc418b3`, downloaded and
checksum-verified **during Docker build**, and copied to
`/opt/furumusic/models/multilingual-e5-small`. Native ONNX Runtime is linked into
the executable. There are no runtime model downloads or external inference
services. The model adds about 487 MB of uncompressed artifacts to the image.
For development:

```sh
python scripts/fetch-lyrics-model.py target/lyrics-model
export FURU_LYRICS_MODEL_DIR="$PWD/target/lyrics-model"
```

The encoder uses attention-mask mean pooling, L2 normalization and E5's
`query:` / `passage:` prefixes. Ranking combines inverse-frequency weighted
term coverage, longest contiguous phrase and term proximity with calibrated
cosine similarity. Exact evidence dominates, with a bonus for agreement between
lexical and semantic scores. Only the best fragment/version contributes to a
track's rank; repetition never accumulates score. Retrieval scans the cached
local vectors, like audio similarity, without a separate vector database.
Text matching stays available while embeddings are pending or the model is
unavailable. Scores are retrieval signals, not confidence percentages.

Authenticated `GET /api/player/search?q=…&mode=lyrics&limit=30` returns the normal
search arrays (`artists`, `releases`, `tracks`, empty `podcasts`) plus optional
`lyrics_status` and `semantic_error` metadata. Queries are limited to 500
characters and results to 50 tracks. Omitting `mode` preserves ordinary search.

## Validation

- `node --test tests/*.test.cjs`
- `cargo test lyrics`
- Real E5 + disposable PostgreSQL: set `FURU_LYRICS_MODEL_DIR` and
  `FURU_LYRICS_TEST_DATABASE_URL`, then
  `cargo test lyrics::search -- --include-ignored`.
- `node tests/browser-lyrics-search.cjs` checks standard results, mobile/desktop
  layout, mode styling, Enter/debounce, local-only requests and reduced motion.
- Against disposable PostgreSQL:
  `FURU_LYRICS_TEST_DATABASE_URL=… cargo test postgres_cache_retry_content_binding_and_manual_versions -- --include-ignored`
- Browser fixtures: set `FURU_PODCAST_RENDER_FIXTURE=1`, run
  `cargo test renders_podcast_navigation_and_views`, then
  `node tests/browser-lyrics.cjs` with `PLAYWRIGHT_MODULE`/`CHROME_PATH` if needed.
