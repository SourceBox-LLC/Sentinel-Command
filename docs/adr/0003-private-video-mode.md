# ADR 0003: Private video, an opt-in end-to-end encrypted mode per camera

- **Status:** Proposed. Design accepted; the build is scheduled after launch.
- **Date:** 2026-10-19
- **Deciders:** Command Center maintainers + product

## Context

Today the service protects video in transit and at rest:

- TLS 1.3 from browsers and CameraNodes to `app.sentinel-command.com`. Postgres accepts TLS verified against our own CA, and each service switches over when its `DATABASE_URL` changes (see `DISASTER_RECOVERY.md`).
- Encrypted Fly volumes for the database and the app, and an AES-256-encrypted off-site backup.

But **Command Center handles plaintext video.**

- Live segments sit unencrypted in the RAM cache (`hls.rs`).
- Incident snapshots and clips are plain bytes in `incident_evidence.data`.
- Sentinel AI sends frames to Ollama Cloud.
- The MCP tools hand frames to whichever client holds the key.

So anyone with access to the servers or the database can watch a customer's cameras: us, Fly, an attacker who gets in, or a court order served on us. That is normal for a cloud camera with server-side AI. It is also why our marketing must never say "end-to-end encrypted".

Some customers will want a camera that nobody but them can watch: a nursery, a bedroom, a home office. The obvious fix, end-to-end encryption for everything, would switch off Sentinel AI, the feature most customers buy the product for. The AI has to see frames to describe them.

Ring and Apple face the same trade-off and give the same answer:

- **Ring's** end-to-end encryption is an opt-in setting that turns off the features that need server access to video.
- **Apple's** Advanced Data Protection is opt-in and requires a recovery method first.

## Decision

1. **Private video is an opt-in mode, chosen per camera.** An organization turns the capability on once, which creates its keys. Then each camera is private or not, and an organization can choose that new cameras start private. A private nursery camera and an AI-watched driveway camera can share one account.
2. **The CameraNode encrypts and the browser decrypts.** Command Center stores, caches, relays and meters only ciphertext for private cameras, and never holds a key that opens it.
3. **The node enforces the mode, not the server.**
   - Turning a camera private can come from anywhere.
   - Turning it back needs a message authenticated by a key holder, or the node's own local dashboard.
   - A private camera never uploads plaintext, whatever the server asks.
4. **Features that need plaintext are switched off for private cameras.**
   - Command Center refuses them in the API, the MCP tools and Sentinel dispatch.
   - The node refuses them when asked for frames.
   - Hiding buttons in the dashboard is not enforcement.
5. **Capturing evidence still works; only viewing needs a key.** Snapshots and clips of a private camera are stored sealed. An approved browser can open them; the server, the AI and MCP clients cannot.
6. **It is free on every plan**, including Free and self-hosted. Selling privacy as an upgrade reads badly, and private cameras cost us less, since they generate no AI runs.
7. **A recovery code is mandatory** before the first camera goes private. Lost keys mean lost footage, and the UI says so before the switch, not after.

## What it protects against, and what it doesn't

**Protects against:**

- A leak of the database, a backup or a snapshot: private cameras' evidence in them is ciphertext.
- Anyone with access to Command Center's machines or the Fly account, including SourceBox staff, watching live or stored video of a private camera.
- A demand to the operator for stored footage: we can only produce ciphertext.
- A compromised Command Center switching a camera back to plaintext: the node refuses unless a key holder authorizes it.
- A server tampering with or splicing video: every sealed object is authenticated and bound to its camera, name and capture time.
- A server replaying old video as live: the browser shows the authenticated capture time and flags stale video.

**Does not protect against:**

- **Metadata.** The server still sees:
  - camera names, and when cameras are online;
  - when motion happens (the node reports motion events);
  - segment sizes and timing, which rise with activity;
  - who watches, when and from where (`stream_access_logs`).
- **A malicious server serving modified dashboard JavaScript.** Such code could take keys from a browser. This is the standing limit of encryption delivered by a web app. Native apps and published build hashes narrow it later; we say so in the docs.
- **A compromised CameraNode or approved device.** These hold keys by design.
- **The LAN.** The node's local dashboard and local HLS (used by Home Assistant) stay plaintext on the customer's network, behind the node's local password and stream tokens.
- **Footage from before a camera went private.** It stays as it was, unless the owner chooses to seal existing evidence (see [Switching modes](#switching-modes)).

## Keys

All key handling happens in browsers (WebCrypto, keys stored in IndexedDB) and in CameraNode (keys stored in its encrypted `node.db`). Command Center stores only wrapped or sealed key material it cannot open.

| Key | What it is | Who holds it |
| --- | --- | --- |
| Org video key `OVK_e` | 32 random bytes, created by the browser of the admin who turns the capability on. `e` is the epoch, starting at 1. | Approved devices; wrapped by the recovery code |
| Node key `NK_e(node)` | `HKDF-SHA256(OVK_e, info = "sentinel/pv1/node/" ‖ node_id)` | That node, and anyone holding `OVK_e` |
| Content key, per camera per UTC day | `HKDF-SHA256(NK_e, info = "sentinel/pv1/content/" ‖ camera_id ‖ "/" ‖ yyyy-mm-dd)` | Derived on demand |
| Control key, per camera | `HKDF-SHA256(NK_e, info = "sentinel/pv1/control/" ‖ camera_id)` | Derived on demand; authorizes "turn private off" |
| Device key pair | ECDH P-256, private key non-extractable, one per approved browser | That browser |
| Recovery code | 128 random bits shown once as 26 base32 characters (with a QR code) | The customer, offline |

Why it is shaped this way:

- **Nodes get node keys, not the org key.** A stolen node exposes its own cameras, not the whole organization. A node derives keys for a camera added later on itself, so adding a camera needs no browser.
- **Content keys change daily.** Random 96-bit GCM nonces stay far inside their safety margin even at one segment a second for years. The day comes from the authenticated capture time, so any object says which key opens it.
- **P-256, not X25519**, because every browser's WebCrypto supports it.
- **No passphrases.** A human-chosen passphrase protects ciphertext the server holds, which invites an offline guessing attack. The recovery code is random, so a fast KDF (HKDF) is enough.

### Getting keys onto a node (pairing)

This mirrors today's setup, where the node's API key is copied from the dashboard into the node:

1. In the dashboard, a key holder chooses **Pair for private video** on a node. The browser creates a one-time 128-bit pairing secret `P` and shows it as a code and QR. `P` is never sent to the server.
2. The browser uploads `seal(HKDF(P, "sentinel/pv1/pair"), NK_e ‖ e ‖ node_id)`, which expires after 15 minutes.
3. The customer enters the code on the node: in its local dashboard, or with `sourcebox-sentry-cameranode pair` in a terminal.
4. The node fetches the sealed blob with its node API key, opens it, and stores `NK_e` in `node.db`.
5. The node creates its own P-256 key pair and uploads the public key with a MAC under `HKDF(NK_e, "sentinel/pv1/node-pub")`. Key holders can check that MAC, and later wrap new epochs' node keys to it (Phase 2).

### Approving another browser or member (Phase 2)

1. A new browser makes its device key pair and registers the public key as pending.
2. An admin who already holds the key is told about it, compares a short fingerprint shown on both screens, and approves.
3. Approving wraps `OVK_e` to the new public key.

The fingerprint check stops a malicious server from slipping in its own key as the "new device". Removing a device or member starts a new epoch:

- `OVK_e+1` is wrapped to the remaining devices and the recovery code. If a departing admin saw the recovery code, a new code is issued instead.
- Each node's new node key is wrapped to the node's public key.
- Old epochs' keys are kept, wrapped under the new one, so old evidence stays readable.

Phase 2 also settles the **key directory**: the list of approved devices and paired nodes, encrypted under `OVK`, versioned, and signed by device keys. Its job is to stop the server from altering or rolling back that list.

## Sealed objects

Every private segment, snapshot and clip piece uses one format, `SV1`:

```text
magic        "SV1\0"            4 bytes
epoch        u32, big-endian    4 bytes
captured_at  u64 ms, big-endian 8 bytes   capture time of the first frame (node clock)
nonce        random             12 bytes
ciphertext ‖ tag                AES-256-GCM, 16-byte tag

AAD = magic ‖ epoch ‖ captured_at ‖ kind ‖ camera_id ‖ name
kind: 1 = HLS segment, 2 = snapshot JPEG
name: the segment filename, or the snapshot id
```

The overhead is 44 bytes per object.

- **The AAD binds each object** to its camera and its name. A server cannot move one camera's video into another camera, or reorder segments, without failing authentication.
- **The capture time is authenticated**, so the player can show it and flag video that is not live.

**Why not HLS's built-in AES-128 (`#EXT-X-KEY`)?** Three reasons:

- It is CBC with no integrity, so the server could alter video undetected.
- Its key-URI scheme assumes the server hands out the key.
- Command Center's playlist rewrite would have to carry key lines.

Decrypting in a custom HLS.js fragment loader avoids all three. `HlsPlayer.jsx` already uses HLS.js everywhere, with no native-HLS fallback, so private cameras play wherever cameras play today. Playlists stay plaintext: they hold only names and durations, and the server needs them.

## What changes where

### CameraNode

- Pairing: the code entry in the local dashboard and a `pair` command, `NK` in `node.db`, and the node key pair.
- The private flag is stored on the node and is authoritative. The node reports it in register and heartbeat.
- The uploader seals each segment before `push-segment`. The filename is unchanged, so the cache and playlist logic stay the same.
- `take_snapshot` always returns a sealed JPEG for a private camera. There is no plaintext path, whatever the request says, and `at_ms` replay keeps working.
- Turning private off needs a command MAC'd with the control key over `(camera_id, "off", nonce)`, where the nonce is a fresh challenge from the node. The alternative is the local dashboard.
- **If keys are missing** (a wiped `node.db`, a new machine), a private camera stops uploading and shows "needs pairing" on the node and in the dashboard. It never falls back to plaintext.
- Unchanged: motion detection (on the node already), local recordings, local HLS.

### Command Center

- Schema:
  - `cameras.private_video` and `private_since`, mirrored from the node;
  - a marker on sealed evidence (`incident_evidence.sealed_epoch`);
  - tables for wrapped keys, devices and pairing blobs. These hold ciphertext and public keys only.
- `push-segment` refuses a segment without the `SV1` header for a camera marked private. That's defense in depth against a buggy node.
- Feature gating for private cameras, in one predicate used everywhere, as `sentinel_blocked_by_license` is today:
  - Sentinel dispatch;
  - the manual run;
  - the MCP tools in the table below;
  - the integration snapshot route.
- Routes to store and fetch wrapped key material, device registration and approval, and pairing blobs.
- New notifications, admin audience: a device waiting for approval, and a camera that needs pairing.
- The Privacy Policy, Sub-processors list and docs say exactly what private cameras do and don't hide.

### Dashboard

- A key manager:
  - creates the org key;
  - shows the recovery code once and makes the user confirm they kept it;
  - manages devices;
  - handles node pairing.
- An HLS.js fragment loader that opens `SV1` segments, and a stale-video badge driven by the authenticated capture time.
- Sealed snapshots and evidence opened in the browser, including clip playback.
- An "End-to-end encrypted" badge on private cameras, and a plain list of what is off for them, next to the toggle and on the Sentinel AI settings page.

## Per-camera feature table

| Keeps working on a private camera | Off for a private camera |
| --- | --- |
| Live view, in an approved browser | Sentinel AI runs, scheduled or manual. Motion on a private camera dispatches nothing. |
| Motion detection, motion alerts and digests | MCP `view_camera`, `watch_camera` and `get_stream_url` |
| Online/offline alerts, recording schedules, local recordings | MCP `get_incident_snapshot` and `get_incident_clip` for sealed evidence |
| Dashboard snapshots (sealed, opened in the browser) | Home Assistant's cloud snapshot (`/api/integration/cameras/{id}/snapshot`) |
| Incidents, and attaching snapshots and clips, MCP `attach_*` included: stored sealed | Opening anything from a browser that isn't approved |
| Home Assistant's LAN stream (`local_url` comes straight from the node) | |
| Viewer-hour metering, plan limits, audit and access logs | |

The MCP tools that refuse answer with a tool error that says why ("this camera is end-to-end encrypted; only approved browsers can view it"), never with an empty result.

## Switching modes

- **Private on:** only new data is sealed. Evidence already stored stays as it is. The dashboard offers **Seal existing evidence**: the browser downloads it, seals it, uploads the sealed copy, and the server deletes the plaintext.
- **Private off:** needs a key holder or the node's local dashboard (see above). New data is plaintext again. Sealed evidence stays sealed and readable by key holders.
- **Turning the capability off for the organization** first requires every camera to be non-private. The keys are then deleted from devices and the server. Sealed evidence becomes unreadable, and the UI says so first.

## Phases

1. **Phase 1: the core, one key holder.**
   - The shared format and key derivation, with test vectors used by both the Rust node and the JavaScript dashboard.
   - The org key and recovery code.
   - Node pairing.
   - Sealing on the node, opening in the dashboard.
   - Feature gating.
   - Sealed evidence.
   - One admin browser plus the recovery code; a second browser recovers with the code.
   - The largest piece of work, across all three codebases.
2. **Phase 2: teams.** Device approval with fingerprints, member access, revocation with new epochs, node keys wrapped to node public keys, and the signed key directory.
3. **Phase 3: AI for private cameras without breaking the promise.** An agent that runs on the customer's own hardware, beside the node with a local model, so frames never leave their network. This gets its own ADR. In the long run, cloud inference inside attested secure hardware is the other path.

## How we'll know it works

- **Test vectors** shared by the node and the dashboard, run in both test suites.
- **A rig test** (the virtual camera rig) that runs a private camera for a while, then scans Command Center for plaintext:
  - the cache, `incident_evidence` and every log;
  - the check: no MPEG-TS sync pattern (`0x47` every 188 bytes) and no JPEG header (`FF D8 FF`) in anything stored for that camera.
- **Hostile-server tests:**
  - an unauthenticated "private off" is refused by the node;
  - a plaintext segment for a private camera is refused by Command Center;
  - a segment moved between cameras, or replayed, fails to open or shows as stale;
  - `take_snapshot` on a private camera never returns a plain JPEG.
- **Recovery drill:** clear a browser's storage and recover with the code. Then lose the code and confirm the UI's warning was true.

## Alternatives considered

- **End-to-end encryption by default for every camera.** Rejected: it switches off Sentinel AI for everyone, and the AI is why most customers are here.
- **One switch for the whole organization.** Rejected: it forces a choice between privacy and the AI that per-camera control avoids.
- **HLS AES-128 with `#EXT-X-KEY`.** Rejected: no integrity, and server-delivered keys (see [Sealed objects](#sealed-objects)).
- **Passphrase-derived keys.** Rejected: low entropy, against ciphertext the server holds.
- **Per-organization keys held by the server, in a KMS.** Not end-to-end: the operator can still decrypt. It may still be worth doing for at-rest defense of non-private cameras, as a separate decision.
- **Letting the server turn private mode off.** Rejected: a server that can switch encryption off can defeat it.

## Open questions

- **Motion events from private cameras** reveal when something happens. Should there be a per-camera choice to keep motion local? That would mean no cloud alerts. Default: motion still goes to the cloud, documented.
- **Recovery code format:** base32 groups or a word list.
- **Who may approve devices:** admins only (proposed), or members for their own second device.
- **The stale-video threshold** for the player's badge, given node clock drift.
- **Self-hosted, local auth:** one admin, so Phase 2's member flows don't apply. Device approval still does.
