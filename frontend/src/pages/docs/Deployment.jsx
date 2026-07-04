import { useDocs } from "./context"


function Deployment() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="deployment">
      <h2>Deployment<a href="#deployment" className="docs-anchor">#</a></h2>
      <p>
        Three ways to run CameraNode in production. Pick the one that matches your
        existing ops setup.
      </p>

      <h3>Docker (single camera)</h3>
      <p>The most portable option. Maps one USB camera device into the container:</p>
      <div className="docs-code-block">
        <code>{`docker build -t sourcebox-sentry-cameranode .

docker run -d \\
  --name sourcebox-sentry-cameranode \\
  --device /dev/video0:/dev/video0 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://sentinel-command.com \\
  -p 8080:8080 \\
  -v ./data:/app/data \\
  sourcebox-sentry-cameranode`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`docker run -d \\
  --name sourcebox-sentry-cameranode \\
  --device /dev/video0:/dev/video0 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://sentinel-command.com \\
  -p 8080:8080 \\
  -v ./data:/app/data \\
  sourcebox-sentry-cameranode`)}>Copy</button>
      </div>

      <h3>Docker (multiple cameras)</h3>
      <p>Pass each <code>/dev/video*</code> device explicitly:</p>
      <div className="docs-code-block">
        <code>{`docker run -d \\
  --device /dev/video0:/dev/video0 \\
  --device /dev/video2:/dev/video2 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://sentinel-command.com \\
  -p 8080:8080 \\
  sourcebox-sentry-cameranode`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`docker run -d \\
  --device /dev/video0:/dev/video0 \\
  --device /dev/video2:/dev/video2 \\
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \\
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \\
  -e SOURCEBOX_SENTRY_API_URL=https://sentinel-command.com \\
  -p 8080:8080 \\
  sourcebox-sentry-cameranode`)}>Copy</button>
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
        <code>{`git clone https://github.com/SourceBox-LLC/Sentinel-CameraNode.git
cd Sentinel-CameraNode
cargo build --release
./target/release/sourcebox-sentry-cameranode setup`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`git clone https://github.com/SourceBox-LLC/Sentinel-CameraNode.git
cd Sentinel-CameraNode
cargo build --release
./target/release/sourcebox-sentry-cameranode setup`)}>Copy</button>
      </div>

      <h3>Cross-compilation (Raspberry Pi)</h3>
      <p>CameraNode runs on ARM64 Linux — build on a dev machine, copy the binary:</p>
      <div className="docs-code-block">
        <code>{`rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu`)}>Copy</button>
      </div>

      <h3>Updating</h3>
      <p>
        Re-run the install script. It downloads the latest release, preserves your{' '}
        <code>data/node.db</code>, and restarts the binary. With Docker, pull the new image
        and recreate the container.
      </p>
    </section>
  )
}

export default Deployment
