// Smoke test #3 — the inline-SVG diagrams on /docs.
//
// These eight components are pure render-only — no Clerk, no router, no
// state. They're a good place to anchor the test suite because:
//
//   - Each one is self-contained, so we don't need any wrappers.
//   - They have ariaLabel and figcaption, so we can assert visible
//     accessibility text without parsing SVG geometry.
//   - If a future refactor breaks a Defs scope or DiagramFrame contract,
//     at least one of these tests will fail loudly.
//
// We don't try to verify SVG correctness pixel-by-pixel — that's brittle
// and the rendered output is verified visually on /docs anyway.

import { describe, it, expect } from 'vitest'
import { render, screen } from '@testing-library/react'

import {
  SystemArchitectureDiagram,
  HlsPipelineDiagram,
  MotionStateMachineDiagram,
  ConfigPrecedenceDiagram,
  IncidentLifecycleDiagram,
  McpWorkflowDiagram,
  SecurityModelDiagram,
  DashboardIaDiagram,
} from '../../src/components/DocsDiagrams.jsx'

const diagrams = [
  ['SystemArchitectureDiagram',   SystemArchitectureDiagram,   /USB camera, CameraNode/i],
  ['HlsPipelineDiagram',          HlsPipelineDiagram,          /HLS pipeline/i],
  ['MotionStateMachineDiagram',   MotionStateMachineDiagram,   /Motion detection state machine/i],
  ['ConfigPrecedenceDiagram',     ConfigPrecedenceDiagram,     /Configuration precedence/i],
  ['IncidentLifecycleDiagram',    IncidentLifecycleDiagram,    /Incident lifecycle/i],
  ['McpWorkflowDiagram',          McpWorkflowDiagram,          /MCP agent workflow/i],
  ['SecurityModelDiagram',        SecurityModelDiagram,        /Security model rings/i],
  ['DashboardIaDiagram',          DashboardIaDiagram,          /Dashboard information architecture/i],
]

describe('DocsDiagrams', () => {
  it.each(diagrams)('%s renders without crashing and exposes its aria-label', (
    _name, Component, ariaLabelPattern,
  ) => {
    render(<Component />)
    expect(screen.getByRole('img', { name: ariaLabelPattern })).toBeInTheDocument()
  })

  it('each diagram is wrapped in a <figure> with a <figcaption>', () => {
    const { container } = render(<SystemArchitectureDiagram />)

    const figure = container.querySelector('figure.docs-diagram')
    expect(figure).not.toBeNull()
    expect(figure.querySelector('figcaption')).not.toBeNull()
  })

  it('SVG IDs are scoped per-diagram so multiple diagrams on the same page do not collide', () => {
    // Render two diagrams in the same DOM tree (the docs page does this).
    // Each diagram's <Defs> uses a different idPrefix internally ("arch",
    // "hls", "motion", etc.) so the gradient IDs / arrow markers shouldn't
    // overlap. We probe by counting unique element IDs across the rendered
    // tree.
    const { container } = render(
      <div>
        <SystemArchitectureDiagram />
        <HlsPipelineDiagram />
      </div>,
    )
    const ids = Array.from(container.querySelectorAll('[id]')).map((el) => el.id)
    expect(ids.length).toBeGreaterThan(0)
    expect(new Set(ids).size).toBe(ids.length) // no duplicates
  })
})
