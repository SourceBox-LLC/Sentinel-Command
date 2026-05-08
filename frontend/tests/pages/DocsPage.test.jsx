// Smoke test for the post-split DocsPage.
//
// The 1,747-line monolith was broken into 19 sections + a shared context in
// commit XXX. This test guards three things:
//
//   1. The composition shell still mounts without crashing.
//   2. Every section ID we list in the sidebar nav has a corresponding
//      <section id="..."> in the rendered output. If a section file gets
//      renamed or its `id` changes, the dead sidebar link will surface here
//      instead of as a broken anchor in production.
//   3. The shared context boundary is wired correctly — useDocs() inside
//      sections doesn't throw "must be used inside <DocsProvider>".
//
// We don't assert the inner content of each section — that's what visual
// inspection on the live /docs URL is for. This is a structural smoke test,
// not a content snapshot.

import { describe, it, expect } from "vitest"
import { render } from "@testing-library/react"
import { MemoryRouter } from "react-router-dom"

import DocsPage from "../../src/pages/DocsPage"
import { ToastProvider } from "../../src/hooks/useToasts.jsx"


// Section IDs the sidebar links to. Keep in sync with DocsPage.jsx's
// DocsSidebar component — if you add a sidebar link, add the id here so
// the test catches a broken anchor before it reaches production.
const EXPECTED_SECTION_IDS = [
  "getting-started",
  "architecture",
  "cloudnode-setup",
  "configuration",
  "deployment",
  "motion-detection",
  "terminal-dashboard",
  "dashboard",
  "recording",
  "camera-groups",
  "notifications",
  "mcp",
  "sentinel",
  "plans",
  "security-procedures",
  "troubleshooting",
  "faq",
  "api-reference",
  "api-rate-limits",
]


// DocsPage uses useToasts() (for the copy-link toast) so every render
// has to be wrapped in a ToastProvider.  Single helper keeps the four
// test bodies short.
function renderDocs() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <DocsPage />
      </ToastProvider>
    </MemoryRouter>,
  )
}


describe("DocsPage (post-split)", () => {
  it("mounts without throwing", () => {
    expect(() => renderDocs()).not.toThrow()
  })

  it("renders every section the sidebar links to", () => {
    const { container } = renderDocs()

    for (const id of EXPECTED_SECTION_IDS) {
      const section = container.querySelector(`section#${id}`)
      expect(section, `missing <section id="${id}">`).not.toBeNull()
    }
  })

  it("renders the resources block (the section without an id)", () => {
    const { container } = renderDocs()
    // Just check the resource grid is present — that's the only block
    // without an id and we don't want to slip its loss past the suite.
    expect(container.querySelector(".docs-resources")).not.toBeNull()
  })

  it("renders the bottom CTA link to /sign-up", () => {
    const { container } = renderDocs()
    const ctaLinks = Array.from(container.querySelectorAll(".docs-cta-btn"))
    expect(ctaLinks.length).toBeGreaterThan(0)
    expect(ctaLinks[0].getAttribute("href")).toBe("/sign-up")
  })
})
