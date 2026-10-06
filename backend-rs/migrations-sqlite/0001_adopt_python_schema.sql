-- The SQLite schema, for a `--features sqlite` build.
--
-- Not transcribed from the Postgres migration and not written by hand:
-- this is what the Python tier's own models produced on SQLite —
-- `Base.metadata.create_all()` + `sync_schema()` + `sync_indexes()` run
-- from the commit before that tier was deleted, then read back out of
-- `sqlite_master`. 21 tables and 62 indexes, the same counts as
-- migrations/0001_adopt_production_schema.sql, which is the same models
-- seen through `pg_dump`.
--
-- Every statement is IF NOT EXISTS for the reason the Postgres one is
-- guarded: a database the Python already created must be ADOPTED, not
-- collided with. A `sentinel.db` from a pre-rewrite self-hosted install
-- opens as it is.
--
-- `kind VARCHAR(20) DEFAULT '''mcp'''` is not a typo here. The model's
-- server_default was the three-character-quoted string, so the default
-- value is `'mcp'` WITH its quotes — in both schemas. Every insert names
-- the column, so the default is never used; it is kept because changing
-- it would make this differ from what an existing database holds.

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    timestamp DATETIME,
    event VARCHAR(50) NOT NULL,
    ip_address VARCHAR(45),
    username VARCHAR(80),
    user_id VARCHAR(100),
    details TEXT,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS camera_groups (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    name VARCHAR(100) NOT NULL,
    color VARCHAR(7),
    icon VARCHAR(10),
    created_at DATETIME,
    updated_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS camera_nodes (
    id INTEGER NOT NULL,
    node_id VARCHAR(100) NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    api_key_hash VARCHAR(128) NOT NULL,
    name VARCHAR(100) NOT NULL,
    hostname VARCHAR(100),
    local_ip VARCHAR(45),
    http_port INTEGER,
    status VARCHAR(20),
    last_seen DATETIME,
    key_rotated_at DATETIME,
    created_at DATETIME,
    video_codec VARCHAR(50),
    audio_codec VARCHAR(50),
    codec_detected_at DATETIME,
    last_register_error VARCHAR(500),
    last_register_error_at DATETIME,
    node_version VARCHAR(50),
    version_checked_at DATETIME,
    storage_used_bytes BIGINT,
    storage_max_bytes BIGINT,
    storage_disk_free_bytes BIGINT,
    storage_disk_total_bytes BIGINT,
    storage_reported_at DATETIME,
    updated_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS cameras (
    id INTEGER NOT NULL,
    camera_id VARCHAR(100) NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    node_id INTEGER,
    name VARCHAR(100) NOT NULL,
    node_type VARCHAR(20),
    capabilities VARCHAR(500),
    group_id INTEGER,
    last_seen DATETIME,
    status VARCHAR(20),
    last_error VARCHAR(500),
    created_at DATETIME,
    updated_at DATETIME,
    video_codec VARCHAR(50),
    audio_codec VARCHAR(50),
    codec_detected_at DATETIME,
    disabled_by_plan BOOLEAN DEFAULT false NOT NULL,
    continuous_24_7 BOOLEAN DEFAULT false NOT NULL,
    scheduled_recording BOOLEAN DEFAULT false NOT NULL,
    scheduled_start VARCHAR(5),
    scheduled_end VARCHAR(5),
    PRIMARY KEY (id),
    FOREIGN KEY(node_id) REFERENCES camera_nodes (id),
    FOREIGN KEY(group_id) REFERENCES camera_groups (id)
);

CREATE TABLE IF NOT EXISTS email_log (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    timestamp DATETIME,
    recipient_email VARCHAR(320) NOT NULL,
    kind VARCHAR(40) NOT NULL,
    status VARCHAR(20) NOT NULL,
    resend_message_id VARCHAR(100),
    error TEXT,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS email_outbox (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    recipient_email VARCHAR(320) NOT NULL,
    subject VARCHAR(500) NOT NULL,
    body_text TEXT NOT NULL,
    body_html TEXT NOT NULL,
    kind VARCHAR(40) NOT NULL,
    notification_id INTEGER,
    status VARCHAR(20) NOT NULL,
    attempts INTEGER NOT NULL,
    last_attempt_at DATETIME,
    sent_at DATETIME,
    resend_message_id VARCHAR(100),
    error TEXT,
    created_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS email_suppression (
    id INTEGER NOT NULL,
    address VARCHAR(320) NOT NULL,
    reason VARCHAR(40) NOT NULL,
    source VARCHAR(40) NOT NULL,
    created_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS incident_evidence (
    id INTEGER NOT NULL,
    incident_id INTEGER NOT NULL,
    kind VARCHAR(20) NOT NULL,
    text TEXT,
    camera_id VARCHAR(100),
    data BLOB,
    data_mime VARCHAR(50),
    timestamp DATETIME,
    PRIMARY KEY (id),
    FOREIGN KEY(incident_id) REFERENCES incidents (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS incidents (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    camera_id VARCHAR(100),
    title VARCHAR(200) NOT NULL,
    summary TEXT NOT NULL,
    report TEXT,
    severity VARCHAR(20) NOT NULL,
    status VARCHAR(20) NOT NULL,
    created_by VARCHAR(150) NOT NULL,
    created_at DATETIME,
    updated_at DATETIME,
    resolved_at DATETIME,
    resolved_by VARCHAR(150),
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS mcp_activity_logs (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    tool_name VARCHAR(100) NOT NULL,
    key_name VARCHAR(100) NOT NULL,
    status VARCHAR(20) NOT NULL,
    duration_ms INTEGER,
    args_summary VARCHAR(500),
    error VARCHAR(500),
    timestamp DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS mcp_api_keys (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    key_hash VARCHAR(128) NOT NULL,
    name VARCHAR(100) NOT NULL,
    created_at DATETIME,
    last_used_at DATETIME,
    revoked BOOLEAN,
    scope_mode VARCHAR(20),
    scope_tools TEXT,
    kind VARCHAR(20) DEFAULT '''mcp''' NOT NULL,
    PRIMARY KEY (id),
    UNIQUE (key_hash)
);

CREATE TABLE IF NOT EXISTS motion_events (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    camera_id VARCHAR(100) NOT NULL,
    node_id VARCHAR(100) NOT NULL,
    score INTEGER NOT NULL,
    segment_seq INTEGER,
    timestamp DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS notifications (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    kind VARCHAR(40) NOT NULL,
    audience VARCHAR(20) NOT NULL,
    title VARCHAR(200) NOT NULL,
    body TEXT NOT NULL,
    severity VARCHAR(20) NOT NULL,
    link VARCHAR(500),
    camera_id VARCHAR(100),
    node_id VARCHAR(100),
    meta_json TEXT,
    created_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS org_monthly_usage (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    year_month VARCHAR(7) NOT NULL,
    viewer_seconds INTEGER NOT NULL,
    updated_at DATETIME,
    PRIMARY KEY (id),
    CONSTRAINT uq_org_monthly_usage UNIQUE (org_id, year_month)
);

CREATE TABLE IF NOT EXISTS processed_webhooks (
    id INTEGER NOT NULL,
    svix_msg_id VARCHAR(255) NOT NULL,
    event_type VARCHAR(100),
    processed_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS sentinel_agent_keys (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    key_hash VARCHAR(128) NOT NULL,
    key_last4 VARCHAR(8),
    name VARCHAR(100) NOT NULL,
    created_at DATETIME,
    created_by VARCHAR(100),
    last_used_at DATETIME,
    revoked BOOLEAN DEFAULT false NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS sentinel_config (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    enabled BOOLEAN NOT NULL,
    motion_enabled BOOLEAN NOT NULL,
    incident_opened_enabled BOOLEAN NOT NULL,
    motion_cooldown_min INTEGER NOT NULL,
    schedule_mode VARCHAR(20) NOT NULL,
    schedule_start VARCHAR(5) NOT NULL,
    schedule_end VARCHAR(5) NOT NULL,
    active_days TEXT,
    camera_scope TEXT,
    created_at DATETIME,
    updated_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS sentinel_runs (
    id VARCHAR(32) NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    triggered_at DATETIME NOT NULL,
    trigger_type VARCHAR(40) NOT NULL,
    camera_id VARCHAR(100),
    tool_call_count INTEGER NOT NULL,
    outcome VARCHAR(20) NOT NULL,
    severity VARCHAR(20),
    incident_id INTEGER,
    started_at DATETIME,
    completed_at DATETIME,
    manual_prompt TEXT,
    summary TEXT,
    tool_trace TEXT,
    updated_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS settings (
    id INTEGER NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    "key" VARCHAR(100) NOT NULL,
    value TEXT,
    updated_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS stream_access_logs (
    id INTEGER NOT NULL,
    user_id VARCHAR(100) NOT NULL,
    user_email VARCHAR(255),
    org_id VARCHAR(100) NOT NULL,
    camera_id VARCHAR(100) NOT NULL,
    node_id VARCHAR(100) NOT NULL,
    ip_address VARCHAR(45),
    user_agent VARCHAR(500),
    accessed_at DATETIME,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS user_notification_state (
    id INTEGER NOT NULL,
    clerk_user_id VARCHAR(100) NOT NULL,
    org_id VARCHAR(100) NOT NULL,
    last_viewed_at DATETIME,
    cleared_at DATETIME,
    PRIMARY KEY (id),
    CONSTRAINT uq_user_notif_state_user_org UNIQUE (clerk_user_id, org_id)
);

CREATE INDEX IF NOT EXISTS ix_audit_log_event ON audit_log (event);

CREATE INDEX IF NOT EXISTS ix_audit_log_org_id ON audit_log (org_id);

CREATE INDEX IF NOT EXISTS ix_audit_log_timestamp ON audit_log (timestamp);

CREATE INDEX IF NOT EXISTS ix_camera_groups_org_id ON camera_groups (org_id);

CREATE INDEX IF NOT EXISTS ix_camera_nodes_api_key_hash ON camera_nodes (api_key_hash);

CREATE UNIQUE INDEX IF NOT EXISTS ix_camera_nodes_node_id ON camera_nodes (node_id);

CREATE INDEX IF NOT EXISTS ix_camera_nodes_org_id ON camera_nodes (org_id);

CREATE UNIQUE INDEX IF NOT EXISTS ix_cameras_camera_id ON cameras (camera_id);

CREATE INDEX IF NOT EXISTS ix_cameras_id ON cameras (id);

CREATE INDEX IF NOT EXISTS ix_cameras_org_id ON cameras (org_id);

CREATE INDEX IF NOT EXISTS ix_email_log_kind ON email_log (kind);

CREATE INDEX IF NOT EXISTS ix_email_log_org_id ON email_log (org_id);

CREATE INDEX IF NOT EXISTS ix_email_log_org_timestamp ON email_log (org_id, timestamp);

CREATE INDEX IF NOT EXISTS ix_email_log_timestamp ON email_log (timestamp);

CREATE INDEX IF NOT EXISTS ix_email_outbox_created_at ON email_outbox (created_at);

CREATE INDEX IF NOT EXISTS ix_email_outbox_org_id ON email_outbox (org_id);

CREATE INDEX IF NOT EXISTS ix_email_outbox_status ON email_outbox (status);

CREATE INDEX IF NOT EXISTS ix_email_outbox_status_created ON email_outbox (status, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS ix_email_suppression_address ON email_suppression (address);

CREATE INDEX IF NOT EXISTS ix_incident_evidence_incident_id ON incident_evidence (incident_id);

CREATE INDEX IF NOT EXISTS ix_incidents_camera_id ON incidents (camera_id);

CREATE INDEX IF NOT EXISTS ix_incidents_created_at ON incidents (created_at);

CREATE INDEX IF NOT EXISTS ix_incidents_org_id ON incidents (org_id);

CREATE INDEX IF NOT EXISTS ix_incidents_severity ON incidents (severity);

CREATE INDEX IF NOT EXISTS ix_incidents_status ON incidents (status);

CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_org_id ON mcp_activity_logs (org_id);

CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_org_timestamp ON mcp_activity_logs (org_id, timestamp);

CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_timestamp ON mcp_activity_logs (timestamp);

CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_tool_name ON mcp_activity_logs (tool_name);

CREATE INDEX IF NOT EXISTS ix_mcp_api_keys_org_id ON mcp_api_keys (org_id);

CREATE INDEX IF NOT EXISTS ix_motion_events_camera_id ON motion_events (camera_id);

CREATE INDEX IF NOT EXISTS ix_motion_events_node_id ON motion_events (node_id);

CREATE INDEX IF NOT EXISTS ix_motion_events_org_id ON motion_events (org_id);

CREATE INDEX IF NOT EXISTS ix_motion_events_org_timestamp ON motion_events (org_id, timestamp);

CREATE INDEX IF NOT EXISTS ix_motion_events_timestamp ON motion_events (timestamp);

CREATE INDEX IF NOT EXISTS ix_notifications_camera_id ON notifications (camera_id);

CREATE INDEX IF NOT EXISTS ix_notifications_created_at ON notifications (created_at);

CREATE INDEX IF NOT EXISTS ix_notifications_kind ON notifications (kind);

CREATE INDEX IF NOT EXISTS ix_notifications_node_id ON notifications (node_id);

CREATE INDEX IF NOT EXISTS ix_notifications_org_created ON notifications (org_id, created_at);

CREATE INDEX IF NOT EXISTS ix_notifications_org_id ON notifications (org_id);

CREATE INDEX IF NOT EXISTS ix_notifications_severity ON notifications (severity);

CREATE INDEX IF NOT EXISTS ix_org_monthly_usage_org_id ON org_monthly_usage (org_id);

CREATE INDEX IF NOT EXISTS ix_processed_webhooks_processed_at ON processed_webhooks (processed_at);

CREATE UNIQUE INDEX IF NOT EXISTS ix_processed_webhooks_svix_msg_id ON processed_webhooks (svix_msg_id);

CREATE UNIQUE INDEX IF NOT EXISTS ix_sentinel_agent_keys_key_hash ON sentinel_agent_keys (key_hash);

CREATE INDEX IF NOT EXISTS ix_sentinel_agent_keys_org_id ON sentinel_agent_keys (org_id);

CREATE UNIQUE INDEX IF NOT EXISTS ix_sentinel_config_org_id ON sentinel_config (org_id);

CREATE INDEX IF NOT EXISTS ix_sentinel_runs_camera_id ON sentinel_runs (camera_id);

CREATE INDEX IF NOT EXISTS ix_sentinel_runs_org_id ON sentinel_runs (org_id);

CREATE INDEX IF NOT EXISTS ix_sentinel_runs_org_triggered ON sentinel_runs (org_id, triggered_at);

CREATE INDEX IF NOT EXISTS ix_sentinel_runs_triggered_at ON sentinel_runs (triggered_at);

CREATE INDEX IF NOT EXISTS ix_settings_key ON settings ("key");

CREATE INDEX IF NOT EXISTS ix_settings_org_id ON settings (org_id);

CREATE INDEX IF NOT EXISTS ix_settings_org_key ON settings (org_id, "key");

CREATE INDEX IF NOT EXISTS ix_stream_access_logs_accessed_at ON stream_access_logs (accessed_at);

CREATE INDEX IF NOT EXISTS ix_stream_access_logs_camera_id ON stream_access_logs (camera_id);

CREATE INDEX IF NOT EXISTS ix_stream_access_logs_org_accessed ON stream_access_logs (org_id, accessed_at);

CREATE INDEX IF NOT EXISTS ix_stream_access_logs_org_id ON stream_access_logs (org_id);

CREATE INDEX IF NOT EXISTS ix_stream_access_logs_user_id ON stream_access_logs (user_id);

CREATE INDEX IF NOT EXISTS ix_user_notification_state_clerk_user_id ON user_notification_state (clerk_user_id);

CREATE INDEX IF NOT EXISTS ix_user_notification_state_org_id ON user_notification_state (org_id);
