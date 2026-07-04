"""
Server-side defenses against malformed codec strings from CameraNodes.

Older CameraNode builds (v0.1.5 and earlier) shipped garbage H.264 codec
strings when the Raspberry Pi's `h264_v4l2m2m` encoder wrote `level_idc=0`
into the SPS.  FFprobe then reported `level=0`, which our `to_hls_codec_string`
rounded to the nearest valid level (10 — i.e. H.264 level 1.0, max 176×144)
for segments where ffprobe couldn't parse dimensions.  Browsers rejected
the MSE attach when the first real 720p/1080p NALU arrived because the
declared codec said "I can only handle QCIF."

The CameraNode-side fix in v0.1.6 avoids producing these strings.  This
module catches the same pattern *on ingest* so a stale binary running in
the field can't silently brick streaming again while we wait for the fleet
to update.
"""

import logging

logger = logging.getLogger(__name__)


def sanitize_video_codec(codec: str) -> str:
    """
    Upgrade a suspicious H.264 codec string to a safe level.

    Real webcams don't shoot at QCIF (176×144).  Any H.264 level below
    2.0 — hex `14` and below — in the field is almost certainly the
    malformed-SPS bug.  Upgrade to level 3.0 (`*e01e`, max 720×576)
    which safely covers standard webcam output.  Higher levels and
    non-H.264 codecs pass through unchanged.
    """
    if not codec or not codec.startswith("avc1.") or len(codec) != 11:
        return codec

    level_hex = codec[-2:].lower()
    try:
        level_val = int(level_hex, 16)
    except ValueError:
        return codec

    if level_val < 0x14:
        upgraded = codec[:-2] + "1e"  # level 3.0
        logger.warning(
            "Upgrading suspicious H.264 codec %s → %s (level < 2.0 almost"
            " certainly means malformed SPS from a hardware encoder)",
            codec,
            upgraded,
        )
        return upgraded

    return codec
