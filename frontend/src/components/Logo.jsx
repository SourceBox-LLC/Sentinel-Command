// Sentinel brand mark.
//
// Shield silhouette with a camera-lens iris inside — semantically a
// portmanteau of the two halves of the name: "Sentinel" (guarding /
// shield) and the surveillance-camera context of the product.
//
// Stroke uses a green→purple gradient (both brand accents); the
// inner pupil is solid green so the mark reads as "active / watching"
// even at small sizes.  At favicon scale the gradient flattens
// optically — see /favicon.svg for a thicker monochrome variant
// tuned for 16-32px tab rendering.

import { useId } from "react"

export function LogoMark({ size = 32, monochrome = false, ...props }) {
  // Each instance gets a unique gradient id so multiple Marks on the
  // same page (header + footer + sign-in panel) don't share + clobber
  // each other's <defs>.  useId() is SSR/concurrent-safe; previous
  // module-scope counter mutated during render and could drift between
  // server + client.
  const id = `sbs-logo-grad-${useId()}`

  const strokeFill = monochrome ? "#22c55e" : `url(#${id})`

  return (
    <svg
      viewBox="0 0 32 32"
      width={size}
      height={size}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label="Sentinel"
      {...props}
    >
      {!monochrome && (
        <defs>
          <linearGradient id={id} x1="4" y1="3" x2="28" y2="29" gradientUnits="userSpaceOnUse">
            <stop offset="0%" stopColor="#22c55e" />
            <stop offset="100%" stopColor="#a855f7" />
          </linearGradient>
        </defs>
      )}
      {/* Shield body */}
      <path
        d="M16 3 L26 7 V15.5 C26 22 21 27 16 29 C11 27 6 22 6 15.5 V7 Z"
        stroke={strokeFill}
        strokeWidth="2"
        strokeLinejoin="round"
      />
      {/* Camera lens iris */}
      <circle
        cx="16"
        cy="15"
        r="5"
        stroke={strokeFill}
        strokeWidth="1.5"
      />
      {/* Lens pupil — always solid green; the visual focus of the mark */}
      <circle cx="16" cy="15" r="2" fill="#22c55e" />
    </svg>
  )
}

export default LogoMark
