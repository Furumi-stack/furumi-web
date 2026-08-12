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
- direct file uploads and torrent or magnet imports;
- optional AI-assisted recognition and normalization of metadata;
- password login or OIDC/SSO with group-based access control;
- optional federation without a central catalog or search service;
- trusted-device pairing, playback handoff, and synchronization between Furumi
  players;
- Last.fm scrobbling and similarity-based discovery when configured.

Furumusic does not include music. Import only media you are allowed to store and
share.

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

## Contributing

Bug reports, design discussions, and patches are welcome. Before submitting a
change, run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```
