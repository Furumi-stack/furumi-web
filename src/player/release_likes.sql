-- One candidate per recording. An unindexed track keeps its catalog identity.
WITH members AS (
    SELECT DISTINCT ON (a.content_id, CASE WHEN a.content_id IS NULL THEN t.id END)
           t.id AS track_id, a.content_id
      FROM furumusic__track t
      LEFT JOIN furumusic__track_content_alias a ON a.track_id = t.id
     WHERE t.release_id = $2 AND NOT t.is_hidden
     ORDER BY a.content_id, CASE WHEN a.content_id IS NULL THEN t.id END, t.id
), copies AS (
    SELECT m.track_id, m.content_id, ARRAY(
        SELECT m.track_id
        UNION ALL
        SELECT a.track_id FROM furumusic__track_content_alias a
         WHERE a.content_id = m.content_id AND a.track_id <> m.track_id
    ) AS aliases FROM members m
)
SELECT c.track_id, ARRAY(
           SELECT l.track_id FROM furumusic__user_liked_track l
            WHERE l.user_id = $1 AND l.track_id = ANY(c.aliases)
       ) AS liked_ids,
       EXISTS (
           SELECT 1 FROM furumusic__fed_state_like f
            WHERE f.user_id = $1 AND f.content_id = c.content_id
              AND f.liked AND f.local_track_id IS NULL
       ) AS liked_remotely
  FROM copies c
