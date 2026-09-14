-- Command Center schema, taken from the production database rather than
-- transcribed from SQLAlchemy.
--
--   pg_dump --schema-only --no-owner --no-privileges
--
-- 21 tables, 62 indexes. The Python service builds this with
-- create_all() plus a hand-rolled additive sync_schema(); this migration
-- adopts exactly what that produced, so the Rust backend attaches to the
-- live database rather than defining a second, subtly different shape.
--
-- psql meta-commands are stripped. pg_dump 18 wraps its output in
-- \restrict / \unrestrict, which psql understands and Postgres does
-- not — sqlx sends raw SQL to the server, so leaving them in failed the
-- migration with 'syntax error at or near "\\"'.
--
-- Everything is IF NOT EXISTS: running it against the existing database
-- must be a no-op, not a failure and not a rewrite. Constraints have no
-- IF NOT EXISTS form in Postgres, so each is guarded by an explicit
-- pg_constraint lookup. Swallowing by SQLSTATE was the first attempt and
-- was wrong twice over: a duplicate PRIMARY KEY raises
-- invalid_table_definition rather than duplicate_object, and catching
-- broadly would hide real errors. A catalog check hides nothing.

CREATE TABLE IF NOT EXISTS public.audit_log (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    "timestamp" timestamp without time zone,
    event character varying(50) NOT NULL,
    ip_address character varying(45),
    username character varying(80),
    user_id character varying(100),
    details text
);
CREATE SEQUENCE IF NOT EXISTS public.audit_log_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.audit_log_id_seq OWNED BY public.audit_log.id;
CREATE TABLE IF NOT EXISTS public.camera_groups (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    name character varying(100) NOT NULL,
    color character varying(7),
    icon character varying(10),
    created_at timestamp without time zone,
    updated_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.camera_groups_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.camera_groups_id_seq OWNED BY public.camera_groups.id;
CREATE TABLE IF NOT EXISTS public.camera_nodes (
    id integer NOT NULL,
    node_id character varying(100) NOT NULL,
    org_id character varying(100) NOT NULL,
    api_key_hash character varying(128) NOT NULL,
    name character varying(100) NOT NULL,
    hostname character varying(100),
    local_ip character varying(45),
    http_port integer,
    status character varying(20),
    last_seen timestamp without time zone,
    key_rotated_at timestamp without time zone,
    created_at timestamp without time zone,
    video_codec character varying(50),
    audio_codec character varying(50),
    codec_detected_at timestamp without time zone,
    last_register_error character varying(500),
    last_register_error_at timestamp without time zone,
    node_version character varying(50),
    version_checked_at timestamp without time zone,
    storage_used_bytes bigint,
    storage_max_bytes bigint,
    storage_disk_free_bytes bigint,
    storage_disk_total_bytes bigint,
    storage_reported_at timestamp without time zone,
    updated_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.camera_nodes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.camera_nodes_id_seq OWNED BY public.camera_nodes.id;
CREATE TABLE IF NOT EXISTS public.cameras (
    id integer NOT NULL,
    camera_id character varying(100) NOT NULL,
    org_id character varying(100) NOT NULL,
    node_id integer,
    name character varying(100) NOT NULL,
    node_type character varying(20),
    capabilities character varying(500),
    group_id integer,
    last_seen timestamp without time zone,
    status character varying(20),
    last_error character varying(500),
    created_at timestamp without time zone,
    updated_at timestamp without time zone,
    video_codec character varying(50),
    audio_codec character varying(50),
    codec_detected_at timestamp without time zone,
    disabled_by_plan boolean DEFAULT false NOT NULL,
    continuous_24_7 boolean DEFAULT false NOT NULL,
    scheduled_recording boolean DEFAULT false NOT NULL,
    scheduled_start character varying(5),
    scheduled_end character varying(5)
);
CREATE SEQUENCE IF NOT EXISTS public.cameras_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.cameras_id_seq OWNED BY public.cameras.id;
CREATE TABLE IF NOT EXISTS public.email_log (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    "timestamp" timestamp without time zone,
    recipient_email character varying(320) NOT NULL,
    kind character varying(40) NOT NULL,
    status character varying(20) NOT NULL,
    resend_message_id character varying(100),
    error text
);
CREATE SEQUENCE IF NOT EXISTS public.email_log_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.email_log_id_seq OWNED BY public.email_log.id;
CREATE TABLE IF NOT EXISTS public.email_outbox (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    recipient_email character varying(320) NOT NULL,
    subject character varying(500) NOT NULL,
    body_text text NOT NULL,
    body_html text NOT NULL,
    kind character varying(40) NOT NULL,
    notification_id integer,
    status character varying(20) NOT NULL,
    attempts integer NOT NULL,
    last_attempt_at timestamp without time zone,
    sent_at timestamp without time zone,
    resend_message_id character varying(100),
    error text,
    created_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.email_outbox_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.email_outbox_id_seq OWNED BY public.email_outbox.id;
CREATE TABLE IF NOT EXISTS public.email_suppression (
    id integer NOT NULL,
    address character varying(320) NOT NULL,
    reason character varying(40) NOT NULL,
    source character varying(40) NOT NULL,
    created_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.email_suppression_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.email_suppression_id_seq OWNED BY public.email_suppression.id;
CREATE TABLE IF NOT EXISTS public.incident_evidence (
    id integer NOT NULL,
    incident_id integer NOT NULL,
    kind character varying(20) NOT NULL,
    text text,
    camera_id character varying(100),
    data bytea,
    data_mime character varying(50),
    "timestamp" timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.incident_evidence_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.incident_evidence_id_seq OWNED BY public.incident_evidence.id;
CREATE TABLE IF NOT EXISTS public.incidents (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    camera_id character varying(100),
    title character varying(200) NOT NULL,
    summary text NOT NULL,
    report text,
    severity character varying(20) NOT NULL,
    status character varying(20) NOT NULL,
    created_by character varying(150) NOT NULL,
    created_at timestamp without time zone,
    updated_at timestamp without time zone,
    resolved_at timestamp without time zone,
    resolved_by character varying(150)
);
CREATE SEQUENCE IF NOT EXISTS public.incidents_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.incidents_id_seq OWNED BY public.incidents.id;
CREATE TABLE IF NOT EXISTS public.mcp_activity_logs (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    tool_name character varying(100) NOT NULL,
    key_name character varying(100) NOT NULL,
    status character varying(20) NOT NULL,
    duration_ms integer,
    args_summary character varying(500),
    error character varying(500),
    "timestamp" timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.mcp_activity_logs_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.mcp_activity_logs_id_seq OWNED BY public.mcp_activity_logs.id;
CREATE TABLE IF NOT EXISTS public.mcp_api_keys (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    key_hash character varying(128) NOT NULL,
    name character varying(100) NOT NULL,
    created_at timestamp without time zone,
    last_used_at timestamp without time zone,
    revoked boolean,
    scope_mode character varying(20),
    scope_tools text,
    kind character varying(20) DEFAULT '''mcp'''::character varying NOT NULL
);
CREATE SEQUENCE IF NOT EXISTS public.mcp_api_keys_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.mcp_api_keys_id_seq OWNED BY public.mcp_api_keys.id;
CREATE TABLE IF NOT EXISTS public.motion_events (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    camera_id character varying(100) NOT NULL,
    node_id character varying(100) NOT NULL,
    score integer NOT NULL,
    segment_seq integer,
    "timestamp" timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.motion_events_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.motion_events_id_seq OWNED BY public.motion_events.id;
CREATE TABLE IF NOT EXISTS public.notifications (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    kind character varying(40) NOT NULL,
    audience character varying(20) NOT NULL,
    title character varying(200) NOT NULL,
    body text NOT NULL,
    severity character varying(20) NOT NULL,
    link character varying(500),
    camera_id character varying(100),
    node_id character varying(100),
    meta_json text,
    created_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.notifications_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.notifications_id_seq OWNED BY public.notifications.id;
CREATE TABLE IF NOT EXISTS public.org_monthly_usage (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    year_month character varying(7) NOT NULL,
    viewer_seconds integer NOT NULL,
    updated_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.org_monthly_usage_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.org_monthly_usage_id_seq OWNED BY public.org_monthly_usage.id;
CREATE TABLE IF NOT EXISTS public.processed_webhooks (
    id integer NOT NULL,
    svix_msg_id character varying(255) NOT NULL,
    event_type character varying(100),
    processed_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.processed_webhooks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.processed_webhooks_id_seq OWNED BY public.processed_webhooks.id;
CREATE TABLE IF NOT EXISTS public.sentinel_agent_keys (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    key_hash character varying(128) NOT NULL,
    key_last4 character varying(8),
    name character varying(100) NOT NULL,
    created_at timestamp without time zone,
    created_by character varying(100),
    last_used_at timestamp without time zone,
    revoked boolean DEFAULT false NOT NULL
);
CREATE SEQUENCE IF NOT EXISTS public.sentinel_agent_keys_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.sentinel_agent_keys_id_seq OWNED BY public.sentinel_agent_keys.id;
CREATE TABLE IF NOT EXISTS public.sentinel_config (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    enabled boolean NOT NULL,
    motion_enabled boolean NOT NULL,
    incident_opened_enabled boolean NOT NULL,
    motion_cooldown_min integer NOT NULL,
    schedule_mode character varying(20) NOT NULL,
    schedule_start character varying(5) NOT NULL,
    schedule_end character varying(5) NOT NULL,
    active_days text,
    camera_scope text,
    created_at timestamp without time zone,
    updated_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.sentinel_config_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.sentinel_config_id_seq OWNED BY public.sentinel_config.id;
CREATE TABLE IF NOT EXISTS public.sentinel_runs (
    id character varying(32) NOT NULL,
    org_id character varying(100) NOT NULL,
    triggered_at timestamp without time zone NOT NULL,
    trigger_type character varying(40) NOT NULL,
    camera_id character varying(100),
    tool_call_count integer NOT NULL,
    outcome character varying(20) NOT NULL,
    severity character varying(20),
    incident_id integer,
    started_at timestamp without time zone,
    completed_at timestamp without time zone,
    manual_prompt text,
    summary text,
    tool_trace text,
    updated_at timestamp without time zone
);
CREATE TABLE IF NOT EXISTS public.settings (
    id integer NOT NULL,
    org_id character varying(100) NOT NULL,
    key character varying(100) NOT NULL,
    value text,
    updated_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.settings_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.settings_id_seq OWNED BY public.settings.id;
CREATE TABLE IF NOT EXISTS public.stream_access_logs (
    id integer NOT NULL,
    user_id character varying(100) NOT NULL,
    user_email character varying(255),
    org_id character varying(100) NOT NULL,
    camera_id character varying(100) NOT NULL,
    node_id character varying(100) NOT NULL,
    ip_address character varying(45),
    user_agent character varying(500),
    accessed_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.stream_access_logs_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.stream_access_logs_id_seq OWNED BY public.stream_access_logs.id;
CREATE TABLE IF NOT EXISTS public.user_notification_state (
    id integer NOT NULL,
    clerk_user_id character varying(100) NOT NULL,
    org_id character varying(100) NOT NULL,
    last_viewed_at timestamp without time zone,
    cleared_at timestamp without time zone
);
CREATE SEQUENCE IF NOT EXISTS public.user_notification_state_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.user_notification_state_id_seq OWNED BY public.user_notification_state.id;
ALTER TABLE ONLY public.audit_log ALTER COLUMN id SET DEFAULT nextval('public.audit_log_id_seq'::regclass);
ALTER TABLE ONLY public.camera_groups ALTER COLUMN id SET DEFAULT nextval('public.camera_groups_id_seq'::regclass);
ALTER TABLE ONLY public.camera_nodes ALTER COLUMN id SET DEFAULT nextval('public.camera_nodes_id_seq'::regclass);
ALTER TABLE ONLY public.cameras ALTER COLUMN id SET DEFAULT nextval('public.cameras_id_seq'::regclass);
ALTER TABLE ONLY public.email_log ALTER COLUMN id SET DEFAULT nextval('public.email_log_id_seq'::regclass);
ALTER TABLE ONLY public.email_outbox ALTER COLUMN id SET DEFAULT nextval('public.email_outbox_id_seq'::regclass);
ALTER TABLE ONLY public.email_suppression ALTER COLUMN id SET DEFAULT nextval('public.email_suppression_id_seq'::regclass);
ALTER TABLE ONLY public.incident_evidence ALTER COLUMN id SET DEFAULT nextval('public.incident_evidence_id_seq'::regclass);
ALTER TABLE ONLY public.incidents ALTER COLUMN id SET DEFAULT nextval('public.incidents_id_seq'::regclass);
ALTER TABLE ONLY public.mcp_activity_logs ALTER COLUMN id SET DEFAULT nextval('public.mcp_activity_logs_id_seq'::regclass);
ALTER TABLE ONLY public.mcp_api_keys ALTER COLUMN id SET DEFAULT nextval('public.mcp_api_keys_id_seq'::regclass);
ALTER TABLE ONLY public.motion_events ALTER COLUMN id SET DEFAULT nextval('public.motion_events_id_seq'::regclass);
ALTER TABLE ONLY public.notifications ALTER COLUMN id SET DEFAULT nextval('public.notifications_id_seq'::regclass);
ALTER TABLE ONLY public.org_monthly_usage ALTER COLUMN id SET DEFAULT nextval('public.org_monthly_usage_id_seq'::regclass);
ALTER TABLE ONLY public.processed_webhooks ALTER COLUMN id SET DEFAULT nextval('public.processed_webhooks_id_seq'::regclass);
ALTER TABLE ONLY public.sentinel_agent_keys ALTER COLUMN id SET DEFAULT nextval('public.sentinel_agent_keys_id_seq'::regclass);
ALTER TABLE ONLY public.sentinel_config ALTER COLUMN id SET DEFAULT nextval('public.sentinel_config_id_seq'::regclass);
ALTER TABLE ONLY public.settings ALTER COLUMN id SET DEFAULT nextval('public.settings_id_seq'::regclass);
ALTER TABLE ONLY public.stream_access_logs ALTER COLUMN id SET DEFAULT nextval('public.stream_access_logs_id_seq'::regclass);
ALTER TABLE ONLY public.user_notification_state ALTER COLUMN id SET DEFAULT nextval('public.user_notification_state_id_seq'::regclass);
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'audit_log_pkey'
           AND conrelid = 'public.audit_log'::regclass
    ) THEN
        ALTER TABLE ONLY public.audit_log ADD CONSTRAINT audit_log_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'camera_groups_pkey'
           AND conrelid = 'public.camera_groups'::regclass
    ) THEN
        ALTER TABLE ONLY public.camera_groups ADD CONSTRAINT camera_groups_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'camera_nodes_pkey'
           AND conrelid = 'public.camera_nodes'::regclass
    ) THEN
        ALTER TABLE ONLY public.camera_nodes ADD CONSTRAINT camera_nodes_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'cameras_pkey'
           AND conrelid = 'public.cameras'::regclass
    ) THEN
        ALTER TABLE ONLY public.cameras ADD CONSTRAINT cameras_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'email_log_pkey'
           AND conrelid = 'public.email_log'::regclass
    ) THEN
        ALTER TABLE ONLY public.email_log ADD CONSTRAINT email_log_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'email_outbox_pkey'
           AND conrelid = 'public.email_outbox'::regclass
    ) THEN
        ALTER TABLE ONLY public.email_outbox ADD CONSTRAINT email_outbox_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'email_suppression_pkey'
           AND conrelid = 'public.email_suppression'::regclass
    ) THEN
        ALTER TABLE ONLY public.email_suppression ADD CONSTRAINT email_suppression_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'incident_evidence_pkey'
           AND conrelid = 'public.incident_evidence'::regclass
    ) THEN
        ALTER TABLE ONLY public.incident_evidence ADD CONSTRAINT incident_evidence_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'incidents_pkey'
           AND conrelid = 'public.incidents'::regclass
    ) THEN
        ALTER TABLE ONLY public.incidents ADD CONSTRAINT incidents_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'mcp_activity_logs_pkey'
           AND conrelid = 'public.mcp_activity_logs'::regclass
    ) THEN
        ALTER TABLE ONLY public.mcp_activity_logs ADD CONSTRAINT mcp_activity_logs_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'mcp_api_keys_key_hash_key'
           AND conrelid = 'public.mcp_api_keys'::regclass
    ) THEN
        ALTER TABLE ONLY public.mcp_api_keys ADD CONSTRAINT mcp_api_keys_key_hash_key UNIQUE (key_hash);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'mcp_api_keys_pkey'
           AND conrelid = 'public.mcp_api_keys'::regclass
    ) THEN
        ALTER TABLE ONLY public.mcp_api_keys ADD CONSTRAINT mcp_api_keys_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'motion_events_pkey'
           AND conrelid = 'public.motion_events'::regclass
    ) THEN
        ALTER TABLE ONLY public.motion_events ADD CONSTRAINT motion_events_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'notifications_pkey'
           AND conrelid = 'public.notifications'::regclass
    ) THEN
        ALTER TABLE ONLY public.notifications ADD CONSTRAINT notifications_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'org_monthly_usage_pkey'
           AND conrelid = 'public.org_monthly_usage'::regclass
    ) THEN
        ALTER TABLE ONLY public.org_monthly_usage ADD CONSTRAINT org_monthly_usage_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'processed_webhooks_pkey'
           AND conrelid = 'public.processed_webhooks'::regclass
    ) THEN
        ALTER TABLE ONLY public.processed_webhooks ADD CONSTRAINT processed_webhooks_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'sentinel_agent_keys_pkey'
           AND conrelid = 'public.sentinel_agent_keys'::regclass
    ) THEN
        ALTER TABLE ONLY public.sentinel_agent_keys ADD CONSTRAINT sentinel_agent_keys_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'sentinel_config_pkey'
           AND conrelid = 'public.sentinel_config'::regclass
    ) THEN
        ALTER TABLE ONLY public.sentinel_config ADD CONSTRAINT sentinel_config_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'sentinel_runs_pkey'
           AND conrelid = 'public.sentinel_runs'::regclass
    ) THEN
        ALTER TABLE ONLY public.sentinel_runs ADD CONSTRAINT sentinel_runs_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'settings_pkey'
           AND conrelid = 'public.settings'::regclass
    ) THEN
        ALTER TABLE ONLY public.settings ADD CONSTRAINT settings_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'stream_access_logs_pkey'
           AND conrelid = 'public.stream_access_logs'::regclass
    ) THEN
        ALTER TABLE ONLY public.stream_access_logs ADD CONSTRAINT stream_access_logs_pkey PRIMARY KEY (id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'uq_org_monthly_usage'
           AND conrelid = 'public.org_monthly_usage'::regclass
    ) THEN
        ALTER TABLE ONLY public.org_monthly_usage ADD CONSTRAINT uq_org_monthly_usage UNIQUE (org_id, year_month);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'uq_user_notif_state_user_org'
           AND conrelid = 'public.user_notification_state'::regclass
    ) THEN
        ALTER TABLE ONLY public.user_notification_state ADD CONSTRAINT uq_user_notif_state_user_org UNIQUE (clerk_user_id, org_id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'user_notification_state_pkey'
           AND conrelid = 'public.user_notification_state'::regclass
    ) THEN
        ALTER TABLE ONLY public.user_notification_state ADD CONSTRAINT user_notification_state_pkey PRIMARY KEY (id);
    END IF;
END $$;
CREATE INDEX IF NOT EXISTS ix_audit_log_event ON public.audit_log USING btree (event);
CREATE INDEX IF NOT EXISTS ix_audit_log_org_id ON public.audit_log USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_audit_log_timestamp ON public.audit_log USING btree ("timestamp");
CREATE INDEX IF NOT EXISTS ix_camera_groups_org_id ON public.camera_groups USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_camera_nodes_api_key_hash ON public.camera_nodes USING btree (api_key_hash);
CREATE UNIQUE INDEX IF NOT EXISTS ix_camera_nodes_node_id ON public.camera_nodes USING btree (node_id);
CREATE INDEX IF NOT EXISTS ix_camera_nodes_org_id ON public.camera_nodes USING btree (org_id);
CREATE UNIQUE INDEX IF NOT EXISTS ix_cameras_camera_id ON public.cameras USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_cameras_id ON public.cameras USING btree (id);
CREATE INDEX IF NOT EXISTS ix_cameras_org_id ON public.cameras USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_email_log_kind ON public.email_log USING btree (kind);
CREATE INDEX IF NOT EXISTS ix_email_log_org_id ON public.email_log USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_email_log_org_timestamp ON public.email_log USING btree (org_id, "timestamp");
CREATE INDEX IF NOT EXISTS ix_email_log_timestamp ON public.email_log USING btree ("timestamp");
CREATE INDEX IF NOT EXISTS ix_email_outbox_created_at ON public.email_outbox USING btree (created_at);
CREATE INDEX IF NOT EXISTS ix_email_outbox_org_id ON public.email_outbox USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_email_outbox_status ON public.email_outbox USING btree (status);
CREATE INDEX IF NOT EXISTS ix_email_outbox_status_created ON public.email_outbox USING btree (status, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS ix_email_suppression_address ON public.email_suppression USING btree (address);
CREATE INDEX IF NOT EXISTS ix_incident_evidence_incident_id ON public.incident_evidence USING btree (incident_id);
CREATE INDEX IF NOT EXISTS ix_incidents_camera_id ON public.incidents USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_incidents_created_at ON public.incidents USING btree (created_at);
CREATE INDEX IF NOT EXISTS ix_incidents_org_id ON public.incidents USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_incidents_severity ON public.incidents USING btree (severity);
CREATE INDEX IF NOT EXISTS ix_incidents_status ON public.incidents USING btree (status);
CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_org_id ON public.mcp_activity_logs USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_org_timestamp ON public.mcp_activity_logs USING btree (org_id, "timestamp");
CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_timestamp ON public.mcp_activity_logs USING btree ("timestamp");
CREATE INDEX IF NOT EXISTS ix_mcp_activity_logs_tool_name ON public.mcp_activity_logs USING btree (tool_name);
CREATE INDEX IF NOT EXISTS ix_mcp_api_keys_org_id ON public.mcp_api_keys USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_motion_events_camera_id ON public.motion_events USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_motion_events_node_id ON public.motion_events USING btree (node_id);
CREATE INDEX IF NOT EXISTS ix_motion_events_org_id ON public.motion_events USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_motion_events_org_timestamp ON public.motion_events USING btree (org_id, "timestamp");
CREATE INDEX IF NOT EXISTS ix_motion_events_timestamp ON public.motion_events USING btree ("timestamp");
CREATE INDEX IF NOT EXISTS ix_notifications_camera_id ON public.notifications USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_notifications_created_at ON public.notifications USING btree (created_at);
CREATE INDEX IF NOT EXISTS ix_notifications_kind ON public.notifications USING btree (kind);
CREATE INDEX IF NOT EXISTS ix_notifications_node_id ON public.notifications USING btree (node_id);
CREATE INDEX IF NOT EXISTS ix_notifications_org_created ON public.notifications USING btree (org_id, created_at);
CREATE INDEX IF NOT EXISTS ix_notifications_org_id ON public.notifications USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_notifications_severity ON public.notifications USING btree (severity);
CREATE INDEX IF NOT EXISTS ix_org_monthly_usage_org_id ON public.org_monthly_usage USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_processed_webhooks_processed_at ON public.processed_webhooks USING btree (processed_at);
CREATE UNIQUE INDEX IF NOT EXISTS ix_processed_webhooks_svix_msg_id ON public.processed_webhooks USING btree (svix_msg_id);
CREATE UNIQUE INDEX IF NOT EXISTS ix_sentinel_agent_keys_key_hash ON public.sentinel_agent_keys USING btree (key_hash);
CREATE INDEX IF NOT EXISTS ix_sentinel_agent_keys_org_id ON public.sentinel_agent_keys USING btree (org_id);
CREATE UNIQUE INDEX IF NOT EXISTS ix_sentinel_config_org_id ON public.sentinel_config USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_sentinel_runs_camera_id ON public.sentinel_runs USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_sentinel_runs_org_id ON public.sentinel_runs USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_sentinel_runs_org_triggered ON public.sentinel_runs USING btree (org_id, triggered_at);
CREATE INDEX IF NOT EXISTS ix_sentinel_runs_triggered_at ON public.sentinel_runs USING btree (triggered_at);
CREATE INDEX IF NOT EXISTS ix_settings_key ON public.settings USING btree (key);
CREATE INDEX IF NOT EXISTS ix_settings_org_id ON public.settings USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_settings_org_key ON public.settings USING btree (org_id, key);
CREATE INDEX IF NOT EXISTS ix_stream_access_logs_accessed_at ON public.stream_access_logs USING btree (accessed_at);
CREATE INDEX IF NOT EXISTS ix_stream_access_logs_camera_id ON public.stream_access_logs USING btree (camera_id);
CREATE INDEX IF NOT EXISTS ix_stream_access_logs_org_accessed ON public.stream_access_logs USING btree (org_id, accessed_at);
CREATE INDEX IF NOT EXISTS ix_stream_access_logs_org_id ON public.stream_access_logs USING btree (org_id);
CREATE INDEX IF NOT EXISTS ix_stream_access_logs_user_id ON public.stream_access_logs USING btree (user_id);
CREATE INDEX IF NOT EXISTS ix_user_notification_state_clerk_user_id ON public.user_notification_state USING btree (clerk_user_id);
CREATE INDEX IF NOT EXISTS ix_user_notification_state_org_id ON public.user_notification_state USING btree (org_id);
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'cameras_group_id_fkey'
           AND conrelid = 'public.cameras'::regclass
    ) THEN
        ALTER TABLE ONLY public.cameras ADD CONSTRAINT cameras_group_id_fkey FOREIGN KEY (group_id) REFERENCES public.camera_groups(id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'cameras_node_id_fkey'
           AND conrelid = 'public.cameras'::regclass
    ) THEN
        ALTER TABLE ONLY public.cameras ADD CONSTRAINT cameras_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.camera_nodes(id);
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'incident_evidence_incident_id_fkey'
           AND conrelid = 'public.incident_evidence'::regclass
    ) THEN
        ALTER TABLE ONLY public.incident_evidence ADD CONSTRAINT incident_evidence_incident_id_fkey FOREIGN KEY (incident_id) REFERENCES public.incidents(id) ON DELETE CASCADE;
    END IF;
END $$;
