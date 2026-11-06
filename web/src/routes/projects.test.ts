import { describe, expect, test } from "bun:test"
import { createElement } from "react"
import { renderToStaticMarkup } from "react-dom/server"

import { OtherNamespacesSection } from "@/routes/projects"

describe("Projects namespace visibility", () => {
  test("keeps other namespaces behind a closed disclosure", () => {
    const markup = renderToStaticMarkup(createElement(OtherNamespacesSection, { items: [] }))
    expect(markup).toMatch(/<details\b[^>]*>/)
    expect(markup).not.toMatch(/<details\b[^>]*\bopen(?:=|\s|>)/)
    expect(markup).toMatch(/<summary\b/)
    expect(markup).toContain("Other namespaces")
    expect(markup).toContain("Show system namespaces")
  })
})
