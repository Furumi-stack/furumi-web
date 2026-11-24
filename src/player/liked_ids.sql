WITH stored AS (
    SELECT track_id FROM furumusic__user_liked_track WHERE user_id = $1
), recordings AS (
    SELECT a.content_id FROM stored s JOIN furumusic__track_content_alias a USING (track_id)
    UNION
    SELECT content_id FROM furumusic__fed_state_like
     WHERE user_id = $1 AND liked AND local_track_id IS NULL
)
SELECT track_id FROM stored
UNION
SELECT a.track_id FROM recordings r JOIN furumusic__track_content_alias a USING (content_id)
 WHERE NOT a.hidden
