"""
Tests for the server-side codec sanitizer.

Specifically covers the h264_v4l2m2m malformed-SPS bug — when a CameraNode
reports `avc1.42e00a` (H.264 level 1.0, max 176×144) for a real webcam,
the browser MSE decoder rejects the first 720p/1080p NALU and the stream
shows spinner-forever.  The sanitizer catches these server-side regardless
of which CameraNode version is in the field.
"""

from app.core.codec import sanitize_video_codec


class TestSanitizeVideoCodec:
    def test_upgrades_avc1_42e00a_to_level_3(self):
        """The exact Pi v0.1.4 bug — level 1.0 upgraded to level 3.0."""
        assert sanitize_video_codec("avc1.42e00a") == "avc1.42e01e"

    def test_upgrades_main_profile_level_1_to_level_3(self):
        assert sanitize_video_codec("avc1.4da00a") == "avc1.4da01e"

    def test_upgrades_level_11_to_level_3(self):
        """Level 1.1 (hex 0b) — still absurd for real cameras."""
        assert sanitize_video_codec("avc1.42e00b") == "avc1.42e01e"

    def test_upgrades_level_13_to_level_3(self):
        """Level 1.3 (hex 0d) — still below the threshold."""
        assert sanitize_video_codec("avc1.42e00d") == "avc1.42e01e"

    def test_preserves_level_2_0(self):
        """Level 2.0 (hex 14) — boundary, allowed through."""
        assert sanitize_video_codec("avc1.42e014") == "avc1.42e014"

    def test_preserves_level_3_0(self):
        assert sanitize_video_codec("avc1.42e01e") == "avc1.42e01e"

    def test_preserves_level_3_1(self):
        assert sanitize_video_codec("avc1.42e01f") == "avc1.42e01f"

    def test_preserves_level_4_0(self):
        assert sanitize_video_codec("avc1.42e028") == "avc1.42e028"

    def test_preserves_level_4_1(self):
        assert sanitize_video_codec("avc1.4da029") == "avc1.4da029"

    def test_preserves_level_5_1(self):
        assert sanitize_video_codec("avc1.64a033") == "avc1.64a033"

    def test_non_h264_passes_through(self):
        """AAC audio codec — not our problem."""
        assert sanitize_video_codec("mp4a.40.2") == "mp4a.40.2"

    def test_hevc_passes_through(self):
        assert sanitize_video_codec("hvc1.1.L123.B0") == "hvc1.1.L123.B0"

    def test_vp9_passes_through(self):
        assert sanitize_video_codec("vp9") == "vp9"

    def test_empty_string_passes_through(self):
        assert sanitize_video_codec("") == ""

    def test_none_passes_through(self):
        # type: ignore
        assert sanitize_video_codec(None) is None

    def test_malformed_hex_passes_through(self):
        """Unparseable hex — can't safely rewrite, leave it alone."""
        assert sanitize_video_codec("avc1.42e0zz") == "avc1.42e0zz"

    def test_wrong_length_passes_through(self):
        """Too short to contain a level — nothing we can do."""
        assert sanitize_video_codec("avc1.42e") == "avc1.42e"
