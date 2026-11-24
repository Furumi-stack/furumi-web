CREATE TABLE IF NOT EXISTS furumusic__dismissed_review (
    input_path text NOT NULL,
    sha256 text NOT NULL,
    review_id bigint NOT NULL,
    imported boolean NOT NULL,
    PRIMARY KEY (input_path, sha256)
);
CREATE INDEX idx_pending_review_input_path ON furumusic__pending_review(input_path, id DESC);
-- Transfer history follows the latest decision for each file, including dismissed reviews.
CREATE VIEW furumusic__upload_review_state AS
SELECT DISTINCT ON (input_path) input_path, status, error_message
FROM (
    SELECT id, input_path, status::text, error_message FROM furumusic__pending_review
    UNION ALL
    SELECT review_id, input_path, CASE WHEN imported THEN 'approved' ELSE 'cancelled' END, NULL::text
    FROM furumusic__dismissed_review
) AS decisions
ORDER BY input_path, id DESC;
