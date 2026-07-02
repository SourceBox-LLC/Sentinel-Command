// DocsDiagrams.jsx — Inline SVG diagrams for the public documentation page.
//
// These replace plain CSS-styled flow boxes with proper illustrations so the
// architecture, motion pipeline, MCP agent flow, security layers, etc. are
// actually readable at a glance.
//
// Design rules:
//   • Dark theme only — pulls from the same CSS variables as the rest of the site
//     (--accent-green / blue / amber / purple / cyan, --bg-secondary, etc.).
//   • No external libraries. Everything is hand-written SVG so there's zero
//     bundle cost beyond the JSX itself.
//   • Every diagram is responsive: viewBox + preserveAspectRatio + width:100%
//     so they scale with the docs column without blowing up on a 4K display.
//   • Colors are expressed as literals inside SVG (CSS vars don't cascade into
//     `stop-color` on Safari) but they're the same hex values as the site vars.
//
// If you add a new diagram, also add its anchor in DocsPage.jsx and, if it
// needs bespoke styling beyond the <DiagramFrame> wrapper, extend
// styles/landing.css under the /* ── Docs diagrams ── */ block.

// ── Palette (mirrors index.css :root vars) ─────────────────────────
const C = {
  bg: '#12121a',
  bgCard: 'rgba(255, 255, 255, 0.03)',
  border: 'rgba(255, 255, 255, 0.10)',
  borderStrong: 'rgba(255, 255, 255, 0.18)',
  text: '#ffffff',
  textDim: '#a1a1aa',
  textMuted: '#71717a',
  green: '#22c55e',
  greenDark: '#16a34a',
  amber: '#f59e0b',
  amberDark: '#d97706',
  red: '#ef4444',
  redDark: '#dc2626',
  blue: '#3b82f6',
  blueDark: '#1d4ed8',
  purple: '#a855f7',
  purpleDark: '#7e22ce',
  cyan: '#06b6d4',
  cyanDark: '#0891b2',
}

// ── Shared <defs>: gradients, glow filters, arrow markers ──────────
// Defined once per diagram. IDs are scoped with a prefix so multiple diagrams
// on the same page don't collide (SVG IDs are global to the document).
function Defs({ id }) {
  return (
    <defs>
      {/* Accent gradients — match --gradient-1..4 */}
      <linearGradient id={`${id}-grad-green`} x1="0%" y1="0%" x2="100%" y2="100%">
        <stop offset="0%" stopColor={C.green} />
        <stop offset="100%" stopColor={C.greenDark} />
      </linearGradient>
      <linearGradient id={`${id}-grad-blue`} x1="0%" y1="0%" x2="100%" y2="100%">
        <stop offset="0%" stopColor={C.blue} />
        <stop offset="100%" stopColor={C.blueDark} />
      </linearGradient>
      <linearGradient id={`${id}-grad-amber`} x1="0%" y1="0%" x2="100%" y2="100%">
        <stop offset="0%" stopColor={C.amber} />
        <stop offset="100%" stopColor={C.amberDark} />
      </linearGradient>
      <linearGradient id={`${id}-grad-purple`} x1="0%" y1="0%" x2="100%" y2="100%">
        <stop offset="0%" stopColor={C.purple} />
        <stop offset="100%" stopColor={C.purpleDark} />
      </linearGradient>
      <linearGradient id={`${id}-grad-cyan`} x1="0%" y1="0%" x2="100%" y2="100%">
        <stop offset="0%" stopColor={C.cyan} />
        <stop offset="100%" stopColor={C.cyanDark} />
      </linearGradient>
      <linearGradient id={`${id}-grad-card`} x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stopColor="rgba(255,255,255,0.05)" />
        <stop offset="100%" stopColor="rgba(255,255,255,0.02)" />
      </linearGradient>

      {/* Soft glow — used on accented nodes */}
      <filter id={`${id}-glow-green`} x="-50%" y="-50%" width="200%" height="200%">
        <feGaussianBlur stdDeviation="6" result="coloredBlur" />
        <feMerge>
          <feMergeNode in="coloredBlur" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>
      <filter id={`${id}-glow-blue`} x="-50%" y="-50%" width="200%" height="200%">
        <feGaussianBlur stdDeviation="6" result="coloredBlur" />
        <feMerge>
          <feMergeNode in="coloredBlur" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>

      {/* Arrow marker — used on all flow arrows */}
      <marker id={`${id}-arrow`} viewBox="0 0 10 10" refX="8" refY="5"
              markerWidth="8" markerHeight="8" orient="auto-start-reverse">
        <path d="M 0 0 L 10 5 L 0 10 z" fill={C.textMuted} />
      </marker>
      <marker id={`${id}-arrow-green`} viewBox="0 0 10 10" refX="8" refY="5"
              markerWidth="8" markerHeight="8" orient="auto-start-reverse">
        <path d="M 0 0 L 10 5 L 0 10 z" fill={C.green} />
      </marker>
      <marker id={`${id}-arrow-amber`} viewBox="0 0 10 10" refX="8" refY="5"
              markerWidth="8" markerHeight="8" orient="auto-start-reverse">
        <path d="M 0 0 L 10 5 L 0 10 z" fill={C.amber} />
      </marker>
    </defs>
  )
}

// ── Reusable primitive: rounded card node ──────────────────────────
// Draws a rounded-rect "node" at (x,y) with optional accent colour. Label
// renders as a bold title inside and an optional subtitle below.
function NodeBox({ x, y, w, h, title, subtitle, accent = 'default', icon = null, idPrefix }) {
  const accentMap = {
    default: { border: C.border, fill: C.bgCard, text: C.text, glow: null },
    green:   { border: C.green,  fill: `url(#${idPrefix}-grad-card)`, text: C.green,  glow: `url(#${idPrefix}-glow-green)` },
    blue:    { border: C.blue,   fill: `url(#${idPrefix}-grad-card)`, text: C.blue,   glow: `url(#${idPrefix}-glow-blue)` },
    amber:   { border: C.amber,  fill: `url(#${idPrefix}-grad-card)`, text: C.amber,  glow: null },
    purple:  { border: C.purple, fill: `url(#${idPrefix}-grad-card)`, text: C.purple, glow: null },
    cyan:    { border: C.cyan,   fill: `url(#${idPrefix}-grad-card)`, text: C.cyan,   glow: null },
  }
  const a = accentMap[accent] || accentMap.default
  const hasIcon = !!icon
  return (
    <g filter={a.glow || undefined}>
      <rect x={x} y={y} width={w} height={h} rx="10" ry="10"
            fill={a.fill} stroke={a.border} strokeWidth="1.5" />
      {hasIcon && (
        <g transform={`translate(${x + 14}, ${y + h / 2 - 10})`}>{icon(a.text)}</g>
      )}
      <text x={x + (hasIcon ? 44 : w / 2)} y={y + (subtitle ? h / 2 - 4 : h / 2 + 5)}
            textAnchor={hasIcon ? 'start' : 'middle'}
            fill={a.text} fontSize="13" fontWeight="600"
            fontFamily="Inter, system-ui, sans-serif">
        {title}
      </text>
      {subtitle && (
        <text x={x + (hasIcon ? 44 : w / 2)} y={y + h / 2 + 12}
              textAnchor={hasIcon ? 'start' : 'middle'}
              fill={C.textMuted} fontSize="10.5"
              fontFamily="Inter, system-ui, sans-serif">
          {subtitle}
        </text>
      )}
    </g>
  )
}

// ── Reusable primitive: flow arrow with optional label ─────────────
function FlowArrow({ x1, y1, x2, y2, label, color = C.textMuted, marker = 'arrow', idPrefix, dashed = false }) {
  const midX = (x1 + x2) / 2
  const midY = (y1 + y2) / 2
  return (
    <g>
      <line x1={x1} y1={y1} x2={x2} y2={y2}
            stroke={color} strokeWidth="1.5"
            strokeDasharray={dashed ? '4 4' : undefined}
            markerEnd={`url(#${idPrefix}-${marker})`} />
      {label && (
        <g>
          <rect x={midX - label.length * 3.2 - 6} y={midY - 10}
                width={label.length * 6.4 + 12} height="18" rx="4"
                fill={C.bg} stroke={C.border} strokeWidth="0.5" />
          <text x={midX} y={midY + 3} textAnchor="middle"
                fill={C.textDim} fontSize="10.5"
                fontFamily="Inter, system-ui, sans-serif">
            {label}
          </text>
        </g>
      )}
    </g>
  )
}

// ── DiagramFrame: responsive wrapper with optional caption ────────
function DiagramFrame({ children, caption, viewBox, aspectRatio = '16/9', ariaLabel }) {
  return (
    <figure className="docs-diagram">
      <svg
        role="img"
        aria-label={ariaLabel}
        viewBox={viewBox}
        preserveAspectRatio="xMidYMid meet"
        style={{ width: '100%', height: 'auto', aspectRatio, display: 'block' }}
      >
        {children}
      </svg>
      {caption && <figcaption className="docs-diagram-caption">{caption}</figcaption>}
    </figure>
  )
}

// ── Small icon paths (returns an SVG group, tinted) ────────────────
const Icon = {
  camera: (stroke) => (
    <g stroke={stroke} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <rect x="0" y="3" width="20" height="14" rx="2.5" />
      <circle cx="10" cy="10" r="4" />
      <circle cx="10" cy="10" r="1.2" fill={stroke} />
      <path d="M5 3 L6.5 1 H13.5 L15 3" />
    </g>
  ),
  node: (stroke) => (
    <g stroke={stroke} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <rect x="0" y="2" width="20" height="13" rx="2" />
      <line x1="0" y1="11" x2="20" y2="11" />
      <circle cx="4" cy="13.5" r="0.8" fill={stroke} />
      <circle cx="7" cy="13.5" r="0.8" fill={stroke} />
      <path d="M3 6 L7 6 M3 8.5 L5 8.5" />
    </g>
  ),
  cloud: (stroke) => (
    <g stroke={stroke} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <path d="M5 14 a4 4 0 0 1 0 -8 a5 5 0 0 1 9 -1 a3.5 3.5 0 0 1 3 6.5 a3 3 0 0 1 -2 2.5 H5 z" />
    </g>
  ),
  browser: (stroke) => (
    <g stroke={stroke} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <rect x="0" y="2" width="20" height="14" rx="2" />
      <line x1="0" y1="6" x2="20" y2="6" />
      <circle cx="2.5" cy="4" r="0.6" fill={stroke} />
      <circle cx="4.5" cy="4" r="0.6" fill={stroke} />
      <circle cx="6.5" cy="4" r="0.6" fill={stroke} />
      <polygon points="8,10 8,14 13,12" fill={stroke} />
    </g>
  ),
  lock: (stroke) => (
    <g stroke={stroke} strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <rect x="3" y="8" width="12" height="10" rx="1.5" />
      <path d="M5.5 8 V5 a3.5 3.5 0 0 1 7 0 V8" />
      <circle cx="9" cy="13" r="0.9" fill={stroke} />
    </g>
  ),
  cog: (stroke) => (
    <g stroke={stroke} strokeWidth="1.4" fill="none" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="10" cy="10" r="3" />
      <path d="M10 2 V4 M10 16 V18 M2 10 H4 M16 10 H18 M4.3 4.3 L5.7 5.7 M14.3 14.3 L15.7 15.7 M4.3 15.7 L5.7 14.3 M14.3 5.7 L15.7 4.3" />
    </g>
  ),
  eye: (stroke) => (
    <g stroke={stroke} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" fill="none">
      <path d="M1 10 C4 5 7 3 10 3 C13 3 16 5 19 10 C16 15 13 17 10 17 C7 17 4 15 1 10 z" />
      <circle cx="10" cy="10" r="2.8" />
    </g>
  ),
  bolt: (stroke) => (
    <g fill={stroke}>
      <polygon points="11,1 3,11 9,11 7,19 15,9 9,9" />
    </g>
  ),
  agent: (stroke) => (
    <g stroke={stroke} strokeWidth="1.5" fill="none" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="10" cy="6" r="3" />
      <path d="M4 17 a6 6 0 0 1 12 0" />
      <circle cx="7.5" cy="5.5" r="0.6" fill={stroke} />
      <circle cx="12.5" cy="5.5" r="0.6" fill={stroke} />
    </g>
  ),
}

// ══════════════════════════════════════════════════════════════════
//   1) SYSTEM ARCHITECTURE
// ══════════════════════════════════════════════════════════════════
// Three-layer architecture: local camera+CameraNode box → HTTPS outbound →
// Command Center cloud → browser viewer. Emphasises: no inbound ports,
// in-memory cache, same-origin streaming.
export function SystemArchitectureDiagram() {
  const id = 'arch'
  return (
    <DiagramFrame
      ariaLabel="System architecture: USB camera, CameraNode, Command Center cloud, browser viewer."
      viewBox="0 0 960 420"
      aspectRatio="960/420"
      caption="The live-video path runs entirely inside the authenticated backend — CameraNode pushes outbound, the browser fetches same-origin. No third-party object storage in the hot path."
    >
      <Defs id={id} />

      {/* Three-layer backdrop panels */}
      <g>
        <rect x="20"  y="40" width="280" height="340" rx="14"
              fill="rgba(59,130,246,0.04)" stroke="rgba(59,130,246,0.2)" strokeWidth="1" strokeDasharray="3 3" />
        <text x="160" y="66" textAnchor="middle" fill={C.blue} fontSize="12.5"
              fontWeight="600" letterSpacing="1.5" fontFamily="Inter, system-ui, sans-serif">
          LOCAL
        </text>

        <rect x="340" y="40" width="280" height="340" rx="14"
              fill="rgba(34,197,94,0.05)" stroke="rgba(34,197,94,0.25)" strokeWidth="1" strokeDasharray="3 3" />
        <text x="480" y="66" textAnchor="middle" fill={C.green} fontSize="12.5"
              fontWeight="600" letterSpacing="1.5" fontFamily="Inter, system-ui, sans-serif">
          CLOUD
        </text>

        <rect x="660" y="40" width="280" height="340" rx="14"
              fill="rgba(168,85,247,0.04)" stroke="rgba(168,85,247,0.2)" strokeWidth="1" strokeDasharray="3 3" />
        <text x="800" y="66" textAnchor="middle" fill={C.purple} fontSize="12.5"
              fontWeight="600" letterSpacing="1.5" fontFamily="Inter, system-ui, sans-serif">
          CLIENT
        </text>
      </g>

      {/* Local layer — USB camera + CameraNode */}
      <NodeBox idPrefix={id} x={50} y={110} w={220} h={70}
               title="USB Camera" subtitle="UVC / DirectShow / V4L2"
               accent="blue" icon={Icon.camera} />
      <NodeBox idPrefix={id} x={50} y={240} w={220} h={90}
               title="CameraNode" subtitle="Rust · FFmpeg · SQLite"
               accent="blue" icon={Icon.node} />
      <FlowArrow idPrefix={id} x1={160} y1={180} x2={160} y2={240} marker="arrow" />

      {/* Cloud layer — Command Center */}
      <NodeBox idPrefix={id} x={370} y={130} w={220} h={70}
               title="Command Center" subtitle="FastAPI · Clerk · SQLAlchemy"
               accent="green" icon={Icon.cloud} />
      <NodeBox idPrefix={id} x={370} y={220} w={220} h={55}
               title="Segment RAM Cache" subtitle="~15 × 1s chunks / camera"
               accent="green" />
      <NodeBox idPrefix={id} x={370} y={295} w={220} h={55}
               title="Incident DB" subtitle="Reports + evidence metadata"
               accent="green" />
      <FlowArrow idPrefix={id} x1={480} y1={200} x2={480} y2={220} marker="arrow-green" color={C.green} />
      <FlowArrow idPrefix={id} x1={480} y1={275} x2={480} y2={295} marker="arrow-green" color={C.green} />

      {/* Client layer — Browser / dashboard / agent */}
      <NodeBox idPrefix={id} x={690} y={130} w={220} h={70}
               title="Browser" subtitle="hls.js + Clerk JWT"
               accent="purple" icon={Icon.browser} />
      <NodeBox idPrefix={id} x={690} y={240} w={220} h={90}
               title="AI Agent (MCP)" subtitle="Claude, Cursor, custom"
               accent="purple" icon={Icon.agent} />

      {/* Crossing arrows with lock + label */}
      <FlowArrow idPrefix={id} x1={272} y1={280} x2={368} y2={160}
                 label="HTTPS push" marker="arrow" />
      <FlowArrow idPrefix={id} x1={592} y1={165} x2={688} y2={165}
                 label="same-origin" marker="arrow" />
      <FlowArrow idPrefix={id} x1={592} y1={245} x2={688} y2={280}
                 label="MCP tools" marker="arrow" dashed />

      {/* Lock icons on the outbound boundary */}
      <g transform="translate(300, 250)">{Icon.lock(C.amber)}</g>
      <g transform="translate(614, 152)">{Icon.lock(C.amber)}</g>

      {/* Footer: outbound-only annotation */}
      <g>
        <line x1={160} y1={355} x2={160} y2={365} stroke={C.amber} strokeWidth="1" />
        <text x={160} y={378} textAnchor="middle" fill={C.amber} fontSize="10.5"
              fontFamily="Inter, system-ui, sans-serif">
          outbound only · no inbound ports
        </text>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   2) HLS SEGMENT PIPELINE
// ══════════════════════════════════════════════════════════════════
// Detailed view of how a frame becomes a playable segment in the browser.
// Main path: camera → FFmpeg → .ts file → uploader → RAM cache → hls.js.
// Side branch: parallel motion-detector FFmpeg probe.
export function HlsPipelineDiagram() {
  const id = 'hls'
  return (
    <DiagramFrame
      ariaLabel="HLS pipeline: camera frames encoded to segments, uploaded, cached in RAM, served to hls.js, with a parallel motion probe."
      viewBox="0 0 980 460"
      aspectRatio="980/460"
      caption="Each camera runs two FFmpeg processes: the encoder producing HLS segments, and a second probe scoring scene changes for motion events. Playback is served from the RAM cache same-origin, never through object storage."
    >
      <Defs id={id} />

      {/* Swim-lane labels */}
      <text x={20} y={60} fill={C.blue} fontSize="11" fontWeight="600"
            letterSpacing="1.3" fontFamily="Inter, system-ui, sans-serif">CAMERANODE</text>
      <text x={20} y={260} fill={C.green} fontSize="11" fontWeight="600"
            letterSpacing="1.3" fontFamily="Inter, system-ui, sans-serif">CLOUD</text>
      <text x={20} y={380} fill={C.purple} fontSize="11" fontWeight="600"
            letterSpacing="1.3" fontFamily="Inter, system-ui, sans-serif">CLIENT</text>

      {/* Horizontal lane backgrounds */}
      <rect x={20}  y={70}  width={940} height={160} rx="12"
            fill="rgba(59,130,246,0.03)" stroke="rgba(59,130,246,0.15)" strokeWidth="1" strokeDasharray="3 3" />
      <rect x={20}  y={270} width={940} height={80}  rx="12"
            fill="rgba(34,197,94,0.04)"  stroke="rgba(34,197,94,0.2)"  strokeWidth="1" strokeDasharray="3 3" />
      <rect x={20}  y={390} width={940} height={50}  rx="12"
            fill="rgba(168,85,247,0.03)" stroke="rgba(168,85,247,0.15)" strokeWidth="1" strokeDasharray="3 3" />

      {/* CameraNode lane — main encode path */}
      <NodeBox idPrefix={id} x={50}  y={100} w={140} h={60} title="Camera" subtitle="raw frames" accent="blue" icon={Icon.camera} />
      <NodeBox idPrefix={id} x={220} y={100} w={140} h={60} title="FFmpeg" subtitle="libx264 / NVENC" accent="blue" />
      <NodeBox idPrefix={id} x={390} y={100} w={150} h={60} title="HLS segments" subtitle=".ts · 1s each" accent="blue" />
      <NodeBox idPrefix={id} x={570} y={100} w={170} h={60} title="Segment uploader" subtitle="POST /push-segment" accent="blue" />

      {/* Motion branch — second FFmpeg probe */}
      <NodeBox idPrefix={id} x={220} y={180} w={140} h={40} title="Motion probe" accent="amber" />
      <FlowArrow idPrefix={id} x1={220} y1={200} x2={200} y2={200} color={C.amber} marker="arrow-amber" />
      <FlowArrow idPrefix={id} x1={190} y1={155} x2={220} y2={200} color={C.amber} marker="arrow-amber" dashed />
      <NodeBox idPrefix={id} x={390} y={180} w={150} h={40} title="scene-change score" accent="amber" />
      <FlowArrow idPrefix={id} x1={360} y1={200} x2={390} y2={200} color={C.amber} marker="arrow-amber" />
      <NodeBox idPrefix={id} x={570} y={180} w={170} h={40} title="WebSocket event" accent="amber" />
      <FlowArrow idPrefix={id} x1={540} y1={200} x2={570} y2={200} color={C.amber} marker="arrow-amber" />

      {/* Arrows across CameraNode main row */}
      <FlowArrow idPrefix={id} x1={190} y1={130} x2={220} y2={130} marker="arrow" />
      <FlowArrow idPrefix={id} x1={360} y1={130} x2={390} y2={130} marker="arrow" />
      <FlowArrow idPrefix={id} x1={540} y1={130} x2={570} y2={130} marker="arrow" />

      {/* Cloud lane */}
      <NodeBox idPrefix={id} x={320} y={290} w={170} h={40} title="Segment RAM cache" accent="green" />
      <NodeBox idPrefix={id} x={520} y={290} w={170} h={40} title="Same-origin proxy" accent="green" />

      {/* Up from CameraNode → Cloud */}
      <FlowArrow idPrefix={id} x1={655} y1={160} x2={420} y2={290}
                 label="HTTPS" marker="arrow-green" color={C.green} />
      <FlowArrow idPrefix={id} x1={655} y1={220} x2={605} y2={290}
                 marker="arrow-green" color={C.green} dashed />
      <FlowArrow idPrefix={id} x1={490} y1={310} x2={520} y2={310} marker="arrow-green" color={C.green} />

      {/* Cloud → Client */}
      <NodeBox idPrefix={id} x={520} y={400} w={170} h={30} title="hls.js player" accent="purple" icon={Icon.browser} />
      <FlowArrow idPrefix={id} x1={605} y1={330} x2={605} y2={400}
                 label="GET .ts" marker="arrow" />

      {/* Cache TTL / window annotation */}
      <g transform="translate(800, 300)">
        <rect x="-60" y="-18" width="140" height="36" rx="18"
              fill="rgba(34,197,94,0.1)" stroke="rgba(34,197,94,0.4)" strokeWidth="1" />
        <text x="10" y="3" textAnchor="middle" fill={C.green} fontSize="11" fontWeight="600"
              fontFamily="Inter, system-ui, sans-serif">~15 segments</text>
        <text x="10" y="14" textAnchor="middle" fill={C.textMuted} fontSize="9.5"
              fontFamily="Inter, system-ui, sans-serif">rolling window</text>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   3) MOTION DETECTION STATE MACHINE
// ══════════════════════════════════════════════════════════════════
export function MotionStateMachineDiagram() {
  const id = 'motion'
  const cx = 480
  const cy = 230
  const r = 130
  // Node positions on a circle: idle (top), scoring (right), firing (bottom), cooldown (left)
  const pos = (deg) => {
    const rad = (deg - 90) * Math.PI / 180
    return { x: cx + Math.cos(rad) * r, y: cy + Math.sin(rad) * r }
  }
  const idle     = pos(0)
  const scoring  = pos(90)
  const firing   = pos(180)
  const cooldown = pos(270)

  return (
    <DiagramFrame
      ariaLabel="Motion detection state machine: Idle, Scoring, Fire Event, Cooldown."
      viewBox="0 0 960 460"
      aspectRatio="960/460"
      caption="The state machine runs once per camera. The cooldown prevents a waving branch or flickering light from hammering the events channel — tune the threshold to control sensitivity, the cooldown to control chatter."
    >
      <Defs id={id} />

      {/* Center-of-diagram label */}
      <text x={cx} y={cy - 6} textAnchor="middle" fill={C.textMuted}
            fontSize="10.5" letterSpacing="1.4" fontFamily="Inter, system-ui, sans-serif">
        PER-CAMERA
      </text>
      <text x={cx} y={cy + 10} textAnchor="middle" fill={C.text}
            fontSize="13" fontWeight="600" fontFamily="Inter, system-ui, sans-serif">
        Motion FSM
      </text>

      {/* Curved arrows between states (quadratic Béziers around the centre) */}
      <g fill="none" stroke={C.amber} strokeWidth="1.5" markerEnd={`url(#${id}-arrow-amber)`}>
        <path d={`M ${idle.x - 20} ${idle.y + 20}     Q ${cx + r * 0.95} ${cy - r * 0.55} ${scoring.x + 20} ${scoring.y - 20}`} />
        <path d={`M ${scoring.x - 20} ${scoring.y + 20} Q ${cx + r * 0.55} ${cy + r * 0.95} ${firing.x + 20}  ${firing.y - 20}`} />
        <path d={`M ${firing.x - 20} ${firing.y - 20}  Q ${cx - r * 0.95} ${cy + r * 0.55} ${cooldown.x + 20} ${cooldown.y + 20}`} />
        <path d={`M ${cooldown.x + 20} ${cooldown.y - 20} Q ${cx - r * 0.55} ${cy - r * 0.95} ${idle.x - 20}  ${idle.y + 20}`} />
      </g>

      {/* State nodes */}
      <NodeBox idPrefix={id} x={idle.x - 70}     y={idle.y - 28}     w={140} h={56}
               title="Idle" subtitle="waiting for frames" accent="default" />
      <NodeBox idPrefix={id} x={scoring.x - 70}  y={scoring.y - 28}  w={140} h={56}
               title="Scoring" subtitle="scene > threshold?" accent="amber" />
      <NodeBox idPrefix={id} x={firing.x - 70}   y={firing.y - 28}   w={140} h={56}
               title="Fire event" subtitle="MotionEvent emitted" accent="amber" />
      <NodeBox idPrefix={id} x={cooldown.x - 70} y={cooldown.y - 28} w={140} h={56}
               title="Cooldown" subtitle="30s quiet window" accent="default" />

      {/* Inline transition labels (tangent to the circle) */}
      <g fontFamily="Inter, system-ui, sans-serif" fontSize="10.5" fill={C.textDim}>
        <text x={cx + r + 30}  y={cy - r + 30}  textAnchor="start">frame in</text>
        <text x={cx + r - 30}  y={cy + r - 10}  textAnchor="middle">score ≥ threshold</text>
        <text x={cx - r - 30}  y={cy + r - 30}  textAnchor="end">delivered</text>
        <text x={cx - r + 30}  y={cy - r + 30}  textAnchor="start">elapsed</text>
      </g>

      {/* Side panel: delivery branches */}
      <g>
        <rect x={740} y={110} width={200} height={210} rx="10"
              fill={C.bgCard} stroke={C.border} strokeWidth="1" />
        <text x={840} y={135} textAnchor="middle" fill={C.text}
              fontSize="12" fontWeight="600" fontFamily="Inter, system-ui, sans-serif">
          Delivery
        </text>
        <line x1={760} y1={148} x2={920} y2={148} stroke={C.border} strokeWidth="1" />

        <circle cx={770} cy={172} r="4" fill={C.green} />
        <text x={782} y={176} fill={C.text} fontSize="11" fontWeight="600"
              fontFamily="Inter, system-ui, sans-serif">primary</text>
        <text x={782} y={190} fill={C.textMuted} fontSize="10"
              fontFamily="Inter, system-ui, sans-serif">WebSocket — low latency</text>

        <circle cx={770} cy={222} r="4" fill={C.amber} />
        <text x={782} y={226} fill={C.text} fontSize="11" fontWeight="600"
              fontFamily="Inter, system-ui, sans-serif">fallback</text>
        <text x={782} y={240} fill={C.textMuted} fontSize="10"
              fontFamily="Inter, system-ui, sans-serif">POST /cameras/{'{id}'}/motion</text>

        <circle cx={770} cy={272} r="4" fill={C.blue} />
        <text x={782} y={276} fill={C.text} fontSize="11" fontWeight="600"
              fontFamily="Inter, system-ui, sans-serif">consumed by</text>
        <text x={782} y={290} fill={C.textMuted} fontSize="10"
              fontFamily="Inter, system-ui, sans-serif">dashboard + MCP agents</text>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   4) CONFIGURATION PRECEDENCE
// ══════════════════════════════════════════════════════════════════
// Pyramid: higher in the stack = higher priority. SQLite DB is the wide base
// (source of truth, but lowest-priority at runtime override); CLI flags the
// narrow peak (always wins).
export function ConfigPrecedenceDiagram() {
  const id = 'cfg'
  const lanes = [
    { label: 'CLI flags',       sub: '--node-id · --api-key · --api-url',  color: C.purple, y: 80,  width: 260 },
    { label: 'Environment',     sub: 'SOURCEBOX_SENTRY_* · RUST_LOG',      color: C.amber,  y: 140, width: 360 },
    { label: 'YAML config',     sub: 'config.yaml (legacy, migrated)',     color: C.blue,   y: 200, width: 460 },
    { label: 'SQLite database', sub: 'data/node.db — source of truth',     color: C.green,  y: 260, width: 560 },
  ]
  return (
    <DiagramFrame
      ariaLabel="Configuration precedence: CLI flags override env vars, which override YAML, which overrides the SQLite database."
      viewBox="0 0 960 380"
      aspectRatio="960/380"
      caption="Higher bands override lower ones at runtime. The DB is the persistent source of truth — YAML, env vars, and CLI flags are ephemeral overrides layered on top for a single invocation."
    >
      <Defs id={id} />

      {/* Pyramid bands */}
      {lanes.map((ln, i) => {
        const cx = 480
        const x = cx - ln.width / 2
        return (
          <g key={i}>
            <rect x={x} y={ln.y} width={ln.width} height={42} rx="6"
                  fill={`url(#${id}-grad-${ln.color === C.purple ? 'purple' : ln.color === C.amber ? 'amber' : ln.color === C.blue ? 'blue' : 'green'})`}
                  opacity="0.85" />
            <text x={cx - ln.width / 2 + 18} y={ln.y + 20} fill={C.text}
                  fontSize="13" fontWeight="700" fontFamily="Inter, system-ui, sans-serif">
              {ln.label}
            </text>
            <text x={cx - ln.width / 2 + 18} y={ln.y + 35} fill="rgba(255,255,255,0.7)"
                  fontSize="10.5" fontFamily="'JetBrains Mono', Monaco, monospace">
              {ln.sub}
            </text>
            <text x={cx + ln.width / 2 + 16} y={ln.y + 26} fill={C.textMuted}
                  fontSize="10.5" fontFamily="Inter, system-ui, sans-serif">
              priority {4 - i}
            </text>
          </g>
        )
      })}

      {/* Vertical override arrow on the left */}
      <g>
        <line x1="100" y1="280" x2="100" y2="80" stroke={C.textDim} strokeWidth="1.5"
              markerEnd={`url(#${id}-arrow)`} />
        <text x="82" y="180" fill={C.textDim} fontSize="11" transform="rotate(-90 82 180)"
              textAnchor="middle" fontFamily="Inter, system-ui, sans-serif">
          overrides
        </text>
      </g>

      {/* Bottom note */}
      <text x={480} y={330} textAnchor="middle" fill={C.textMuted} fontSize="11"
            fontFamily="Inter, system-ui, sans-serif">
        Missing values fall through to the next band. Present values at a higher band win.
      </text>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   5) INCIDENT LIFECYCLE
// ══════════════════════════════════════════════════════════════════
// Left-to-right: Create → Investigate (parallel evidence attachments) →
// Finalize → Review → Resolve/Dismiss.
export function IncidentLifecycleDiagram() {
  const id = 'inc'
  return (
    <DiagramFrame
      ariaLabel="Incident lifecycle: create, investigate with evidence attachments, finalize, review, resolve or dismiss."
      viewBox="0 0 980 420"
      aspectRatio="980/420"
      caption="An incident is an append-only record until it's reviewed. Agents attach evidence as they investigate; the finalize call seals the markdown report body. Humans make the close call."
    >
      <Defs id={id} />

      {/* Main horizontal pipeline */}
      <NodeBox idPrefix={id} x={30}  y={180} w={150} h={70} title="Create" subtitle="title · severity · camera" accent="amber" />
      <NodeBox idPrefix={id} x={230} y={180} w={200} h={70} title="Investigate" subtitle="attach evidence + notes" accent="amber" />
      <NodeBox idPrefix={id} x={480} y={180} w={150} h={70} title="Finalize" subtitle="markdown report body" accent="blue" />
      <NodeBox idPrefix={id} x={680} y={180} w={150} h={70} title="Review" subtitle="human triage" accent="purple" />
      <NodeBox idPrefix={id} x={870} y={150} w={100} h={40} title="Resolve" accent="green" />
      <NodeBox idPrefix={id} x={870} y={240} w={100} h={40} title="Dismiss" accent="default" />

      {/* Main arrows */}
      <FlowArrow idPrefix={id} x1={180} y1={215} x2={230} y2={215} label="open" marker="arrow" />
      <FlowArrow idPrefix={id} x1={430} y1={215} x2={480} y2={215} label="write report" marker="arrow" />
      <FlowArrow idPrefix={id} x1={630} y1={215} x2={680} y2={215} label="publish" marker="arrow" />
      <FlowArrow idPrefix={id} x1={830} y1={205} x2={870} y2={170} marker="arrow-green" color={C.green} />
      <FlowArrow idPrefix={id} x1={830} y1={225} x2={870} y2={260} marker="arrow" />

      {/* Evidence attachments (branch off of Investigate) */}
      <g>
        <NodeBox idPrefix={id} x={230} y={80}  w={90}  h={36} title="snapshot" accent="cyan" />
        <NodeBox idPrefix={id} x={340} y={80}  w={90}  h={36} title="clip" accent="cyan" />
        <NodeBox idPrefix={id} x={230} y={310} w={90}  h={36} title="observation" accent="cyan" />
        <NodeBox idPrefix={id} x={340} y={310} w={90}  h={36} title="update_incident" accent="cyan" />

        {/* Lines curving up/down from Investigate */}
        <g fill="none" stroke={C.cyan} strokeWidth="1.5" strokeDasharray="3 3">
          <path d={`M 275 180 Q 275 150 275 116`} markerEnd={`url(#${id}-arrow)`} />
          <path d={`M 385 180 Q 385 150 385 116`} markerEnd={`url(#${id}-arrow)`} />
          <path d={`M 275 250 Q 275 280 275 310`} markerEnd={`url(#${id}-arrow)`} />
          <path d={`M 385 250 Q 385 280 385 310`} markerEnd={`url(#${id}-arrow)`} />
        </g>

        <text x={330} y={75} textAnchor="middle" fill={C.cyan} fontSize="10.5"
              letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">EVIDENCE</text>
        <text x={330} y={370} textAnchor="middle" fill={C.cyan} fontSize="10.5"
              letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">NOTES &amp; REVISIONS</text>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   6) MCP AGENT WORKFLOW
// ══════════════════════════════════════════════════════════════════
// Swim-lane sequence: Agent lane calls tools, Command Center lane dispatches,
// CameraNode lane produces the physical data (JPEG, clip bytes).
export function McpWorkflowDiagram() {
  const id = 'mcp'
  // y-coords per lane
  const L_AGENT = 100
  const L_CC = 230
  const L_NODE = 360
  const step = (x, y, label, color) => (
    <g key={x + label}>
      <circle cx={x} cy={y} r="10" fill={color} opacity="0.9" />
      <circle cx={x} cy={y} r="15" fill="none" stroke={color} strokeWidth="1" opacity="0.4" />
      <text x={x} y={y + 3.5} textAnchor="middle" fill="#fff" fontSize="10" fontWeight="700"
            fontFamily="Inter, system-ui, sans-serif">{label}</text>
    </g>
  )
  return (
    <DiagramFrame
      ariaLabel="MCP agent workflow: agent calls tools through Command Center, which fans out to CameraNode for camera data."
      viewBox="0 0 980 470"
      aspectRatio="980/470"
      caption="A typical agent loop. Each numbered step is a tool call. The agent drives the conversation; Command Center authenticates and dispatches; CameraNode produces image / clip bytes when the tool needs them."
    >
      <Defs id={id} />

      {/* Lane labels */}
      <text x={40} y={L_AGENT - 30} fill={C.purple} fontSize="11" fontWeight="600" letterSpacing="1.3"
            fontFamily="Inter, system-ui, sans-serif">AI AGENT</text>
      <text x={40} y={L_CC - 30}    fill={C.green}  fontSize="11" fontWeight="600" letterSpacing="1.3"
            fontFamily="Inter, system-ui, sans-serif">COMMAND CENTER</text>
      <text x={40} y={L_NODE - 30}  fill={C.blue}   fontSize="11" fontWeight="600" letterSpacing="1.3"
            fontFamily="Inter, system-ui, sans-serif">CAMERANODE</text>

      {/* Lane rails */}
      <line x1={40}  y1={L_AGENT} x2={940} y2={L_AGENT} stroke="rgba(168,85,247,0.2)" strokeWidth="1" strokeDasharray="2 3" />
      <line x1={40}  y1={L_CC}    x2={940} y2={L_CC}    stroke="rgba(34,197,94,0.2)"  strokeWidth="1" strokeDasharray="2 3" />
      <line x1={40}  y1={L_NODE}  x2={940} y2={L_NODE}  stroke="rgba(59,130,246,0.2)" strokeWidth="1" strokeDasharray="2 3" />

      {/* Vertical call arrows. X-positions of each numbered step. */}
      {[130, 260, 390, 520, 650, 780, 880].map((x, i) => (
        <line key={i} x1={x} y1={L_AGENT + 16} x2={x} y2={L_NODE - 16}
              stroke="rgba(255,255,255,0.08)" strokeWidth="1" strokeDasharray="2 3" />
      ))}

      {/* Step labels (at top / bottom of each column) */}
      <g fontFamily="'JetBrains Mono', Monaco, monospace" fontSize="10.5" fill={C.textDim} textAnchor="middle">
        <text x={130} y={L_AGENT - 12}>list_cameras</text>
        <text x={260} y={L_AGENT - 12}>view_camera</text>
        <text x={390} y={L_AGENT - 12}>watch_camera</text>
        <text x={520} y={L_AGENT - 12}>create_incident</text>
        <text x={650} y={L_AGENT - 12}>attach_snapshot</text>
        <text x={780} y={L_AGENT - 12}>attach_clip</text>
        <text x={880} y={L_AGENT - 12}>finalize_incident</text>
      </g>
      <g fontFamily="Inter, system-ui, sans-serif" fontSize="10" fill={C.textMuted} textAnchor="middle">
        <text x={130} y={L_NODE + 34}>metadata only</text>
        <text x={260} y={L_NODE + 34}>JPEG snapshot</text>
        <text x={390} y={L_NODE + 34}>JPEG burst</text>
        <text x={520} y={L_NODE + 34}>incident row</text>
        <text x={650} y={L_NODE + 34}>JPEG → DB</text>
        <text x={780} y={L_NODE + 34}>clip from cache</text>
        <text x={880} y={L_NODE + 34}>report body</text>
      </g>

      {/* Numbered step dots on each lane */}
      {/* Agent lane: every step goes through here */}
      {[130, 260, 390, 520, 650, 780, 880].map((x, i) => step(x, L_AGENT, String(i + 1), C.purple))}
      {/* Command Center lane: dispatch point for every call */}
      {[130, 260, 390, 520, 650, 780, 880].map((x, i) => step(x, L_CC, String(i + 1), C.green))}
      {/* CameraNode lane: only for calls that need a physical camera */}
      {[260, 390, 650, 780].map((x) => step(x, L_NODE, '✓', C.blue))}

      {/* Side panel: tool-class legend */}
      <g>
        <rect x={40} y={420} width={900} height={32} rx="8" fill={C.bgCard} stroke={C.border} strokeWidth="1" />
        <g fontFamily="Inter, system-ui, sans-serif" fontSize="10.5">
          <circle cx={70}  cy={436} r="5" fill={C.green} />
          <text x={82}  y={440} fill={C.text}>READ</text>
          <text x={122} y={440} fill={C.textMuted}>metadata only · list / get</text>

          <circle cx={340} cy={436} r="5" fill={C.amber} />
          <text x={352} y={440} fill={C.text}>VISUAL</text>
          <text x={395} y={440} fill={C.textMuted}>returns images the model can see</text>

          <circle cx={690} cy={436} r="5" fill={C.purple} />
          <text x={702} y={440} fill={C.text}>WRITE</text>
          <text x={742} y={440} fill={C.textMuted}>creates or updates state</text>
        </g>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   7) SECURITY MODEL (LAYERED RINGS)
// ══════════════════════════════════════════════════════════════════
// Concentric rings: outermost = transport (TLS), then auth, then data at rest,
// then tenant isolation. Labels pulled to the side to stay readable.
export function SecurityModelDiagram() {
  const id = 'sec'
  const cx = 340
  const cy = 240
  return (
    <DiagramFrame
      ariaLabel="Security model rings: transport, authentication, data at rest, tenant isolation."
      viewBox="0 0 960 480"
      aspectRatio="960/480"
      caption="Every call crosses every ring: TLS on the wire, a key or JWT at the edge, hashing or encryption wherever data lives, and org-scoped queries all the way down."
    >
      <Defs id={id} />

      {/* Rings */}
      <g>
        <circle cx={cx} cy={cy} r={185} fill="none"
                stroke="rgba(59,130,246,0.5)" strokeWidth="2" />
        <circle cx={cx} cy={cy} r={140} fill="none"
                stroke="rgba(168,85,247,0.5)" strokeWidth="2" />
        <circle cx={cx} cy={cy} r={95}  fill="none"
                stroke="rgba(245,158,11,0.55)" strokeWidth="2" />
        <circle cx={cx} cy={cy} r={50}  fill="rgba(34,197,94,0.15)"
                stroke={C.green} strokeWidth="2" />

        {/* Ring labels on top of each arc */}
        <text x={cx} y={cy - 188} textAnchor="middle" fill={C.blue} fontSize="11"
              fontWeight="700" letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">
          TRANSPORT
        </text>
        <text x={cx} y={cy - 143} textAnchor="middle" fill={C.purple} fontSize="11"
              fontWeight="700" letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">
          AUTH
        </text>
        <text x={cx} y={cy - 98} textAnchor="middle" fill={C.amber} fontSize="11"
              fontWeight="700" letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">
          DATA
        </text>
        <text x={cx} y={cy + 4} textAnchor="middle" fill={C.green} fontSize="11"
              fontWeight="700" letterSpacing="1.2" fontFamily="Inter, system-ui, sans-serif">
          TENANT
        </text>
        <text x={cx} y={cy + 18} textAnchor="middle" fill={C.green} fontSize="10"
              fontFamily="Inter, system-ui, sans-serif">
          isolation
        </text>
      </g>

      {/* Right-side detail panel */}
      <g transform="translate(580, 70)">
        <rect x="0" y="0" width="340" height="340" rx="12"
              fill={C.bgCard} stroke={C.border} strokeWidth="1" />

        <g fontFamily="Inter, system-ui, sans-serif">
          {/* Transport row */}
          <circle cx="22" cy="40" r="7" fill={C.blue} />
          <text x="40" y="36" fill={C.text} fontSize="13" fontWeight="600">Transport</text>
          <text x="40" y="54" fill={C.textMuted} fontSize="11">TLS 1.2+ on every hop</text>
          <text x="40" y="68" fill={C.textMuted} fontSize="11">outbound-only CameraNode → cloud</text>
          <line x1="22" y1="82" x2="320" y2="82" stroke={C.border} strokeWidth="1" />

          {/* Auth row */}
          <circle cx="22" cy="108" r="7" fill={C.purple} />
          <text x="40" y="104" fill={C.text} fontSize="13" fontWeight="600">Authentication</text>
          <text x="40" y="122" fill={C.textMuted} fontSize="11">Clerk JWT — dashboard users</text>
          <text x="40" y="136" fill={C.textMuted} fontSize="11">nak_*  — CameraNode keys</text>
          <text x="40" y="150" fill={C.textMuted} fontSize="11">osc_*  — MCP agent keys</text>
          <line x1="22" y1="164" x2="320" y2="164" stroke={C.border} strokeWidth="1" />

          {/* Data row */}
          <circle cx="22" cy="190" r="7" fill={C.amber} />
          <text x="40" y="186" fill={C.text} fontSize="13" fontWeight="600">Data at rest</text>
          <text x="40" y="204" fill={C.textMuted} fontSize="11">SHA-256 hashed API keys (backend)</text>
          <text x="40" y="218" fill={C.textMuted} fontSize="11">AES-256-GCM creds (CameraNode DB)</text>
          <text x="40" y="232" fill={C.textMuted} fontSize="11">live video in RAM only</text>
          <line x1="22" y1="246" x2="320" y2="246" stroke={C.border} strokeWidth="1" />

          {/* Tenant row */}
          <circle cx="22" cy="272" r="7" fill={C.green} />
          <text x="40" y="268" fill={C.text} fontSize="13" fontWeight="600">Tenant isolation</text>
          <text x="40" y="286" fill={C.textMuted} fontSize="11">every row scoped to org_id</text>
          <text x="40" y="300" fill={C.textMuted} fontSize="11">no cross-org reads, ever</text>
          <text x="40" y="314" fill={C.textMuted} fontSize="11">MCP scope filters per key</text>
        </g>
      </g>
    </DiagramFrame>
  )
}

// ══════════════════════════════════════════════════════════════════
//   8) DASHBOARD INFORMATION ARCHITECTURE
// ══════════════════════════════════════════════════════════════════
// Tree view of the dashboard: Dashboard root → four top sections with
// sub-items. Admin-only branches clearly tagged.
export function DashboardIaDiagram() {
  const id = 'ia'
  const branch = (x, y, title, accent, items, adminOnly = false) => (
    <g key={title}>
      <NodeBox idPrefix={id} x={x} y={y} w={170} h={42}
               title={title} accent={accent} />
      {adminOnly && (
        <g transform={`translate(${x + 124}, ${y - 4})`}>
          <rect x="0" y="0" width="48" height="16" rx="8"
                fill="rgba(168,85,247,0.15)" stroke={C.purple} strokeWidth="0.75" />
          <text x="24" y="11" textAnchor="middle" fill={C.purple}
                fontSize="9" fontWeight="700" letterSpacing="0.5"
                fontFamily="Inter, system-ui, sans-serif">ADMIN</text>
        </g>
      )}
      {items.map((it, i) => (
        <g key={it}>
          <line x1={x + 20} y1={y + 42} x2={x + 20} y2={y + 70 + i * 22}
                stroke={C.border} strokeWidth="1" />
          <line x1={x + 20} y1={y + 70 + i * 22} x2={x + 32} y2={y + 70 + i * 22}
                stroke={C.border} strokeWidth="1" />
          <text x={x + 38} y={y + 74 + i * 22} fill={C.textDim}
                fontSize="11" fontFamily="Inter, system-ui, sans-serif">{it}</text>
        </g>
      ))}
    </g>
  )
  return (
    <DiagramFrame
      ariaLabel="Dashboard information architecture tree: Live, Settings, Admin, Incidents."
      viewBox="0 0 1000 460"
      aspectRatio="1000/460"
      caption="The dashboard splits into four top-level sections. Admin-only branches are gated on the Pro / Pro Plus plan."
    >
      <Defs id={id} />

      {/* Root node */}
      <NodeBox idPrefix={id} x={400} y={30} w={200} h={52}
               title="Dashboard" subtitle="opensentry-command.fly.dev"
               accent="green" icon={Icon.cloud} />

      {/* Connecting tree lines from root down to four branches */}
      <g stroke={C.border} strokeWidth="1.5" fill="none">
        <line x1="500" y1="82"  x2="500" y2="105" />
        <line x1="110" y1="105" x2="890" y2="105" />
        <line x1="110" y1="105" x2="110" y2="125" />
        <line x1="370" y1="105" x2="370" y2="125" />
        <line x1="630" y1="105" x2="630" y2="125" />
        <line x1="890" y1="105" x2="890" y2="125" />
      </g>

      {/* Four sub-trees */}
      {branch(25, 125, 'Live view', 'blue', [
        'Camera tiles',
        'Fullscreen + multi-view',
        'Snapshot capture',
        'Manual record',
      ])}
      {branch(285, 125, 'Settings', 'amber', [
        'Node Management',
        'Recording Policy',
        'Organization',
        'Subscription',
        'Danger Zone',
      ], true)}
      {branch(545, 125, 'Admin', 'purple', [
        'Stream Access Logs',
        'Usage Statistics',
        'MCP Tool Activity',
        'System Health',
      ], true)}
      {branch(805, 125, 'Incidents', 'cyan', [
        'Open incidents',
        'Evidence viewer',
        'Markdown reports',
        'Triage actions',
      ])}
    </DiagramFrame>
  )
}
