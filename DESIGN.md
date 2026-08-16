---
name: inseam
description: the web visual world of inseam — terminal-native, one thread of chartreuse on near-black
colors:
  ground: "#0a0b0a"
  thread: "#d8ff1c"
  ink: "#f2f0e9"
  ink-dim: "#a6ab9b"
  ink-faint: "#8a8f80"
  hairline: "rgba(242, 240, 233, 0.12)"
typography:
  display:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "clamp(1.55rem, 6.2vw, 4.4rem)"
    fontWeight: 700
    lineHeight: 1.14
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "clamp(1.4rem, 3vw, 1.9rem)"
    fontWeight: 700
    letterSpacing: "-0.01em"
  title:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "1.05rem"
    fontWeight: 700
  body:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "1rem"
    fontWeight: 400
    lineHeight: 1.6
  body-dim:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "0.9rem"
    fontWeight: 400
  label:
    fontFamily: "Commit Mono, ui-monospace, SF Mono, Menlo, monospace"
    fontSize: "0.8rem"
    fontWeight: 400
rounded:
  pill: "4px"
  focus: "2px"
components:
  button-cta:
    backgroundColor: "{colors.thread}"
    textColor: "{colors.ground}"
    rounded: "{rounded.pill}"
    padding: "0.45rem 0.95rem"
  nav-link:
    textColor: "{colors.ink-dim}"
  nav-link-hover:
    textColor: "{colors.thread}"
  install-command:
    textColor: "{colors.thread}"
    backgroundColor: "transparent"
---

# Design System: inseam (web surfaces)

This file records the visual world built at `apps/www.inseam.io` — the world every web surface (the docs site next) inherits. The brand — mark geometry, color roles, type stance, voice — is defined in [docs/brand/README.md](docs/brand/README.md), which is **binding**; this file records how that brand landed as a working web system and does not restate it as authority.

## Overview

**Creative North Star: "The Seam in the Terminal"**

The category-standard dev-tool landing played straight at Stripe/Linear craft — but the world refuses glow-and-cards furniture. Everything sits directly on near-black ground; a single chartreuse thread does all accent work; Commit Mono sets every character; the voice is lowercase and declarative. The hero of the site is an authored architecture diagram, not a screenshot: thin-outline devices holding fixed content dots, joined by glowing stitch lines that envelopes travel along. The whole page should look at home in a terminal — and read as one drawn artifact, not assembled components.

**Key Characteristics:**
- one accent, everywhere it matters: chartreuse `#d8ff1c` is links' underlines, prompts, threads, selection, caret, focus, and the single CTA — nothing else gets color
- no cards, no boxes: hierarchy is scale, brightness (ink → ink-dim → ink-faint), and generous vertical spacing
- monospace everywhere, lowercase everywhere, terminal punctuation (`$` prompts, em dashes)
- honesty as a visual element: every claim section carries a faint stage line ("shipping today." / "designed; … and we say so.")

## Colors

Five values total: one ground, one accent, a three-step ink ramp — plus a hairline alpha of the ink.

### Primary
- **Thread** (`#d8ff1c`): the chartreuse accent. Used for stitch threads and envelope outlines in diagrams, link underlines, link/nav hover color, the `$` prompt, the install command text, inline literals (`.lit`), text selection background, the text caret, the focus ring, and the one CTA pill. It is the only saturated color in the world.

### Neutral
- **Ground** (`#0a0b0a`): the only background. Near-black, faintly green. Also the text color on thread surfaces (CTA, selection).
- **Ink** (`#f2f0e9`): headings, wordmark, default body/link text, diagram device outlines and content dots. Off-white.
- **Ink-dim** (`#a6ab9b`): supporting prose, sublines, nav links at rest, terminal output lines, diagram labels.
- **Ink-faint** (`#8a8f80`): stage lines, footer text, the prompt `$` inside install buttons, idle copy glyphs.
- **Hairline** (`rgba(242, 240, 233, 0.12)`): the only border. Nav bottom rule, footer top rule, the terminal session's left rule.

### Named Rules
**The One Thread Rule.** Chartreuse is the only accent and it is never a surface except the CTA pill. If something else needs emphasis, it gets brighter ink or bigger type, not a new color.
**The Brightness Ladder Rule.** De-emphasis is done by stepping down the ink ramp (ink → ink-dim → ink-faint), never by opacity on text or by smaller-and-grayer boxes.

## Typography

**Only Font:** Commit Mono (self-hosted via `@fontsource/commit-mono`, weights 400 and 700 only), falling back to `ui-monospace, 'SF Mono', Menlo, monospace`.

**Character:** one voice at every size — the same monospace sets a 4.4rem hero and a 0.75rem "copied" toast. No italics. All copy, headings included, is lowercase and usually ends with a period.

### Hierarchy
- **Display** (700, `clamp(1.55rem, 6.2vw, 4.4rem)`, 1.14, `-0.02em`, `text-wrap: balance`): the hero headline only.
- **Headline** (700, `clamp(1.4rem, 3vw, 1.9rem)`, `-0.01em`): section headings ("discovery is a ladder, not a dump.").
- **Title** (700, 1.05rem): concept-column headings. Same size as body — weight alone carries it.
- **Body** (400, 1rem base, 1.6 line-height): default. Supporting prose steps to 0.9–0.92rem in ink-dim, max measure 60–62ch.
- **Label / stage** (400, 0.8rem, ink-faint): status lines, footer.
- **Code/session** (400, `clamp(0.72rem, 1.8vw, 0.88rem)`, 1.75): terminal transcripts.

### Named Rules
**The Lowercase Rule.** Every heading, label, nav item, and button is lowercase. Sentences end with periods, even fragments.
**The Two Weights Rule.** 400 and 700 exist; nothing between, nothing beyond, no italics.

## Layout

Single centered column, `max-width: 72rem`, horizontal padding `clamp(1.25rem, 4vw, 2.5rem)`. Reading sections (ladder, open) narrow to `max-width: 46rem` and left-align; the hero and diagram use the full column. Section rhythm is large clamped vertical padding (`clamp(4rem, 10vh, 7rem)` and up) with **no separators between sections** — hairlines appear only at the page frame (under nav, above footer).

- Nav: sticky, hairline bottom border, background `color-mix(in srgb, var(--ground) 92%, transparent)` with `backdrop-filter: blur(8px)`.
- Concepts: three equal columns, gap `clamp(2rem, 5vw, 4rem)`; stacks to one column at ≤800px.
- Breakpoints in use: **640px** (nav compacts: wordmark stitch hidden, links 0.78rem; install command wraps at 0.72rem; diagram labels swap to the drawn-dot key), **800px** (concepts stack), **1150px** (concept headings gain `white-space: nowrap`).
- `overflow-x: clip` on html/body; wide code scrolls inside its own container.

## Elevation & Depth

No shadows on surfaces, no layering — everything sits on one plane of ground. The single glow in the world is `filter: drop-shadow(0 0 9px rgba(216, 255, 28, 0.55))` on diagram stitch threads, read as light emitted by the thread, not as elevation.

### Named Rules
**The Thread Glows Alone Rule.** Drop-shadow glow belongs to chartreuse threads in authored diagrams only. Text, buttons, and containers never glow and never cast shadows.

## Shapes

The world is essentially radiusless because it is essentially boxless. The only enclosed UI element is the chartreuse CTA pill at **4px radius**; the focus ring rounds at 2px. Everything else — install commands, terminal sessions, concept columns — is open composition marked by color, indent, or a single hairline rule (the session's 1px left border). Diagram geometry uses small radii as drawing detail (device rects 6–10px, envelopes 2.5px), not as UI chrome.

**The Stitch.** The brand's dash-dash-dot mark drawn as a line: `stroke-dasharray: 16 11 16 11 0.01 11` with `stroke-linecap: round` (the 0.01 segment renders as the dot), stroke-width 2.5, in thread. This is how any connection is drawn. The wordmark and footer carry the mark as inline SVG per the brand doc's canonical geometry (ink dots, thread dashes).

## Components

### CTA ("get started")
- **Shape:** pill, 4px radius — the one enclosed element on the page.
- **Style:** thread background, ground text, 700 weight, padding 0.45rem 0.95rem (0.35rem 0.6rem ≤640px).
- **Hover:** `filter: brightness(1.12)`, 120ms ease-out. No color change, no movement.

### Links
- **Style:** ink text, 1px underline **in thread**, `text-underline-offset: 5px`.
- **Hover:** text turns thread, 120ms ease-out. Nav links drop the underline and rest in ink-dim.

### Install command
- Not a box: a bare inline-flex `<button>` — ink-faint `$` prompt, thread command text, ink-faint copy glyph (thread on hover). Click copies; a 0.75rem ink-dim "copied" toast fades in below-right for 1.6s. Wraps rather than truncates on small screens (0.72rem, glyph-width reserved).

### Terminal session
- `<pre>` with a 1px hairline left border and 1.1rem left padding — no background fill. Thread `$` prompts, ink command text, ink-dim output. Ends with a blinking thread caret (0.55em × 1em block, `blink 1.1s steps(2, jump-none) infinite`).

### Navigation
- Sticky translucent bar (92% ground + 8px blur), hairline bottom rule. Bold 1.15rem wordmark + 64×8 stitch SVG; right side 0.9rem ink-dim links plus the CTA pill.

### Stage line
- The honesty marker: 0.8rem ink-faint line closing a claim ("shipping today." / "designed; the boundary is not built yet, and we say so."). Every capability claim carries one.

### Authored diagram (signature)
The hero artifact and the grammar for any future diagram:
- **Devices:** hand-authored SVG outlines, no fill, ink stroke at 2px, round joins/caps, 0.85 opacity.
- **Content dots:** 4.5r ink circles, fixed inside devices — content never moves.
- **Threads:** stitch-dashed thread paths (dasharray above), 2.5 stroke, with the glow drop-shadow.
- **Envelopes:** 24×16 thread-stroked, ground-filled envelopes traveling the thread via `offset-path: path(...)` + `offset-distance` keyframes (7–9.5s linear infinite, staggered negative delays), `offset-rotate: 0deg`.
- **Labels:** ink-dim 15px SVG text tethered by 1px ink leader lines at 0.35 opacity. Under 640px, in-SVG labels hide and an HTML key list appears — 0.85rem ink-dim items led by 7px thread dots.

### Themed browser surfaces
The world extends into the chrome: `::selection` is thread-on-ground inverted; `caret-color: var(--thread)`; `:focus-visible` is a 2px thread outline, offset 3px, 2px radius; `scrollbar-color: #2a2d27 var(--ground)`; `color-scheme: dark`.

## Do's and Don'ts

### Do:
- **Do** build hierarchy with scale, weight, and the ink brightness ladder — a heading is bolder or bigger ink, never a boxed header.
- **Do** draw every connection as the stitch (`stroke-dasharray: 16 11 16 11 0.01 11`, round caps, thread) and keep the brand mark on one horizontal line, per the brand doc.
- **Do** keep motion to one authored moment per page (here: envelope travel) plus the blinking caret; under `prefers-reduced-motion` rest envelopes at `offset-distance: 45%` (the caret keeps blinking).
- **Do** close capability claims with an ink-faint stage line stating shipped vs. designed — early, and said so.
- **Do** use 120–160ms ease-out for hover/state transitions.

### Don't:
- **Don't** add cards, panels, filled containers, or background tints — the CTA pill is the only enclosed element; a second one needs the same justification.
- **Don't** introduce a second accent color, gradients, or glow outside diagram threads.
- **Don't** use uppercase, italics, intermediate font weights, or any non-mono face — including in OG images and future surfaces.
- **Don't** replace authored SVG diagrams with screenshots, stock illustration, or icon-library art; diagram devices are drawn thin-line outlines with fixed dots.
- **Don't** let marketing voice in: no exclamation points, no superlatives, no capitalized product name (`inseam`, always).
