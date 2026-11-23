CREATE TABLE furumusic__sidebar_pin (
    user_id bigint NOT NULL REFERENCES furumusic__user(id) ON DELETE CASCADE,
    href text NOT NULL,
    artist_id bigint REFERENCES furumusic__artist(id) ON DELETE CASCADE,
    release_id bigint REFERENCES furumusic__release(id) ON DELETE CASCADE,
    playlist_id bigint REFERENCES furumusic__playlist(id) ON DELETE CASCADE,
    peer_title text,
    peer_cover text,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, href),
    CHECK (num_nonnulls(artist_id, release_id, playlist_id, peer_title) = 1)
)
