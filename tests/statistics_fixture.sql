CREATE TEMP TABLE furumusic__user (id bigint PRIMARY KEY, username text, display_name text);
CREATE TEMP TABLE furumusic__fed_device_identity (user_id bigint PRIMARY KEY, device_id text);
CREATE TEMP TABLE furumusic__fed_device (user_id bigint, device_id text, name text, PRIMARY KEY(user_id, device_id));
CREATE TEMP TABLE furumusic__listen_event (
    user_id bigint, listen_id text, content_id text, local_track_id bigint, origin_device_id text,
    started_at_ms bigint, listened_ms bigint, track_duration_ms bigint, ended_reason text,
    qualified boolean, metadata_json jsonb, PRIMARY KEY(user_id, listen_id)
);
CREATE TEMP TABLE furumusic__track (id bigint);
CREATE TEMP TABLE furumusic__release (id bigint);
CREATE TEMP TABLE furumusic__artist (id bigint);
CREATE TEMP TABLE furumusic__playlist (id bigint);
CREATE TEMP TABLE furumusic__media_file (file_size_bytes bigint, audio_format text, file_type text);
CREATE TEMP TABLE furumusic__pending_review (status text);
CREATE TEMP TABLE furumusic__job_run (status text);
CREATE TEMP TABLE furumusic__lastfm_scrobble_outbox (status text);
INSERT INTO furumusic__user VALUES (1, 'alice', 'Alice'), (2, 'bob', 'Bob'), (3, 'silent', NULL);
INSERT INTO furumusic__fed_device_identity VALUES (1, 'web-a'), (2, 'web-b');
INSERT INTO furumusic__fed_device VALUES (1, 'phone', 'Alice phone');
INSERT INTO furumusic__listen_event
SELECT 1, 'listen-' || n, 'content-one', NULL, 'web-a',
       1793491200000 + n * 1000, 180000, 180000, '"finished"', true,
       '{"title":"First song", "artist_names":["Artist A"], "featured_artist_names":["Artist A", "Guest"], "release_title":"Album"}'::jsonb
FROM generate_series(1, 30) n;
INSERT INTO furumusic__listen_event VALUES
(2, 'listen-1', 'content-one', NULL, 'web-b', 1793664000000, 90000, 180000, '"skipped"', true,
 '{"title":"First song", "artist_names":["Artist A"], "release_title":"Album"}'),
(1, 'phone-1', 'content-two', NULL, 'phone', 1793664001000, 120000, 240000, '"replaced"', true,
 '{"title":"Remote song", "artist_names":["Artist B"], "release_title":"Remote album"}'),
(2, 'short-1', 'content-one', NULL, 'web-b', 1793664002000, 8000, 180000, '"skipped"', false,
 '{"title":"First song", "artist_names":["Artist A"]}');
