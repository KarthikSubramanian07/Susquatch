# Accessibility

The password-protected Cloudflare Pages dashboard (`dashboard/public`) targets **WCAG 2.2 Level AA**.

## How it is checked

- Automated: axe-core with tags `wcag2a`, `wcag2aa`, `wcag21a`, `wcag21aa`, `wcag22aa` at 1440×900, 390×844 and 320×800 on `/login`, login error, the live dashboard, the Kill dialog, and the silent/stale banner state.
- Manual: skip links, keyboard chart scrub, visible `:focus-visible`, dialog focus and Escape, live status region, form labels and errors, contrast (including no opacity dimming for stale data), colour-independent health state text, reflow at 320 px, and `prefers-reduced-motion`.

## Known limits

- The equity sparkline is a decorative SVG with a text alternative and keyboard point readout; it is not a full data table.
- Dashboard routes require Basic auth or a session cookie; login is the public surface for unauthenticated visitors and link-preview crawlers.

## Reporting a barrier

Open an issue on this repository describing the page, the barrier, and the assistive technology or browser you used.
