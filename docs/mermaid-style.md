# Mermaid visual style

Quiet surfaces, readable ink, purposeful color. Diagrams should feel native to
the terminal rather than like a separate presentation theme.

## Color hierarchy

All colors derive from the terminal foreground, background and ANSI palette.
Dark and light themes use the same hierarchy, not the same literal hex values.

| Role | Treatment |
| --- | --- |
| Canvas | Terminal background; no decorative frame |
| Node / participant / entity | Near-neutral surface, not a saturated blue panel |
| Group | A weaker neutral surface than its contents |
| Outline | Thin neutral ink; strong enough to distinguish the shape |
| Connector | Quiet neutral ink, with existing arrows and dash semantics |
| Label | Terminal foreground; contrasting ink for filled badges |
| Note | Restrained amber tint, not a fluorescent yellow sticky note |
| Category / status | Softened ANSI hues; color distinguishes data, not every border |

Node surfaces mix foreground into background by 6.5% in dark themes and 3.5%
in light themes. Groups start at 2.5%. Accent hues are softened toward neutral
before being used as chart colors. Fills back off toward the background to
retain 4.5:1 foreground-label contrast. Small filled badges need their own
contrasting ink, rather than a blanket text-color replacement. Outlines and
thin connectors aim for at least 3:1 against their adjacent surfaces. Broad
translucent Sankey ribbons stay subordinate to the labels drawn over them.

These are safeguards for themes with readable foreground/background pairs,
not a claim that an unreadable terminal theme or arbitrary custom Mermaid
colors become accessible automatically. Color is never a substitute for
labels, arrowheads, line styles or shape semantics.

## Shape and typography

- Favor thin, clean outlines and gently rounded cards. Sequence participant
  cards and class outer cards use 6 px corners and 1.25 px outlines; other
  shapes retain their upstream corner geometry.
- Preserve process, decision, stadium, cylinder, subroutine and other distinct
  shapes. Keep table grids, activation bars, packet fields, arrowheads and
  relationship markers intact.
- Preserve connector width and dash meaning. Do not make a thick flow, dashed
  dependency or crow's-foot relation look like an ordinary arrow.
- Use the bundled Noto Sans with existing layout metrics. Keep upstream title
  and heading hierarchy; do not enlarge or embolden text after sizing its box.
- Keep the actual smallest rendered label at least 12 px, unless the user
  explicitly requests a smaller text size. Wrap supported labels before layout,
  never by clipping, ellipsizing or shrinking their text afterward.
- No shadows, glow, bevels, textures or extra decoration. Data-bearing fills
  and Sankey gradients remain useful; decorative effects do not.

## Rendering contract

Style is resolved once when preparing the immutable scene, not on every
scroll, resize or viewport-band paint. Theme changes invalidate the scene.
There is no browser, animation loop, external font request or new raster
effect in the styling path.

Prefer the upstream theme API. Where a renderer hard-codes presentation,
change only identified SVG attributes, not label contents or geometry.
Explicit diagram styles opt out of the extra SVG polish rather than guessing
which generated attributes came from the author. The terminal palette still
supplies defaults; source theme-selection and `themeVariables` preambles
remain ignored.

This is a visual system, not a replacement layout engine. The existing
readability fallback remains, and upstream label collisions, clipped labels
and edge routing limitations still need separate layout work.

## Review checklist

- Inspect real Toastty captures in both dark and light themes, including the
  app's saturated default ANSI palette.
- Check node-heavy diagrams, table-like diagrams, categorical charts and
  thin-line/arrow-heavy diagrams, not only flowcharts.
- Keep label and geometry tests, full-image versus band-paint comparisons,
  cache/resize tests and intentional golden-image reviews.
- Never hide a layout defect by removing its label, recoloring it into the
  background, dropping an edge or changing the example's data.

See the [23-family gallery](mermaid-gallery.md) for examples and
[Markdown rendering](markdown.md#mermaid-readability) for fitting behavior.
