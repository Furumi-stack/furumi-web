# Furumusic

**Your library. Your users. Your network.**

Furumusic is a self-hosted music server with a full web player. Bring your music,
share a library with friends, and keep your own playlists, likes, and listening
history. Listen from a desktop or phone, or connect independent servers through
the Furumi federated network.

## What you can do

- **Listen anywhere.** Browse artists and releases, search your collection, build
  queues, and manage playlists in a responsive web player.
- **Bring your collection.** Upload files and folders, or import music from
  YouTube, torrents, and magnet links directly into your library or a playlist.
- **Organize your music.** Optional AI assistance recognizes metadata, finds
  artwork, and helps repair messy releases. Review proposed changes before
  applying them.
- **Share a song.** Send a track or queue link that anyone can play and download
  without creating an account.
- **Welcome your people.** Use password login or SSO. Instance admins can approve
  new users without needing access to the identity provider.
- **Discover more.** Find similar tracks and connect Last.fm for scrobbling.
- **Connect your players.** Pair trusted Furumi devices to synchronize playlists,
  likes, playback state, and playback controls.
- **Join a network.** Discover and stream music from other participating peers
  while keeping your own library independent.
- **Manage from any screen.** Review imports, manage users and background tasks,
  and explore listening statistics in a desktop and mobile admin console.

AI assistance, federation, similarity search, Last.fm, and SSO are optional.
You choose which features to enable.

## Quick start

With Rust and PostgreSQL installed, create an empty database and start the server:

```bash
export FURU_DATABASE_URL='postgresql://furumusic:password@127.0.0.1/furumusic'
cargo run --release --locked
```

Open <http://127.0.0.1:8000/admin/setup> to create your administrator account,
then configure the inbox and music library directories in **Admin → Settings**.

## Documentation

- [Installation and configuration](docs/setup.md)
- [Using and administering Furumusic](docs/administration.md)
- [Development, architecture, and tests](docs/development.md)
- [Admin UI conventions](docs/admin-ui.md)

Bug reports, ideas, and contributions are welcome.
