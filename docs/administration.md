# Using and administering Furumusic

[Project overview](../README.md) · [Installation](setup.md) · [Development and tests](development.md)

## Podcasts

The player's **Podcasts** entry below **My uploads** manages RSS/Atom subscriptions.
Subscriptions start private. Publishing shares a feed card with other instance users
and, while federation is enabled, the Frid podcast discovery overlay. Network search
uses the same overlay as Furumi TUI; subscribing fetches the origin feed itself rather
than trusting a publisher's claimed redirect or episode metadata. Feed URLs that may
contain private access tokens require an additional confirmation before publication.

The catalogue and downloaded audio are shared by the instance. Unsubscribing removes
only that user's subscription and publication; other subscribers keep their access.
Listening position and played status are personal. Playback can begin from the source
while a separate download saves the episode on the server. Cached episodes play from
the server even when the publisher is unavailable; a connection to this server is
still required.

The internal **podcast_refresh** job checks subscribed feeds every 30 minutes and
downloads missing episodes, retrying failed downloads on later runs. Its schedule can
be changed in the normal job settings. A manual check makes new episode metadata
available before the audio download finishes. Up to three downloads run concurrently;
each episode is limited to 2 GiB. Podcast sources must resolve to public HTTP(S)
addresses, including after redirects.

**Settings → Podcasts → Manage podcasts** lists every known podcast, subscriber and
publication counts, and saved episode counts. Administrators can check a feed, remove
its publications, clear saved audio, or delete the podcast for every user. Audio
retention is disabled by default. When enabled, its day count is measured from the
download time; the periodic job removes old audio while keeping episode descriptions
and listening progress. Removed audio is downloaded again only on an explicit save
or playback request. Podcast personal state is not replicated to trusted Frid devices
yet; device synchronization continues to advertise protocol v3.

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

## Last.fm popularity updates

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
