from typing import Literal, Optional

from pydantic import BaseModel, Field, field_validator


class CameraGroupCreate(BaseModel):
    name: str = Field(..., max_length=100)
    color: Optional[str] = Field("#22c55e", max_length=20)
    icon: Optional[str] = Field("📁", max_length=10)


class CameraRecordingPolicy(BaseModel):
    """Per-camera recording policy — set via
    ``PATCH /api/cameras/{camera_id}/recording-settings``.

    Replaces the org-wide ``RecordingSettings`` (v0.1.42 and earlier),
    which persisted but never actually drove any recording.  Per-camera
    is the granularity that matches how recording state is keyed at
    runtime in CameraNode (``recording_state: HashSet<camera_id>``) and
    lets operators record some cameras 24/7 while leaving others off
    for privacy / storage reasons.

    All fields optional so a PATCH can flip just one toggle without
    re-asserting the others.
    """

    continuous_24_7: Optional[bool] = None
    scheduled_recording: Optional[bool] = None
    # "HH:MM" 24-hour.  Nullable so the operator can clear the window.
    scheduled_start: Optional[str] = Field(None, max_length=5)
    scheduled_end: Optional[str] = Field(None, max_length=5)

    @field_validator("scheduled_start", "scheduled_end")
    @classmethod
    def _validate_hhmm(cls, v):
        if v is None or v == "":
            return v
        # Strict HH:MM 24-hour validation.  Anything else is a bug —
        # we'd rather 422 the PATCH than store garbage that the
        # heartbeat handler then has to defend against.
        import re
        if not re.match(r"^([01]\d|2[0-3]):[0-5]\d$", v):
            raise ValueError("must be HH:MM 24-hour, e.g. 08:30")
        return v


class NotificationSettings(BaseModel):
    """Per-org toggles controlling which kinds of notifications show up in
    the bell inbox.  Camera/node transitions stay on by default because
    those are operator-critical; motion can get noisy so we let operators
    silence it per-org without disabling the motion-event pipeline itself
    (motion still records to the DB for incidents and analytics).
    """

    motion_notifications: bool = True
    camera_transition_notifications: bool = True
    node_transition_notifications: bool = True


class CameraReport(BaseModel):
    camera_id: Optional[str] = Field(None, max_length=150)
    device_path: Optional[str] = Field(None, max_length=255)
    name: Optional[str] = Field(None, max_length=100)
    node_type: Optional[str] = Field("usb", max_length=20)
    capabilities: Optional[list[str]] = []
    width: Optional[int] = Field(1280, ge=1, le=7680)
    height: Optional[int] = Field(720, ge=1, le=4320)


class NodeRegister(BaseModel):
    node_id: str = Field(..., max_length=50)
    name: Optional[str] = Field(None, max_length=100)
    hostname: Optional[str] = Field(None, max_length=255)
    local_ip: Optional[str] = Field(None, max_length=45)
    http_port: Optional[int] = Field(8080, ge=1, le=65535)
    # Whether the node's local HLS server is reachable on the LAN
    # (bind != loopback).  False → CC clears local_ip so the Home
    # Assistant integration never gets a connection-refused stream
    # URL.  None → old CameraNode that predates the field.
    lan_streaming: Optional[bool] = None
    cameras: Optional[list[CameraReport]] = []
    video_codec: Optional[str] = Field(None, max_length=50)
    audio_codec: Optional[str] = Field(None, max_length=50)
    # CameraNode build version ("X.Y.Z" from its Cargo.toml).  Optional so
    # very old nodes that pre-date version reporting can still register;
    # they'll show up as "unknown" in the dashboard and get an
    # update_available hint pointing at the latest release.
    node_version: Optional[str] = Field(None, max_length=50)


class CameraStatus(BaseModel):
    camera_id: str = Field(..., max_length=150)
    status: str = Field(..., max_length=20)
    # Optional failure reason — sent by the CameraNode supervisor when
    # the pipeline is restarting, failed, or errored. Old nodes that
    # predate the supervisor simply omit it (the field is Optional).
    last_error: Optional[str] = Field(None, max_length=500)


class NodeStorageStats(BaseModel):
    """Filesystem-aware storage snapshot from CameraNode.

    Reported on every heartbeat by v0.1.41+ nodes; the dashboard
    renders the per-node usage bar from these numbers and warns the
    operator when ``disk_free_bytes`` drops below the safety floor.
    Optional throughout because (1) older CameraNodes don't send the
    block at all, and (2) a single field can be unknown
    (``disk_free_bytes = 0`` means "couldn't identify the disk" in
    Docker etc.) without invalidating the rest.
    """

    used_bytes: Optional[int] = Field(None, ge=0)
    max_bytes: Optional[int] = Field(None, ge=0)
    disk_free_bytes: Optional[int] = Field(None, ge=0)
    disk_total_bytes: Optional[int] = Field(None, ge=0)


class NodeHeartbeat(BaseModel):
    node_id: str = Field(..., max_length=50)
    local_ip: Optional[str] = Field(None, max_length=45)
    # See NodeRegister.lan_streaming — re-sent every heartbeat so a
    # bind change (re-enrol with/without --lan-streaming) propagates
    # without a re-register.
    lan_streaming: Optional[bool] = None
    cameras: Optional[list[CameraStatus]] = []
    # See NodeRegister.node_version — same field, re-sent on every heartbeat
    # so the backend always knows what's actually running (e.g. after the
    # operator updates CameraNode without re-registering).
    node_version: Optional[str] = Field(None, max_length=50)
    # Filesystem-aware storage snapshot (v0.1.41+).  Optional so
    # heartbeats from older CameraNodes that don't send the block
    # still pass validation.
    storage_stats: Optional[NodeStorageStats] = None


class NodeCreate(BaseModel):
    name: Optional[str] = Field(None, max_length=100)


class McpKeyCreate(BaseModel):
    """Create an MCP API key with per-key tool scoping.

    scope_mode:
      - "all"      — key may invoke every MCP tool (default)
      - "readonly" — key is limited to read-only tools
      - "custom"   — key is limited to the explicit list in ``scope_tools``
                     (unknown tool names are rejected by the endpoint)
    """
    name: str = Field("Default", max_length=100)
    scope_mode: Literal["all", "readonly", "custom"] = "all"
    scope_tools: Optional[list[str]] = None

    @field_validator("scope_tools")
    @classmethod
    def _normalize_scope_tools(cls, v):
        if v is None:
            return v
        # Deduplicate + drop empty strings without reordering user intent.
        seen = set()
        out: list[str] = []
        for name in v:
            name = (name or "").strip()
            if not name or name in seen:
                continue
            seen.add(name)
            out.append(name)
        return out


class IntegrationKeyCreate(BaseModel):
    """Create a REST integration API key (osi_) — e.g. for Home Assistant.

    Unlike MCP keys, integration keys have no per-tool scoping: they grant
    access to the whole /api/integration/* surface (camera list, snapshots,
    recording control, motion). Org isolation is enforced per-request from
    the key's org_id, same as every other authenticated endpoint.
    """
    name: str = Field("Home Assistant", max_length=100)
