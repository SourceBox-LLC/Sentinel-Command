from datetime import UTC, datetime, timedelta

from sqlalchemy import (
    BigInteger,
    Boolean,
    Column,
    DateTime,
    ForeignKey,
    Index,
    Integer,
    LargeBinary,
    String,
    Text,
    UniqueConstraint,
)
from sqlalchemy.orm import deferred, relationship

from app.core.database import Base


class Camera(Base):
    __tablename__ = "cameras"

    id = Column(Integer, primary_key=True, index=True)
    camera_id = Column(String(100), unique=True, nullable=False, index=True)
    org_id = Column(String(100), nullable=False, index=True)
    node_id = Column(Integer, ForeignKey("camera_nodes.id"), nullable=True)
    name = Column(String(100), nullable=False)
    node_type = Column(String(20), default="unknown")
    capabilities = Column(String(500), default="streaming")
    group_id = Column(Integer, ForeignKey("camera_groups.id"), nullable=True)
    last_seen = Column(DateTime)
    # Pipeline state. In addition to the legacy "online" / "offline", the
    # CameraNode's FFmpeg supervisor now reports "starting", "streaming",
    # "restarting", "failed", and "error" so the UI can tell the user
    # why a camera they expect to be live isn't showing video.
    status = Column(String(20), default="offline")
    # Human-readable failure reason that goes alongside `status` when the
    # pipeline is `restarting` / `failed` / `error`. Cleared whenever the
    # node reports a healthy status.
    last_error = Column(String(500), nullable=True)
    created_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None))

    # Codec detection fields
    video_codec = Column(String(50), nullable=True)  # e.g., "avc1.42e01e"
    audio_codec = Column(String(50), nullable=True)  # e.g., "mp4a.40.2"
    codec_detected_at = Column(DateTime, nullable=True)

    # Set by `app.core.plans.enforce_camera_cap` when the owning org's plan
    # cap would otherwise be exceeded. The oldest `max_cameras` cameras
    # (by `created_at`) keep `disabled_by_plan = False`; the rest are
    # flagged, and `POST /push-segment` rejects their uploads with
    # HTTP 402 + `plan_limit_hit` so the CameraNode can surface the
    # reason in its TUI. Flag is cleared on upgrade and on re-registration.
    # Default False so fresh installs and unaffected rows behave normally.
    disabled_by_plan = Column(Boolean, nullable=False, default=False, server_default="0")

    # Per-camera recording policy (v0.1.43+).  The heartbeat handler
    # computes this camera's target recording state per-tick from
    # ``continuous_24_7 OR (scheduled_recording AND in-window)`` and
    # echoes it back to CameraNode in the heartbeat response, which
    # reconciles its in-memory recording set to match.
    #
    # Replaces the previous org-level `Setting` rows for the same
    # toggles, which never actually drove anything (they persisted
    # but no consumer read them to start recording).  Per-camera is
    # both more flexible (privacy in bedroom + always-on in garage)
    # and the granularity that matches how recording_state is keyed
    # at runtime.
    continuous_24_7 = Column(Boolean, nullable=False, default=False, server_default="0")
    scheduled_recording = Column(Boolean, nullable=False, default=False, server_default="0")
    # "HH:MM" 24-hour strings; nullable so a fresh row doesn't have to
    # commit to a window before the operator opens the toggle.  When
    # ``scheduled_recording`` is true and either field is null, the
    # heartbeat handler treats the camera as "scheduled but not
    # configured" and skips recording — same effect as off.
    scheduled_start = Column(String(5), nullable=True)
    scheduled_end = Column(String(5), nullable=True)

    group = relationship("CameraGroup", back_populates="cameras")
    node = relationship("CameraNode", back_populates="cameras")

    @property
    def effective_status(self) -> str:
        """Return the real-time status based on last_seen.
        If no heartbeat in 90s (3 missed), the camera is offline."""
        if not self.last_seen or self.status == "offline":
            return "offline"
        age = datetime.now(tz=UTC).replace(tzinfo=None) - self.last_seen
        if age > timedelta(seconds=90):
            return "offline"
        return self.status

    def to_dict(self):
        eff = self.effective_status
        # Only surface last_error when the camera is actually in a
        # broken state — once it flips back to streaming, the stale
        # reason would just confuse anyone reading the API response.
        err = self.last_error if eff in ("restarting", "failed", "error") else None
        return {
            "camera_id": self.camera_id,
            "name": self.name,
            # Parent node's *string* node_id (not the DB integer FK) so
            # frontend code can join cameras to nodes without a second
            # round-trip.  None when the camera row exists without a
            # registered node — vanishingly rare, but the cap-check
            # path can briefly leave a Camera with node_id=None during
            # node deletion.
            "node_id": self.node.node_id if self.node else None,
            # Human-readable node name (e.g. "Pi test", "Front yard").
            # Surfaced alongside node_id so the dashboard can show the
            # operator's chosen label rather than just the 8-char UUID
            # prefix.  None on an orphaned camera (rare; happens
            # transiently during node deletion when the cap-check path
            # leaves a Camera row with node_id=None).
            "node_name": self.node.name if self.node else None,
            "node_type": self.node_type,
            "capabilities": self.capabilities.split(",") if self.capabilities else [],
            # Group association: ``group_id`` is the FK (int|None) used by
            # the frontend dashboard for filter / color-tag lookup against
            # the groups list it fetches separately.  ``group`` stays as
            # the human-readable name for back-compat (MCP ``list_cameras``
            # consumers read this) — the LLM is happiest with a name it
            # can put in prose.
            "group_id": self.group_id,
            "group": self.group.name if self.group else None,
            "status": eff,
            "last_error": err,
            "last_seen": self.last_seen.isoformat() if self.last_seen else None,
            # `true` when plan enforcement has suspended this camera.
            # Push-segment returns 402 while this is set; frontend shows a
            # locked-by-plan badge and an upgrade CTA.
            "disabled_by_plan": bool(self.disabled_by_plan),
            "recording_policy": {
                "continuous_24_7": bool(self.continuous_24_7),
                "scheduled_recording": bool(self.scheduled_recording),
                "scheduled_start": self.scheduled_start,
                "scheduled_end": self.scheduled_end,
            },
        }


class CameraGroup(Base):
    __tablename__ = "camera_groups"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    name = Column(String(100), nullable=False)
    color = Column(String(7), default="#22c55e")
    icon = Column(String(10), default="📁")
    created_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None))

    cameras = relationship("Camera", back_populates="group")

    def to_dict(self):
        return {
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "icon": self.icon,
            "camera_count": len(self.cameras) if self.cameras else 0,
        }


class Setting(Base):
    __tablename__ = "settings"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    key = Column(String(100), nullable=False, index=True)
    value = Column(Text)
    updated_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), onupdate=lambda: datetime.now(tz=UTC).replace(tzinfo=None))

    # Setting.get(org_id, key) is the single most-executed query after
    # auth (plan resolution, toggles, anchors — several per request on
    # some paths).  The composite lets SQLite satisfy it with one index
    # seek instead of intersecting the two single-column indexes.
    # NOT unique: concurrent first-writers can transiently duplicate a
    # (org, key) pair (Setting.set is check-then-insert); a UNIQUE index
    # would make sync_indexes fail on any existing dup.  Created by
    # sync_indexes() on next boot.
    __table_args__ = (
        Index("ix_settings_org_key", "org_id", "key"),
    )

    @staticmethod
    def get(db, org_id: str, key: str, default: str = None) -> str:
        setting = db.query(Setting).filter_by(org_id=org_id, key=key).first()
        return setting.value if setting else default

    @staticmethod
    def get_many(db, org_id: str, keys_defaults: dict) -> dict:
        """Fetch multiple settings in a single query.
        keys_defaults: {key: default_value, ...}
        Returns: {key: value, ...}
        """
        rows = (
            db.query(Setting)
            .filter(Setting.org_id == org_id, Setting.key.in_(keys_defaults.keys()))
            .all()
        )
        found = {row.key: row.value for row in rows}
        return {k: found.get(k, default) for k, default in keys_defaults.items()}

    @staticmethod
    def set(db, org_id: str, key: str, value: str, *, commit: bool = True):
        """Upsert one setting.

        ``commit=False`` for multi-write flows (the Clerk webhook
        handler) that need a SINGLE terminal commit: the default
        auto-commit made the handler's "process-then-mark, retry-safe"
        idempotency story an illusion — every Setting.set persisted
        immediately, so a raise midway left plan settings committed
        while camera enable/disable state and the ProcessedWebhook
        marker were not.  Callers passing commit=False must flush()
        before any reader that needs the value in-transaction.
        """
        setting = db.query(Setting).filter_by(org_id=org_id, key=key).first()
        if setting:
            setting.value = value
        else:
            setting = Setting(org_id=org_id, key=key, value=value)
            db.add(setting)
        if commit:
            db.commit()
        return setting


class AuditLog(Base):
    __tablename__ = "audit_log"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    timestamp = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), index=True)
    event = Column(String(50), nullable=False, index=True)
    ip_address = Column(String(45))
    username = Column(String(80))
    user_id = Column(String(100))
    details = Column(Text)

    def to_dict(self):
        return {
            "id": self.id,
            "timestamp": self.timestamp.isoformat(),
            "event": self.event,
            "ip": self.ip_address,
            "username": self.username,
            "details": self.details,
        }


class CameraNode(Base):
    __tablename__ = "camera_nodes"

    id = Column(Integer, primary_key=True)
    node_id = Column(String(100), unique=True, nullable=False, index=True)
    org_id = Column(String(100), nullable=False, index=True)
    # Indexed: every node-authenticated request (push-segment at up to
    # 20/s/node, playlist, heartbeat, motion, codec) looks the node up
    # by this hash — unindexed it was a full table scan per request.
    api_key_hash = Column(String(128), nullable=False, index=True)
    name = Column(String(100), nullable=False)
    hostname = Column(String(100))
    local_ip = Column(String(45))
    http_port = Column(Integer, default=8080)
    status = Column(String(20), default="offline")
    last_seen = Column(DateTime)
    key_rotated_at = Column(DateTime, nullable=True)
    created_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None))
    video_codec = Column(String(50), nullable=True)
    audio_codec = Column(String(50), nullable=True)
    codec_detected_at = Column(DateTime, nullable=True)
    # Surfaces the most recent registration / auth failure to the UI so
    # a node stuck in ``pending`` can show *why* it's stuck (bad API key,
    # plan limit hit, etc.) instead of the user staring at an opaque
    # status badge.  Cleared on successful re-registration.
    last_register_error = Column(String(500), nullable=True)
    last_register_error_at = Column(DateTime, nullable=True)
    # CameraNode-reported build version (e.g. "0.1.0") + when we last saw it.
    # Updated by both register and heartbeat; nullable so very old nodes that
    # pre-date version reporting can still register without failing migration.
    # Used by the dashboard to surface "update available" badges and by
    # versions.check_node_version() to gate registrations from too-old nodes.
    node_version = Column(String(50), nullable=True)
    version_checked_at = Column(DateTime, nullable=True)

    # Filesystem-aware storage stats reported by CameraNode on heartbeat
    # (v0.1.41+).  Used by Settings → Camera Nodes to render a per-node
    # usage bar (used vs configured cap) and warn when the underlying
    # host disk is filling up.  All four are nullable so v0.1.40 and
    # earlier nodes still register / heartbeat without failing migration.
    storage_used_bytes = Column(BigInteger, nullable=True)
    storage_max_bytes = Column(BigInteger, nullable=True)
    storage_disk_free_bytes = Column(BigInteger, nullable=True)
    storage_disk_total_bytes = Column(BigInteger, nullable=True)
    storage_reported_at = Column(DateTime, nullable=True)

    cameras = relationship(
        "Camera", back_populates="node", cascade="all, delete-orphan"
    )

    @property
    def effective_status(self) -> str:
        """Return the real-time status based on last_seen.
        If no heartbeat in 90s (3 missed), the node is offline."""
        if not self.last_seen or self.status in ("offline", "pending"):
            return self.status or "offline"
        age = datetime.now(tz=UTC).replace(tzinfo=None) - self.last_seen
        if age > timedelta(seconds=90):
            return "offline"
        return self.status

    def to_dict(self):
        return {
            "node_id": self.node_id,
            "name": self.name,
            "hostname": self.hostname,
            "local_ip": self.local_ip,
            "http_port": self.http_port,
            "status": self.effective_status,
            "last_seen": self.last_seen.isoformat() if self.last_seen else None,
            "key_rotated_at": self.key_rotated_at.isoformat()
            if self.key_rotated_at
            else None,
            "camera_count": len(self.cameras) if self.cameras else 0,
            "created_at": self.created_at.isoformat() if self.created_at else None,
            "video_codec": self.video_codec,
            "audio_codec": self.audio_codec,
            "last_register_error": self.last_register_error,
            "last_register_error_at": self.last_register_error_at.isoformat()
            if self.last_register_error_at
            else None,
            "node_version": self.node_version,
            "version_checked_at": self.version_checked_at.isoformat()
            if self.version_checked_at
            else None,
            "storage": {
                "used_bytes": self.storage_used_bytes,
                "max_bytes": self.storage_max_bytes,
                "disk_free_bytes": self.storage_disk_free_bytes,
                "disk_total_bytes": self.storage_disk_total_bytes,
                "reported_at": self.storage_reported_at.isoformat()
                if self.storage_reported_at
                else None,
            } if self.storage_reported_at else None,
        }


class StreamAccessLog(Base):
    __tablename__ = "stream_access_logs"

    id = Column(Integer, primary_key=True)
    user_id = Column(String(100), nullable=False, index=True)
    user_email = Column(String(255), default="")
    org_id = Column(String(100), nullable=False, index=True)
    camera_id = Column(String(100), nullable=False, index=True)
    node_id = Column(String(100), nullable=False)
    ip_address = Column(String(45))
    user_agent = Column(String(500))
    accessed_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), index=True)

    # Composite index for the hot audit-log query pattern in
    # api/audit.py: WHERE org_id = ? AND accessed_at >= ? ORDER BY
    # accessed_at DESC (and the activity-aggregate variants that
    # filter org_id + accessed_at then group_by something else).
    # Single-column indexes on each column already exist above, but a
    # composite lets SQLite (and Postgres if we ever migrate) do a
    # straight index range-scan instead of intersecting two
    # single-column index hits, which materially matters once an org
    # has > a few thousand log rows.  SQLAlchemy create_all() is
    # idempotent — adding this index applies on the next boot.
    __table_args__ = (
        Index("ix_stream_access_logs_org_accessed", "org_id", "accessed_at"),
    )

    def to_dict(self):
        return {
            "id": self.id,
            "user_id": self.user_id,
            "user_email": self.user_email or "",
            "org_id": self.org_id,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "ip_address": self.ip_address,
            "accessed_at": self.accessed_at.isoformat(),
        }


class McpApiKey(Base):
    __tablename__ = "mcp_api_keys"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    key_hash = Column(String(128), nullable=False, unique=True)
    name = Column(String(100), nullable=False)
    created_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None))
    last_used_at = Column(DateTime, nullable=True)
    revoked = Column(Boolean, default=False)

    # Scope configuration — controls which MCP tools this key can invoke.
    #   scope_mode = "all"       → every tool is allowed (default for existing keys)
    #   scope_mode = "readonly"  → only read-classified tools (no mutations)
    #   scope_mode = "custom"    → scope_tools holds a JSON list of allowed tool names
    # A NULL scope_mode behaves like "all" so legacy rows keep working.
    scope_mode = Column(String(20), nullable=True, default="all")
    scope_tools = Column(Text, nullable=True)

    # Distinguishes an MCP-protocol key (osc_, the default) from a REST
    # integration key (osi_, e.g. Home Assistant). Both kinds live in this
    # one table to reuse the minting / hashing / revocation / audit
    # machinery, but every auth AND management query filters on `kind` so
    # a key of one kind can NEVER authenticate as — or be listed/revoked
    # alongside — the other. server_default backfills every legacy row to
    # "mcp" on the next boot, so existing keys keep working. The default is
    # SQL-quoted ("'mcp'") because migrations.py emits the value verbatim
    # into the ADD COLUMN DDL (an unquoted "mcp" would be invalid SQL).
    kind = Column(String(20), nullable=False, default="mcp", server_default="'mcp'")

    def get_scope_tools(self) -> list[str]:
        """Return the parsed scope_tools list, or [] if unset/invalid."""
        if not self.scope_tools:
            return []
        import json
        try:
            val = json.loads(self.scope_tools)
            if isinstance(val, list):
                return [str(v) for v in val]
        except (ValueError, TypeError):
            pass
        return []

    def to_dict(self):
        return {
            "id": self.id,
            "name": self.name,
            "created_at": self.created_at.isoformat() if self.created_at else None,
            "last_used_at": self.last_used_at.isoformat() if self.last_used_at else None,
            "revoked": self.revoked,
            "scope_mode": self.scope_mode or "all",
            "scope_tools": self.get_scope_tools(),
            "kind": self.kind or "mcp",
        }


class McpActivityLog(Base):
    __tablename__ = "mcp_activity_logs"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    tool_name = Column(String(100), nullable=False, index=True)
    key_name = Column(String(100), nullable=False)
    status = Column(String(20), nullable=False)
    duration_ms = Column(Integer)
    args_summary = Column(String(500))
    error = Column(String(500))
    timestamp = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), index=True)

    # Composite index for the hot admin-query pattern in
    # api/mcp_activity.py: WHERE org_id = ? AND timestamp >= ? ORDER BY
    # timestamp DESC.  Single-column indexes on org_id and timestamp
    # already exist above; the composite lets SQLite (and Postgres if
    # we ever migrate) do a single index range-scan instead of
    # intersecting two single-column hits.  Mirrors the pattern set by
    # StreamAccessLog above — bites at scale (~10K+ rows per org).
    # SQLAlchemy create_all() / sync_schema() pick this up on next
    # boot.
    __table_args__ = (
        Index("ix_mcp_activity_logs_org_timestamp", "org_id", "timestamp"),
    )

    def to_dict(self):
        return {
            "id": self.id,
            "org_id": self.org_id,
            "tool_name": self.tool_name,
            "key_name": self.key_name,
            "status": self.status,
            "duration_ms": self.duration_ms,
            "args_summary": self.args_summary,
            "error": self.error,
            "timestamp": self.timestamp.isoformat() if self.timestamp else None,
        }


# ---------------------------------------------------------------------------
# AI-generated incident reports
# ---------------------------------------------------------------------------

INCIDENT_SEVERITIES = ("low", "medium", "high", "critical")
INCIDENT_STATUSES = ("open", "acknowledged", "resolved", "dismissed")


class Incident(Base):
    """An incident report — typically authored by an MCP agent investigating
    suspicious activity, but reviewable + actionable from the dashboard."""

    __tablename__ = "incidents"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    camera_id = Column(String(100), nullable=True, index=True)
    title = Column(String(200), nullable=False)
    summary = Column(Text, nullable=False)
    report = Column(Text, default="")
    severity = Column(String(20), nullable=False, default="medium", index=True)
    status = Column(String(20), nullable=False, default="open", index=True)
    created_by = Column(String(150), nullable=False)  # mcp:<key_name> or user:<clerk_id>
    created_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), index=True)
    updated_at = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None), onupdate=lambda: datetime.now(tz=UTC).replace(tzinfo=None))
    resolved_at = Column(DateTime, nullable=True)
    resolved_by = Column(String(150), nullable=True)

    evidence = relationship(
        "IncidentEvidence",
        back_populates="incident",
        cascade="all, delete-orphan",
        order_by="IncidentEvidence.timestamp",
    )

    def to_dict(self, include_evidence: bool = False) -> dict:
        d = {
            "id": self.id,
            "camera_id": self.camera_id,
            "title": self.title,
            "summary": self.summary,
            "report": self.report or "",
            "severity": self.severity,
            "status": self.status,
            "created_by": self.created_by,
            "created_at": self.created_at.isoformat() if self.created_at else None,
            "updated_at": self.updated_at.isoformat() if self.updated_at else None,
            "resolved_at": self.resolved_at.isoformat() if self.resolved_at else None,
            "resolved_by": self.resolved_by,
            "evidence_count": len(self.evidence) if self.evidence is not None else 0,
        }
        if include_evidence:
            d["evidence"] = [e.to_dict() for e in self.evidence]
        return d


class IncidentEvidence(Base):
    """A piece of evidence attached to an incident: a snapshot, a text
    observation, or a logged action the agent took."""

    __tablename__ = "incident_evidence"

    id = Column(Integer, primary_key=True)
    incident_id = Column(Integer, ForeignKey("incidents.id", ondelete="CASCADE"), nullable=False, index=True)
    kind = Column(String(20), nullable=False)  # "snapshot" | "observation" | "action"
    text = Column(Text, nullable=True)
    camera_id = Column(String(100), nullable=True)
    # deferred(): clips run to ~tens of MB each, and EVERY consumer of
    # Incident.to_dict() (list_incidents, get_incident, update/finalize,
    # the dashboard REST list) touches the evidence relationship just to
    # COUNT it — without deferral that materialized every blob of every
    # listed incident into RAM (multi-GB on the 1 GiB VM from one plain
    # read call).  Deferred columns load lazily on attribute access, so
    # the blob-serving paths (evidence streaming, clip playback) work
    # unchanged while counts stay light.
    data = deferred(Column(LargeBinary, nullable=True))
    data_mime = Column(String(50), nullable=True)
    timestamp = Column(DateTime, default=lambda: datetime.now(tz=UTC).replace(tzinfo=None))

    incident = relationship("Incident", back_populates="evidence")

    def to_dict(self) -> dict:
        # Never inline blob bytes — clients fetch them separately.
        # has_data via data_mime, NOT self.data: `data` is deferred(),
        # so touching it here would lazy-load the multi-MB blob per row
        # — recreating exactly the bulk-materialization the deferral
        # exists to prevent.  attach_snapshot/attach_clip always set
        # data_mime together with data; observations set neither.
        return {
            "id": self.id,
            "incident_id": self.incident_id,
            "kind": self.kind,
            "text": self.text,
            "camera_id": self.camera_id,
            "has_data": self.data_mime is not None,
            "data_mime": self.data_mime,
            "timestamp": self.timestamp.isoformat() if self.timestamp else None,
        }


class MotionEvent(Base):
    """A motion detection event reported by a CameraNode.

    Created when a node's FFmpeg scene-change analysis exceeds the
    configured threshold for a camera segment.
    """

    __tablename__ = "motion_events"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    camera_id = Column(String(100), nullable=False, index=True)
    node_id = Column(String(100), nullable=False, index=True)
    score = Column(Integer, nullable=False)  # 0-100 (normalised)
    segment_seq = Column(Integer, nullable=True)
    timestamp = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        index=True,
    )

    # Composite index for the hot dashboard-feed query pattern:
    # WHERE org_id = ? AND timestamp >= ? ORDER BY timestamp DESC.
    # Notifications inbox + motion-event listing both hit this shape.
    # Mirrors the StreamAccessLog and McpActivityLog composites — at
    # 100K+ events per high-traffic org the single-column indexes
    # become noticeably slower than a true range scan.  Picked up by
    # sync_schema() on next boot.
    __table_args__ = (
        Index("ix_motion_events_org_timestamp", "org_id", "timestamp"),
    )

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "org_id": self.org_id,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "score": self.score,
            "segment_seq": self.segment_seq,
            "timestamp": self.timestamp.isoformat() if self.timestamp else None,
        }


class Notification(Base):
    """A user-facing notification in the org inbox.

    Unified feed for motion detection, camera/node status transitions,
    and (future) system errors.  Read state is tracked per-user via
    ``UserNotificationState.last_viewed_at``.
    """

    __tablename__ = "notifications"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)

    # Event type discriminator, e.g.
    #   "motion", "camera_offline", "camera_online",
    #   "node_offline", "node_online", "error"
    kind = Column(String(40), nullable=False, index=True)

    # Who should see this.  "all" = every member of the org,
    # "admin" = only users with admin role.  The inbox endpoint
    # filters based on the caller's role.
    audience = Column(String(20), nullable=False, default="all")

    # Display copy
    title = Column(String(200), nullable=False)
    body = Column(Text, nullable=False, default="")

    # "info" | "warning" | "error" | "critical"
    severity = Column(String(20), nullable=False, default="info", index=True)

    # Optional deep-link (relative path) so clicking the notification
    # jumps to a camera / incident / settings page.
    link = Column(String(500), nullable=True)

    # Optional subject references (kept as strings, not FKs, so a notification
    # still renders even if the camera or node is later deleted).
    camera_id = Column(String(100), nullable=True, index=True)
    node_id = Column(String(100), nullable=True, index=True)

    # Free-form extra data as a JSON string (e.g. motion score, segment_seq).
    # Kept small — not meant to be queried, just rendered.
    meta_json = Column(Text, nullable=True)

    created_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        index=True,
    )

    # The bell polls unread-count (COUNT WHERE org_id=? AND created_at>?)
    # and the inbox lists WHERE org_id=? ORDER BY created_at DESC on
    # every mount — an active org accumulates 100K+ rows within
    # retention, so the single-column indexes force a full org-partition
    # scan per poll.  Created by sync_indexes() on next boot.
    __table_args__ = (
        Index("ix_notifications_org_created", "org_id", "created_at"),
    )

    def to_dict(self) -> dict:
        import json as _json
        meta = None
        if self.meta_json:
            try:
                meta = _json.loads(self.meta_json)
            except (ValueError, TypeError):
                meta = None
        return {
            "id": self.id,
            "kind": self.kind,
            "audience": self.audience,
            "title": self.title,
            "body": self.body,
            "severity": self.severity,
            "link": self.link,
            "camera_id": self.camera_id,
            "node_id": self.node_id,
            "meta": meta,
            "created_at": self.created_at.isoformat() if self.created_at else None,
        }


class UserNotificationState(Base):
    """Per-user read-state for the notification inbox.

    One row per (Clerk user, org) combination.

    ``last_viewed_at`` is bumped when the user opens the notification
    panel; the unread count is computed as
    ``COUNT(*) WHERE created_at > last_viewed_at``.

    ``cleared_at`` is bumped when the user clicks "Clear all" — the
    inbox list then hides everything with ``created_at <= cleared_at``.
    Soft-hide (per user) rather than hard-delete (org-wide) so one
    user clearing their view doesn't erase history for teammates or
    for incidents/audit queries.  Nullable so pre-existing rows don't
    need a backfill; ``None`` means "never cleared".
    """

    __tablename__ = "user_notification_state"

    id = Column(Integer, primary_key=True)
    clerk_user_id = Column(String(100), nullable=False, index=True)
    org_id = Column(String(100), nullable=False, index=True)
    last_viewed_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
    )
    cleared_at = Column(DateTime, nullable=True)

    __table_args__ = (
        # One read-state row per user per org.
        UniqueConstraint("clerk_user_id", "org_id", name="uq_user_notif_state_user_org"),
    )


class ProcessedWebhook(Base):
    """Dedupe ledger for incoming Clerk webhooks.

    Svix (Clerk's webhook signer) retries on any non-2xx response and on
    network failure.  Without idempotency, a transient hiccup in our
    handler can cause Clerk to redeliver the SAME ``user.created`` /
    ``subscription.updated`` event, and the handler would re-run all
    its side effects (re-set member limits, re-upsert plan settings,
    re-fire enforce_camera_cap).  Most operations are upserts so
    re-running is benign, but anything that reads-then-writes is at
    risk of doubling — and the fix is cheap.

    The ``svix_msg_id`` is the unique message identifier Svix sends in
    the ``svix-id`` header on every delivery (same id across retries).
    Insert at the end of successful handler execution; future retries
    short-circuit on lookup.

    Cleanup: a periodic sweep can drop rows older than 30 days
    (Svix's max retry window is 5 days, so 30d is comfortable).
    """

    __tablename__ = "processed_webhooks"

    id = Column(Integer, primary_key=True)
    svix_msg_id = Column(String(255), nullable=False, unique=True, index=True)
    event_type = Column(String(100), default="")
    processed_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        index=True,
    )


class OrgMonthlyUsage(Base):
    """Per-org viewer-seconds counter, bucketed by calendar month (UTC).

    Used to enforce per-tier monthly viewer-hour caps (see
    ``PLAN_LIMITS[plan]["max_viewer_hours_per_month"]``). Each cached HLS
    segment served to an authenticated viewer is ~1 second of video, so
    the counter increments by 1 per successful segment delivery.

    Writes happen out-of-band from the request path: a process-local
    in-memory accumulator (see ``app.api.hls.flush_viewer_usage``) flushes
    pending increments every ~60 seconds with a single UPSERT per org, so
    the hot HLS-serve path never touches the DB. Cap-enforcement reads the
    cached total + the pending in-memory delta before serving, so an org
    that's currently blowing past its cap gets blocked on the next segment.

    ``year_month`` is stored as a ``YYYY-MM`` string for clean
    human-readable debugging and trivial month-rollover queries. The
    ``(org_id, year_month)`` uniqueness guarantee is what makes the
    background UPSERT safe.
    """

    __tablename__ = "org_monthly_usage"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    year_month = Column(String(7), nullable=False)  # "YYYY-MM"
    viewer_seconds = Column(Integer, nullable=False, default=0)
    updated_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        onupdate=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
    )

    __table_args__ = (
        UniqueConstraint("org_id", "year_month", name="uq_org_monthly_usage"),
    )

    def to_dict(self) -> dict:
        return {
            "org_id": self.org_id,
            "year_month": self.year_month,
            "viewer_seconds": self.viewer_seconds,
            "viewer_hours": round(self.viewer_seconds / 3600.0, 2),
            "updated_at": self.updated_at.isoformat() if self.updated_at else None,
        }


# ── Email notifications (Resend) ─────────────────────────────────────
# Three tables back the operator-critical email pipeline:
#
#   EmailOutbox      — pending sends, drained by app/core/email_worker.py
#   EmailLog         — append-only audit trail of every send attempt
#   EmailSuppression — local mirror of Resend's suppression list
#                      (bounces, complaints, manual unsubscribes)
#
# Why three tables and not one row that mutates through statuses:
#   - Outbox rows get cleaned up (the dashboard doesn't need to render
#     last week's "sent" rows; the audit trail does).
#   - Suppression is a different lifecycle (per-address forever, not
#     per-message) and needs a UNIQUE on the address.
#   - Splitting them keeps the worker's hot SELECT narrow.
#
# See plans/gentle-coalescing-teacup.md for the full design rationale.

class EmailOutbox(Base):
    """Pending email send.  Populated by ``create_notification()`` when
    an org has email enabled for the notification kind; drained by
    ``app/core/email_worker.py`` in batches.

    Surviving process restart is the whole point of having a table
    here (vs. fire-and-forget asyncio).  A Fly machine restart that
    loses an in-flight ``camera_offline`` send is exactly the kind of
    silent failure we don't want for security alerts.

    The (status, created_at) composite index is what the worker scans
    every tick — "give me the oldest pending rows, in order."  Ordering
    by created_at preserves the original event order so the operator
    sees alerts in the sequence they fired, not in random worker
    pickup order.

    No FK to Notification by design: Notification rows can be cleared
    by the per-user inbox prefs without breaking the audit trail of
    what actually emailed out.  ``notification_id`` is a soft
    reference for traceability only.
    """

    __tablename__ = "email_outbox"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    # 320 = max RFC 5321 local + @ + domain length.
    recipient_email = Column(String(320), nullable=False)
    subject = Column(String(500), nullable=False)
    body_text = Column(Text, nullable=False, default="")
    body_html = Column(Text, nullable=False, default="")
    # Matches Notification.kind so the worker can tag the Resend
    # message and the per-event prefs gate stays trivially derivable.
    kind = Column(String(40), nullable=False)
    # Soft reference back to the originating Notification row.  Not a
    # FK because Notification rows can be cleaned up by retention
    # without stranding outbox sends in the middle of the worker tick.
    notification_id = Column(Integer, nullable=True)
    # 'pending' | 'sending' | 'sent' | 'failed' | 'suppressed'
    # 'sending' is the lock state — claimed by a worker, not yet
    # acknowledged by Resend.  A row stuck in 'sending' for >60s on
    # the next tick gets reclaimed (worker restart mid-flight).
    status = Column(String(20), nullable=False, default="pending", index=True)
    attempts = Column(Integer, nullable=False, default=0)
    last_attempt_at = Column(DateTime, nullable=True)
    sent_at = Column(DateTime, nullable=True)
    # Resend's id for the message.  Used for dashboard deep-links and
    # for correlating webhook events back to our outbox row.
    resend_message_id = Column(String(100), nullable=True)
    error = Column(Text, nullable=True)
    created_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        index=True,
    )

    __table_args__ = (
        # The worker's hot scan: WHERE status='pending' ORDER BY created_at.
        Index("ix_email_outbox_status_created", "status", "created_at"),
    )

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "org_id": self.org_id,
            "recipient_email": self.recipient_email,
            "subject": self.subject,
            "kind": self.kind,
            "status": self.status,
            "attempts": self.attempts,
            "resend_message_id": self.resend_message_id,
            "error": self.error,
            "created_at": self.created_at.isoformat() if self.created_at else None,
            "sent_at": self.sent_at.isoformat() if self.sent_at else None,
        }


class EmailLog(Base):
    """Audit trail — one row per send attempt outcome.

    Used to render "recent emails" in the dashboard, debug "why
    didn't I get an email?" tickets, and (eventually) feed the
    weekly digest of what the operator was alerted about.

    Mirrors AuditLog's shape on purpose — same retention story
    (per-org tiered cleanup in run_log_cleanup), same scannable
    shape, same error-swallowing write pattern.  We keep this
    separate from EmailOutbox so the outbox stays narrow (the
    worker's hot path) while the audit trail can grow without
    bound until retention sweeps it.
    """

    __tablename__ = "email_log"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)
    timestamp = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        index=True,
    )
    recipient_email = Column(String(320), nullable=False)
    kind = Column(String(40), nullable=False, index=True)
    # 'sent' | 'failed' | 'suppressed'.  The transient 'sending' /
    # 'pending' states never make it here — they only exist on the
    # outbox row before the final outcome.
    status = Column(String(20), nullable=False)
    resend_message_id = Column(String(100), nullable=True)
    error = Column(Text, nullable=True)

    __table_args__ = (
        # Hot query: "show me this org's recent email activity."
        Index("ix_email_log_org_timestamp", "org_id", "timestamp"),
    )

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "timestamp": self.timestamp.isoformat() if self.timestamp else None,
            "recipient_email": self.recipient_email,
            "kind": self.kind,
            "status": self.status,
            "resend_message_id": self.resend_message_id,
            "error": self.error,
        }


class EmailSuppression(Base):
    """Local mirror of Resend's suppression list.

    Resend's webhook tells us when an address bounces, complains
    (marked spam), or manually unsubscribes.  We persist these so
    the worker can short-circuit before the API call — saves a
    round-trip and prevents accidentally re-suppressing an address
    Resend already knows about (which counts against deliverability
    reputation).

    Address is the natural key (UNIQUE) because the same address
    can't be suppressed twice for different reasons — most recent
    reason wins.  Inserts are upserts in practice (handler swallows
    UniqueConstraint violations silently).

    Manual unsubscribes (via the email's unsubscribe link) write
    here too with source='unsubscribe', so the worker treats them
    identically to bounces — no special-case code on the send path.
    """

    __tablename__ = "email_suppression"

    id = Column(Integer, primary_key=True)
    address = Column(String(320), nullable=False, unique=True, index=True)
    # 'bounce' | 'complaint' | 'unsubscribe' | 'manual'
    reason = Column(String(40), nullable=False)
    # 'resend_webhook' | 'unsubscribe_link' | 'admin_action'
    source = Column(String(40), nullable=False)
    created_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
    )

    def to_dict(self) -> dict:
        return {
            "address": self.address,
            "reason": self.reason,
            "source": self.source,
            "created_at": self.created_at.isoformat() if self.created_at else None,
        }


# ── Sentinel ──────────────────────────────────────────────────────────
# Per-org configuration + run history for the autonomous "Sentinel"
# security agent.  This is slice 1 of the Sentinel rollout (UI-only
# persistence) — the agent itself is not yet wired up, so SentinelRun
# rows will only exist once slice 3 lands.  See plans/ for the
# 7-slice roadmap.
class SentinelConfig(Base):
    """Per-org configuration for the Sentinel agent.

    One row per org, lazily upserted on first GET via
    `_ensure_config_row()` in app/api/sentinel.py.  All boolean +
    string columns carry sane defaults so a fresh row is immediately
    usable without a separate setup step.

    JSON columns (`active_days`, `camera_scope`) are stored as
    serialised TEXT to match the rest of the codebase's pattern (see
    `Notification.meta_json`, `McpApiKey.scope_tools`).
    """

    __tablename__ = "sentinel_config"

    id = Column(Integer, primary_key=True)
    org_id = Column(String(100), unique=True, nullable=False, index=True)

    # Master kill-switch for the whole agent.  When false, no triggers
    # fire regardless of the per-trigger toggles below.
    enabled = Column(Boolean, default=True, nullable=False)

    # Per-trigger subscription toggles.  Sentinel is intentionally
    # narrow — only security-relevant events fire it.  Infrastructure
    # events (camera_offline, node_offline, disk_low) and admin events
    # (member changes, MCP key audit) deliberately go elsewhere.
    motion_enabled = Column(Boolean, default=True, nullable=False)
    incident_opened_enabled = Column(Boolean, default=True, nullable=False)

    # Per-camera cooldown in minutes — limits how often Sentinel runs
    # for the same camera.  Independent of the email-digest cooldown.
    motion_cooldown_min = Column(Integer, default=5, nullable=False)

    # Schedule mode + window.  schedule_mode is one of:
    #   "always"     — run on every configured trigger, 24/7
    #   "scheduled"  — only respond during the schedule_start..end window
    #   "off"        — never run, regardless of triggers
    # Times are HH:MM in the org's timezone.  active_days is a JSON
    # list of {mon,tue,wed,thu,fri,sat,sun} keys.
    schedule_mode = Column(String(20), default="always", nullable=False)
    schedule_start = Column(String(5), default="22:00", nullable=False)
    schedule_end = Column(String(5), default="06:00", nullable=False)
    active_days = Column(Text, nullable=True)  # JSON list, see helpers below

    # Per-camera scope.  JSON object {"<camera_id>": true|false, ...}
    # — true means Sentinel may investigate that camera.  Cameras
    # absent from the dict default to true (everything in scope) so
    # existing cameras don't silently disappear from the agent's
    # purview when a new camera is added.
    camera_scope = Column(Text, nullable=True)  # JSON object

    created_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
    )
    updated_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        onupdate=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
    )

    # ── JSON helpers (mirror McpApiKey.get_scope_tools pattern) ───
    def get_active_days(self) -> list[str]:
        if not self.active_days:
            return ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        import json as _json
        try:
            val = _json.loads(self.active_days)
            if isinstance(val, list):
                return [str(v) for v in val]
        except (ValueError, TypeError):
            pass
        return ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]

    def set_active_days(self, days: list[str]) -> None:
        import json as _json
        self.active_days = _json.dumps(list(days))

    def get_camera_scope(self) -> dict:
        if not self.camera_scope:
            return {}
        import json as _json
        try:
            val = _json.loads(self.camera_scope)
            if isinstance(val, dict):
                return {str(k): bool(v) for k, v in val.items()}
        except (ValueError, TypeError):
            pass
        return {}

    def set_camera_scope(self, scope: dict) -> None:
        import json as _json
        self.camera_scope = _json.dumps({str(k): bool(v) for k, v in scope.items()})

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "motion_enabled": self.motion_enabled,
            "incident_opened_enabled": self.incident_opened_enabled,
            "motion_cooldown_min": self.motion_cooldown_min,
            "schedule_mode": self.schedule_mode,
            "schedule_start": self.schedule_start,
            "schedule_end": self.schedule_end,
            "active_days": self.get_active_days(),
            "camera_scope": self.get_camera_scope(),
            "created_at": self.created_at.isoformat() if self.created_at else None,
            "updated_at": self.updated_at.isoformat() if self.updated_at else None,
        }


class SentinelRun(Base):
    """One row per Sentinel agent run.

    Slice 1: rows are not yet produced (no agent service exists).
    Slice 3: the agent posts back to a new internal endpoint that
    inserts rows here.  Slice 7: the run-detail drawer shows real
    `tool_trace` instead of the placeholder.

    `incident_id` intentionally not a FK constraint — incidents are
    tracked in their own table and we don't want a cascade rule to
    entangle the two on delete.
    """

    __tablename__ = "sentinel_runs"

    # Use string UUID hex for the id so the agent can synthesize one
    # client-side and the DB doesn't need to round-trip an
    # auto-increment back.
    id = Column(String(32), primary_key=True)
    org_id = Column(String(100), nullable=False, index=True)

    triggered_at = Column(
        DateTime,
        default=lambda: datetime.now(tz=UTC).replace(tzinfo=None),
        nullable=False,
        index=True,
    )

    # One of: motion | incident_opened | manual | scheduled
    trigger_type = Column(String(40), nullable=False)
    # Camera the trigger was for; nullable for scheduled sweeps that
    # cover all cameras at once.
    camera_id = Column(String(100), nullable=True, index=True)

    tool_call_count = Column(Integer, default=0, nullable=False)
    # Unified state field.  Values:
    #   pending   — created by the dispatcher (notification hook /
    #               manual run / scheduled cron); not yet picked up
    #               by the agent.
    #   running   — agent has claimed the run and is working on it.
    #               (Optional intermediate state; the agent may jump
    #               straight to a terminal outcome.)
    #   incident  — terminal: agent filed an incident.
    #   no_action — terminal: agent decided not to file.
    #   error     — terminal: agent errored mid-run.
    # Slice 1 used this column only for terminal outcomes; slice 2
    # widens it to cover the pending-run state machine.
    outcome = Column(String(20), nullable=False)
    # Severity if outcome=incident: low | medium | high | critical (else NULL)
    severity = Column(String(20), nullable=True)
    # Foreign reference to the filed incident (no FK constraint —
    # see class docstring).
    incident_id = Column(Integer, nullable=True)

    # When the agent picked up the run (entered running state) and
    # when it finished.  Both nullable for pending runs.
    started_at = Column(DateTime, nullable=True)
    completed_at = Column(DateTime, nullable=True)

    # Operator-provided prompt for trigger_type=manual runs (the text
    # the user typed in the "Run now" modal).  NULL for automated
    # triggers (motion / incident_opened / scheduled).
    manual_prompt = Column(Text, nullable=True)

    # Human-readable agent reasoning summary (the body of what the
    # agent concluded).  Truncated server-side to ~2KB at write time
    # in slice 3 to keep rows small.
    summary = Column(Text, default="")
    # JSON list of tool-call entries: [{"tool", "args", "result"}, ...]
    # Truncated server-side to last 50 entries at write time.
    tool_trace = Column(Text, nullable=True)

    # Composite index for the hot path: list runs for an org ordered
    # by recency.  `sync_schema` doesn't add indexes after the fact,
    # so this MUST be defined on the model the first time the table
    # is created.
    __table_args__ = (
        Index("ix_sentinel_runs_org_triggered", "org_id", "triggered_at"),
    )

    def get_tool_trace(self) -> list[dict]:
        if not self.tool_trace:
            return []
        import json as _json
        try:
            val = _json.loads(self.tool_trace)
            if isinstance(val, list):
                return val
        except (ValueError, TypeError):
            pass
        return []

    def set_tool_trace(self, trace: list[dict]) -> None:
        """Truncate trace to keep row sizes bounded.

        Two layers of cap:

        1. **Last 50 entries** — protects against a runaway agent that
           emits hundreds of tool calls before terminating.
        2. **Per-entry size limits** — protects against a single tool
           result that's huge (e.g. a leaked agent posting a multi-MB
           blob in `result`).  Without this, 50 × N-MB entries would
           bloat the SentinelRun row to N × 50 MB.

        Per-entry caps mirror what the Sentinel agent already does
        in `agent.py:_sanitize_args` + the 800-char result truncation
        in the trace builder, so a well-behaved agent's output is
        unchanged; this is purely a server-side belt for misbehaving
        clients.
        """
        import json as _json

        def _cap_str(value, limit: int) -> str:
            if not isinstance(value, str):
                value = str(value)
            return value if len(value) <= limit else value[:limit] + "…"

        sanitized: list[dict] = []
        for entry in list(trace)[-50:]:
            if not isinstance(entry, dict):
                continue
            tool_name = _cap_str(entry.get("tool", ""), 200)
            result_text = _cap_str(entry.get("result", ""), 1000)
            raw_args = entry.get("args", {})
            if not isinstance(raw_args, dict):
                raw_args = {}
            try:
                args_json = _json.dumps(raw_args, default=str)
            except (TypeError, ValueError):
                args_json = "{}"
            if len(args_json) > 1500:
                args_json = args_json[:1500] + "…"
                # Re-emit as a string blob — the JSON would no longer
                # parse if we stuffed it back as a dict, and the trace
                # consumer (UI run drawer) treats `args` as opaque.
                args_payload: object = {"_truncated": args_json}
            else:
                args_payload = raw_args
            sanitized.append({
                "tool": tool_name,
                "args": args_payload,
                "result": result_text,
            })
        self.tool_trace = _json.dumps(sanitized)

    def to_dict(self, include_trace: bool = False) -> dict:
        d = {
            "id": self.id,
            "triggered_at": self.triggered_at.isoformat() if self.triggered_at else None,
            "trigger_type": self.trigger_type,
            "camera_id": self.camera_id,
            "tool_call_count": self.tool_call_count,
            "outcome": self.outcome,
            "severity": self.severity,
            "incident_id": self.incident_id,
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "manual_prompt": self.manual_prompt,
            "summary": self.summary or "",
        }
        if include_trace:
            d["tool_trace"] = self.get_tool_trace()
        return d

    @property
    def is_terminal(self) -> bool:
        """True when the run has reached a final outcome."""
        return self.outcome in ("incident", "no_action", "error")


