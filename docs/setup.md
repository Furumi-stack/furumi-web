# Installation and configuration

[Project overview](../README.md) · [Administration guide](administration.md)

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
