CREATE TABLE furumusic__fed_device_identity (
 user_id BIGINT PRIMARY KEY, device_id TEXT NOT NULL, group_id TEXT NOT NULL,
 device_name TEXT NOT NULL, local_seq BIGINT DEFAULT 0, last_hlc_ms BIGINT DEFAULT 0,
 local_seeded_at_ms BIGINT NOT NULL DEFAULT 1
);
CREATE TABLE furumusic__fed_sync_ops (
 user_id BIGINT, op_id TEXT, origin_device_id TEXT, seq BIGINT, kind TEXT,
 payload_json JSONB, hlc_ms BIGINT, received_at_ms BIGINT, tombstone BOOLEAN,
 PRIMARY KEY(user_id,op_id)
);
CREATE TABLE furumusic__fed_sync_vector (
 user_id BIGINT, device_id TEXT, max_seq BIGINT, PRIMARY KEY(user_id,device_id)
);
INSERT INTO furumusic__fed_device_identity(user_id,device_id,group_id,device_name)
 VALUES(1,'web_test_one','group_test','one'),(2,'web_test_two','group_test','two');
