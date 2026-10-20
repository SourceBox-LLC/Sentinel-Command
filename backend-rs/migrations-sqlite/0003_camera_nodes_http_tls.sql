-- Whether a CameraNode's local server speaks HTTPS (CameraNode 0.1.89+,
-- reported at registration), so Home Assistant's LAN stream URL is built
-- as https://. NULL is a node that did not say: plain HTTP.
ALTER TABLE camera_nodes ADD COLUMN http_tls BOOLEAN;
