# Changelog

## Unreleased

- Content-bound lyrics with alternative plain/LRC versions, an optional Frid
  protocol and HTTP APIs that leave existing audio/catalog clients compatible.
- Daily LRCLIB enrichment with durable positive/negative caching, metadata
  invalidation, throttling and retry cooldowns; enabled by default and separately
  configurable from stored lyrics support.
- Desktop/mobile lyrics view following playback, individual track previews,
  availability icons, and local version editing in the admin track editor.

## v0.16.3 — 2026-11-24

- Furumi downloads the lyrics search model when indexing is enabled and reuses
  it from the audio models directory. This removes about 487 MB of uncompressed
  model files from the Docker image.
- Shared track details omit the uploader's name.
- The account menu opens and closes with a short animation on desktop and mobile
  and respects the device's reduced motion preference.

## v0.16.2 — 2026-11-24

- Saved tracks and their likes load when the browser loses its connection.
  Federation streams close while offline and reconnect when the server returns.
- Only the device already playing a saved track automatically continues offline.
  Remote controller tabs stay paused. The account menu explains offline mode,
  and a soft yellow label beside search shows when it is active.
- Collapsing popular tracks on an artist page returns the list to the position
  where it was expanded.
- The desktop sidebar now has a `#furumi` wordmark with an embedded monospace
  font and a subtle click animation that respects reduced motion. The build
  version beside it links to the project repository.

## v0.16.1 — 2026-11-24

- Artist pages show five distinct popular titles at first. Matching titles are
  grouped regardless of capitalization, with each recording available from its
  branch button. A slim control below the list quickly expands or collapses
  the top 50 and respects reduced-motion settings.
- The bottom player uses a shorter, responsive grid for playback, seeking and
  track actions on desktop and mobile.
- Saved music in account settings opens a searchable, sortable, paginated cache
  manager. It shows download and last-played dates, play counts, audio details
  and sizes. Cleanup by date previews the number of recordings and the space
  that removing them would free, including shared audio and artwork.
- The account menu shows cache usage against the configured limit and stays
  open when interacting with its controls. Unpin notifications can be dismissed early.
- Playlists and Liked songs have an in-list search by track title or artist.
  Matching rows animate into place and retain their original playback and edit positions.

## v0.16.0 — 2026-11-24

- Added opt-in offline listening to the web player. Starting a music track saves
  the recording on this device with its catalog entries, available artwork and lyrics.
  Saved music can be searched, played and browsed through artist and release pages.
- The account menu shows cache usage and switches the player to local playback.
  Server outages activate offline listening automatically until the instance returns.
- Cache management includes individual removal and a logarithmic size limit from
  100 MB to 512 GB, initially 3 GB. Downloads pause when the cache fills.

## v0.15.3 — 2026-11-24

- Listening by day can display a calendar heatmap shaded by daily play count.
  Select a day to open its history, or select the last 365 days for a full year.
  The player remembers the chart style in the browser.
- Fixed inflated listening time from older web clients. Existing affected plays
  are corrected on upgrade. Both daily charts now show play count and listening
  time together, and the selected day's history shows its total play count.
- Moved My listening into the account menu on desktop and mobile.
- Catalog now brings All music, Playlists and My uploads together. Personal
  playlists appear first, followed by available public playlists. Followed
  artists are accessible from the account menu.
- Playlist headers show release artwork as a collage and keep the pin on the
  right. Owners can rename inline, choose up to four images from the playlist,
  upload a cover, or restore the automatic collage. The artwork picker floats
  beside the cover and keeps the track list in place.
- Liked songs now keeps one entry when several catalog rows share the same
  audio. Existing duplicate likes are repaired on upgrade. Different recordings
  with the same title can still be liked separately.
- Track rows keep lyrics, popularity and playback controls aligned. The new
  actions menu includes sharing, playlists, queueing, track details, similarity
  search and downloading where available.

## v0.15.2 — 2026-11-24

- Reorder pinned sidebar shortcuts by dragging or using the arrow keys. The
  order is saved for the account on desktop and mobile.
- Unpin shortcuts directly from the menu. Each removal can be undone for ten
  seconds to restore the shortcut to its previous position.

## v0.15.1 — 2026-11-24

- My listening now finds local artist portraits for Cyrillic and other Unicode
  credits, including older catalog entries with different sort keys.
- A failed listening thumbnail falls back to the original image. A refreshed
  ranking row can display new artwork after an earlier image failed.

## v0.15.0 — 2026-11-23

- Desktop and mobile navigation now includes a personal music collection,
  account shortcuts and pins for artists, releases and playlists.
- My listening shows personal listening time, daily activity, rankings and a
  playable history that can be filtered by day.
- The lyrics preview keeps loading status in the header, offers one-click
  copying and lets listeners switch versions from the source names below the text.
- Double-clicking a track row starts playback without selecting the title.

## v0.14.0 — 2026-11-23

- Artist pages count guest appearances as tracks and include them in the Listen
  queue. A guest appearance leaves the artist's release count unchanged.
- The player can open an audio similarity search for the current library track.
  Hover over the cover to enlarge it, or click to pin it. On touchscreens, tap
  the cover in the expanded player.

## v0.13.3 — 2026-11-23

- Discover lyrics versions from federation peers by exact audio content ID.
- Show local lyrics immediately and append peer alternatives without changing
  the selected version. Save on listen makes peer lyrics available for further
  distribution alongside imported audio.
- Keep outbound lyrics streams alive until the response finishes sending.

## v0.13.2 — 2026-11-23

- Search brings local and federated artists, releases and tracks into the same
  ranked lists. A small green corner marks peer-only results as they arrive.
  A matching local copy supplies the artwork and links for a single result.
- Artist and release links work from search, track info and the queue.
  Opening a federated release link directly or after a refresh loads the
  matching local edition or retrieves the release from its peers.

## v0.10.6 — 2026-09-02

- Recover federation automatically after sleep, prolonged idle, or a degraded rendezvous transport.
