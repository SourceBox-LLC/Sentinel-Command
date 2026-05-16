import { useDocs } from "./context"


function Deployment() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="deployment">
      <h2>Deployment<a href="#deployment" className="docs-anchor">#</a></h2>
      <p>
        Three ways to run CloudNode in production. Pick the one that matches your
        existing ops setup.
      </p>

      <h3>Docker (single camera)</h3>
      <p>The most portable option. Maps one USB camera device into the container:</p>
      <div className="docs-code-block">
        <code>{`docker build -t sourcebox-sentry-cloudnode .

docker run -d \\
  --name sourcebox-sentry-cloudnode \\
  --device /dev/video0:/dev/video0 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://opensentry-command.fly.dev \\
  -p 8080:8080 \\
  -v ./data:/app/data \\
  sourcebox-sentry-cloudnode`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`docker run -d \\
  --name sourcebox-sentry-cloudnode \\
  --device /dev/video0:/dev/video0 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://opensentry-command.fly.dev \\
  -p 8080:8080 \\
  -v ./data:/app/data \\
  sourcebox-sentry-cloudnode`)}>Copy</button>
      </div>

      <h3>Docker (multiple cameras)</h3>
      <p>Pass each <code>/dev/video*</code> device explicitly:</p>
      <div className="docs-code-block">
        <code>{`docker run -d \\
  --device /dev/video0:/dev/video0 \\
  --device /dev/video2:/dev/video2 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://opensentry-command.fly.dev \\
  -p 8080:8080 \\
  sourcebox-sentry-cloudnode`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`docker run -d \\
  --device /dev/video0:/dev/video0 \\
  --device /dev/video2:/dev/video2 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://opensentry-command.fly.dev \\
  -p 8080:8080 \\
  sourcebox-sentry-cloudnode`)}>Copy</button>
      </div>

      <h3>Docker Compose</h3>
      <p>For declarative config, use the included <code>docker-compose.yml</code>:</p>
      <div className="docs-code-block">
        <code>{`cp .env.example .env
# Edit .env with your credentials
docker-compose up -d`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`cp .env.example .env
# Edit .env with your credentials
docker-compose up -d`)}>Copy</button>
      </div>

      <h3>Build from source</h3>
      <p>If you prefer native install — Rust 1.70+ and FFmpeg must already be on the box:</p>
      <div className="docs-code-block">
        <code>{`git clone https://github.com/SourceBox-LLC/opensentry-cloud-node.git
cd opensentry-cloud-node
cargo build --release
./target/release/sourcebox-sentry-cloudnode setup`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`git clone https://github.com/SourceBox-LLC/opensentry-cloud-node.git
cd opensentry-cloud-node
cargo build --release
./target/release/sourcebox-sentry-cloudnode setup`)}>Copy</button>
      </div>

      <h3>systemd service (Linux)</h3>
      <p>
        The easiest path is the install script — after the wizard finishes, it offers to write
        the systemd unit and enable it for you. If you'd rather wire it up manually, the unit
        below mirrors what the script would have written. It assumes the binary lives at{' '}
        <code>~/.sourcebox-sentry/sourcebox-sentry-cloudnode</code> (the install script's
        default <code>INSTALL_DIR</code>) and the wizard wrote <code>node.db</code> under{' '}
        <code>$HOME</code>; substitute your own paths if you installed elsewhere.
      </p>
      <p>Drop into <code>/etc/systemd/system/sourcebox-sentry-cloudnode.service</code>:</p>
      <div className="docs-code-block">
        <code>{`[Unit]
Description=Sentinel CloudNode
Documentation=https://opensentry-command.fly.dev
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=YOUR_USER
# 'video' is the standard group that owns /dev/video* on Debian / Ubuntu /
# Raspberry Pi OS — needed to open USB cameras.
SupplementaryGroups=video
# Inherit a sane PATH so ffmpeg (system-installed) is found even when
# systemd's default PATH skips /usr/local/bin.
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Suppress the TUI's ANSI cursor escapes so journalctl entries stay
# line-oriented instead of full-screen redraws.
Environment=NO_COLOR=1
Environment=TERM=dumb
Environment=RUST_LOG=info
WorkingDirectory=/home/YOUR_USER
ExecStart=/home/YOUR_USER/.sourcebox-sentry/sourcebox-sentry-cloudnode run
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5s
# Stop retrying after 5 failures in a minute so logs stay readable.
StartLimitIntervalSec=60
StartLimitBurst=5

[Install]
WantedBy=multi-user.target`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`[Unit]
Description=Sentinel CloudNode
Documentation=https://opensentry-command.fly.dev
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=YOUR_USER
SupplementaryGroups=video
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=NO_COLOR=1
Environment=TERM=dumb
Environment=RUST_LOG=info
WorkingDirectory=/home/YOUR_USER
ExecStart=/home/YOUR_USER/.sourcebox-sentry/sourcebox-sentry-cloudnode run
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5s
StartLimitIntervalSec=60
StartLimitBurst=5

[Install]
WantedBy=multi-user.target`)}>Copy</button>
      </div>
      <p>Enable and start:</p>
      <div className="docs-code-block">
        <code>sudo systemctl enable --now sourcebox-sentry-cloudnode</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard('sudo systemctl enable --now sourcebox-sentry-cloudnode')}>Copy</button>
      </div>

      <h3>Cross-compilation (Raspberry Pi)</h3>
      <p>CloudNode runs on ARM64 Linux — build on a dev machine, copy the binary:</p>
      <div className="docs-code-block">
        <code>{`rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu`)}>Copy</button>
      </div>

      <h3>Updating</h3>
      <p>
        Re-run the install script. It downloads the latest release, preserves your
        <code>data/node.db</code>, and restarts the binary. With Docker, pull the new image
        and recreate the container. With systemd, replace the binary and run
        <code>sudo systemctl restart sourcebox-sentry-cloudnode</code>.
      </p>
    </section>
  )
}

export default Deployment
