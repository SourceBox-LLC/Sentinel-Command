/**
 * Parse a timestamp the API sent.
 *
 * Command Center stores and serves UTC without an offset —
 * `2026-10-04T04:18:01.743710` — because the Python it replaced did, and
 * CameraNode and the integrations read that shape. JavaScript reads an
 * ISO string with no offset as LOCAL time, so every such value rendered
 * shifted by the viewer's UTC offset: an incident filed at 9:18 PM in
 * California showed as 4:18 AM the next day. Only the motion panel
 * corrected for it, by appending "Z" itself.
 *
 * Every server timestamp goes through here instead. A value that already
 * carries "Z" or an offset is left alone, so this stays right if the API
 * ever starts sending them.
 */
const HAS_OFFSET = /(Z|[+-]\d{2}:?\d{2})$/i

export function parseServerDate(value) {
  if (value == null || value === "") return null
  if (value instanceof Date) return value
  if (typeof value === "number") return new Date(value)
  const text = String(value).trim()
  // A date with no time part is a calendar date, not an instant.
  if (!text.includes("T") && !text.includes(" ")) return new Date(text)
  return new Date(HAS_OFFSET.test(text) ? text : `${text}Z`)
}
