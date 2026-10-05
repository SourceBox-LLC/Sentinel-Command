// Shared by the incident report and the Sentinel run drawer: both show
// text an LLM wrote, which arrives as Markdown.
// Tiny markdown renderer — handles headings, bold, italic, ordered + unordered
// lists (with nesting), code, paragraphs.
// Deliberately minimal: no external dep, no HTML injection (we escape first).
export function renderMarkdown(md) {
  if (!md) return null
  const escaped = md
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")

  const lines = escaped.split("\n")
  const blocks = []
  let para = []
  // Stack of list blocks being built. Each frame:
  //   { type: "list", ordered: bool, indent: number, items: [{ text, children }] }
  // listStack[0] is the root; deeper entries are nested inside the previous
  // entry's last item.
  let listStack = []
  let codeBlock = null

  const flushPara = () => {
    if (para.length) {
      blocks.push({ type: "p", content: para.join(" ") })
      para = []
    }
  }
  const flushListStack = () => {
    if (listStack.length === 0) return
    while (listStack.length > 1) {
      const top = listStack.pop()
      const parent = listStack[listStack.length - 1]
      parent.items[parent.items.length - 1].children = top
    }
    blocks.push(listStack.pop())
  }

  for (const raw of lines) {
    // Preserve leading whitespace for list indent detection, strip trailing only.
    const line = raw.replace(/\s+$/, "")

    // Code fence
    if (line.trimStart().startsWith("```")) {
      flushPara(); flushListStack()
      if (codeBlock === null) {
        codeBlock = []
      } else {
        blocks.push({ type: "code", content: codeBlock.join("\n") })
        codeBlock = null
      }
      continue
    }
    if (codeBlock !== null) {
      codeBlock.push(line)
      continue
    }

    // Headings
    const h = line.match(/^(#{1,4})\s+(.*)$/)
    if (h) {
      flushPara(); flushListStack()
      blocks.push({ type: "h", level: h[1].length, content: h[2] })
      continue
    }

    // List items: `  - foo`, `* foo`, `1. foo`, etc.
    const li = line.match(/^(\s*)([-*]|\d+\.)\s+(.*)$/)
    if (li) {
      flushPara()
      const indent = li[1].length
      const ordered = /^\d+\./.test(li[2])
      const text = li[3]
      const newItem = { text, children: null }

      if (listStack.length === 0) {
        listStack.push({ type: "list", ordered, indent, items: [newItem] })
      } else {
        const top = listStack[listStack.length - 1]
        if (indent > top.indent) {
          // Nested — start a new list inside the current top's last item.
          listStack.push({ type: "list", ordered, indent, items: [newItem] })
        } else {
          // Pop back until we find a frame with indent <= this one.
          while (
            listStack.length > 1 &&
            listStack[listStack.length - 1].indent > indent
          ) {
            const popped = listStack.pop()
            const parent = listStack[listStack.length - 1]
            parent.items[parent.items.length - 1].children = popped
          }
          const nowTop = listStack[listStack.length - 1]
          if (nowTop.indent === indent) {
            // If marker style changed at the same level, flush and start fresh
            // so a numbered list doesn't accidentally merge into an unordered one.
            if (nowTop.ordered !== ordered) {
              flushListStack()
              listStack.push({ type: "list", ordered, indent, items: [newItem] })
            } else {
              nowTop.items.push(newItem)
            }
          } else {
            // Fell through to a shallower indent that doesn't match any frame.
            flushListStack()
            listStack.push({ type: "list", ordered, indent, items: [newItem] })
          }
        }
      }
      continue
    }

    // Blank line
    if (!line.trim()) {
      flushPara(); flushListStack()
      continue
    }

    // Paragraph
    flushListStack()
    para.push(line)
  }
  flushPara(); flushListStack()
  if (codeBlock !== null) {
    blocks.push({ type: "code", content: codeBlock.join("\n") })
  }

  // Inline formatting: **bold**, *italic*, `code`
  const inline = (s) =>
    s
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|\W)\*([^*\n]+)\*(\W|$)/g, "$1<em>$2</em>$3")
      .replace(/`([^`]+)`/g, "<code>$1</code>")

  const renderList = (block, key) => {
    const Tag = block.ordered ? "ol" : "ul"
    return (
      <Tag key={key}>
        {block.items.map((it, j) => (
          <li key={j}>
            <span dangerouslySetInnerHTML={{ __html: inline(it.text) }} />
            {it.children && renderList(it.children, `${key}-${j}`)}
          </li>
        ))}
      </Tag>
    )
  }

  return blocks.map((b, i) => {
    if (b.type === "h") {
      const Tag = `h${Math.min(6, 2 + b.level)}`
      return <Tag key={i} dangerouslySetInnerHTML={{ __html: inline(b.content) }} />
    }
    if (b.type === "list") {
      return renderList(b, i)
    }
    if (b.type === "code") {
      return <pre key={i}><code>{b.content}</code></pre>
    }
    return <p key={i} dangerouslySetInnerHTML={{ __html: inline(b.content) }} />
  })
}
