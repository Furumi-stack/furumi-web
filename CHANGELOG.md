# Changelog

## Unreleased

- Content-bound lyrics with alternative plain/LRC versions, an optional Frid
  protocol and HTTP APIs that leave existing audio/catalog clients compatible.
- Daily LRCLIB enrichment with durable positive/negative caching, metadata
  invalidation, throttling and retry cooldowns; enabled by default and separately
  configurable from stored lyrics support.
- Desktop/mobile lyrics view following playback, individual track previews,
  availability icons, and local version editing in the admin track editor.

## v0.14.0 — 2026-11-23

- Artist pages count guest appearances as tracks and include them in the Listen
  queue. A guest appearance leaves the artist's release count unchanged.
- The player can open an audio similarity search for the current library track.
  Hover over the cover to enlarge it, or click to pin it. On touchscreens, tap
  the cover in the expanded player.

## v0.13.2 — 2026-11-23

- Search brings local and federated artists, releases and tracks into the same
  ranked lists. A small green corner marks peer-only results as they arrive.
  A matching local copy supplies the artwork and links for a single result.
- Artist and release links work from search, track info and the queue.
  Opening a federated release link directly or after a refresh loads the
  matching local edition or retrieves the release from its peers.

## v0.10.6 — 2026-09-02

- Recover federation automatically after sleep, prolonged idle, or a degraded rendezvous transport.
