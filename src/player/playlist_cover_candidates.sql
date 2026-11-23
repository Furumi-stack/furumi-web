WITH members AS (
    SELECT r.id AS release_id, r.title::text AS release_title, r.cover_file_id AS release_art,
           t.title::text AS track_title, t.cover_file_id AS track_art, pt.position, pt.id AS item_id
      FROM furumusic__playlist_track pt
      JOIN furumusic__track t ON t.id = pt.track_id AND NOT t.is_hidden
      JOIN furumusic__release r ON r.id = t.release_id AND NOT r.is_hidden
     WHERE pt.playlist_id = p.id AND COALESCE(r.cover_file_id, t.cover_file_id) IS NOT NULL
), album_tiles AS (
    SELECT DISTINCT ON (release_id) COALESCE(release_art, track_art) AS art,
           release_title AS title, position, item_id, true AS automatic
      FROM members ORDER BY release_id, position, item_id
), tiles AS (
    SELECT * FROM album_tiles
    UNION ALL
    SELECT track_art, track_title, position, item_id, false
      FROM members WHERE track_art IS NOT NULL AND track_art IS DISTINCT FROM release_art
)
SELECT DISTINCT ON (art) art, title, position, item_id, automatic
  FROM tiles ORDER BY art, automatic DESC, position, item_id
