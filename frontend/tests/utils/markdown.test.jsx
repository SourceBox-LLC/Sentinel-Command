import { describe, it, expect } from "vitest"
import { render } from "@testing-library/react"
import { renderMarkdown } from "../../src/utils/markdown.jsx"

describe("renderMarkdown", () => {
  it("shows code-block text as written, not double-escaped", () => {
    const { container } = render(<div>{renderMarkdown("```\nif a < b && c > d\n```")}</div>)
    expect(container.querySelector("pre code").textContent).toBe("if a < b && c > d")
  })

  it("never turns report text into markup", () => {
    // An incident report is written by a model a camera feed can steer.
    const { container } = render(
      <div>{renderMarkdown('<img src=x onerror="alert(1)"> **bold** <script>x</script>\n\n```\n<script>y</script>\n```')}</div>,
    )
    expect(container.querySelector("img")).toBeNull()
    expect(container.querySelector("script")).toBeNull()
    expect(container.querySelector("strong").textContent).toBe("bold")
    expect(container.querySelector("pre code").textContent).toBe("<script>y</script>")
  })
})
