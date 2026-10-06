import { describe, expect, it } from "vitest"
import { parseServerDate } from "../src/utils/time.js"

describe("parseServerDate", () => {
  it("reads an offset-less API timestamp as UTC", () => {
    expect(parseServerDate("2026-10-04T04:18:01.743710").toISOString())
      .toBe("2026-10-04T04:18:01.743Z")
  })
  it("leaves an explicit Z or offset alone", () => {
    expect(parseServerDate("2026-10-04T04:18:01Z").toISOString()).toBe("2026-10-04T04:18:01.000Z")
    expect(parseServerDate("2026-10-04T04:18:01+00:00").toISOString()).toBe("2026-10-04T04:18:01.000Z")
    expect(parseServerDate("2026-10-03T21:18:01-07:00").toISOString()).toBe("2026-10-04T04:18:01.000Z")
  })
  it("passes through nothing, Dates and epoch milliseconds", () => {
    expect(parseServerDate(null)).toBeNull()
    expect(parseServerDate("")).toBeNull()
    const d = new Date(0)
    expect(parseServerDate(d)).toBe(d)
    expect(parseServerDate(1000).toISOString()).toBe("1970-01-01T00:00:01.000Z")
  })
})
