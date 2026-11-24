WITH selected AS MATERIALIZED (
    SELECT id, input_path,
           lower(substring(context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9A-Fa-f]{64})"')) AS sha256
    FROM furumusic__pending_review
    WHERE id = ANY($1) AND status <> 'processing'
      AND ($2::text IS NULL OR (status IN ('pending','failed') AND context_json ~ $2))
    FOR UPDATE
), eligible AS (
    -- Older unfinished reviews without a hash stay visible until discovery
    -- identifies their bytes. A later upload must not inherit the dismissal.
    SELECT * FROM selected WHERE input_path IS NULL OR input_path = '' OR sha256 IS NOT NULL
), targets AS (
    SELECT id FROM eligible
    UNION
    -- Older scanners could enqueue the very same upload more than once.
    -- Include only unfinished copies belonging to that exact path and hash.
    SELECT r.id FROM furumusic__pending_review r
    JOIN eligible e ON r.input_path=e.input_path
       AND lower(substring(r.context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9A-Fa-f]{64})"'))=e.sha256
    WHERE r.status IN ('queued','pending','failed','rejected') AND ($2 IS NULL OR r.context_json ~ $2)
), removed AS (
    DELETE FROM furumusic__pending_review r USING targets t
    WHERE r.id=t.id AND r.status <> 'processing'
      AND (r.id IN (SELECT id FROM eligible) OR r.status IN ('queued','pending','failed','rejected'))
    RETURNING r.id, r.input_path, r.status,
              lower(substring(r.context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9A-Fa-f]{64})"')) AS sha256
), recorded AS (
    INSERT INTO furumusic__dismissed_review(input_path, sha256, review_id, imported)
    SELECT DISTINCT ON (input_path, sha256) input_path, sha256, id, status IN ('approved','auto_approved')
    FROM removed WHERE input_path IS NOT NULL AND input_path <> '' AND sha256 IS NOT NULL
    ORDER BY input_path, sha256, id DESC
    ON CONFLICT (input_path, sha256) DO UPDATE
        SET review_id = EXCLUDED.review_id, imported = EXCLUDED.imported
        WHERE EXCLUDED.review_id > furumusic__dismissed_review.review_id
)
SELECT count(*) FROM removed WHERE id = ANY($1)
