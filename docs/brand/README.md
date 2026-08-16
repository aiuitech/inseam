# Brand

Inseam is the seam that runs underneath all your context — connecting you everywhere, seamlessly connecting everything. The brand stays technical, simple, straightforward: it should look at home in a terminal.

## Mark

Dash dash dot. Nodes joined by stitches. The iconography degrades gracefully to plain UTF-8:

```
●▬▬●▬▬●
```

- **Mark** (`assets/mark.svg`): a single stitch, `▬●▬` — used square (favicon, app icon).
- **Stitch strip** (`assets/stitch.svg`): the repeating `●▬▬●▬▬●` run — used as a horizontal rule, hero band, or underline. It may extend or truncate to fit; always cut on a whole element, never mid-shape.

Geometry (canonical units, scale freely): dash 49×35, node circle r 25, gap 8 between every element. Everything vertically centered on one line — it is a seam, never stacked.

## Color

| Role | Hex | Use |
| --- | --- | --- |
| Ground | `#0a0b0a` | Backgrounds. Near-black, faintly green. |
| Thread | `#d8ff1c` | The dashes, accents, links, highlights. Chartreuse. |
| Node | `#f2f0e9` | The dots, body text. Off-white; use pure `#ffffff` below ~32px for contrast. |

Dark-first. On rare light surfaces, invert: ground becomes the ink, thread stays `#d8ff1c` on white.

## Type

Monospace everywhere — headings, body, UI. Lowercase `inseam` is the wordmark: just the name set in the mono font, optionally over a short stitch strip. No custom lettering, no italics, no weights beyond regular/bold.

## Voice

Plain, lowercase-leaning, declarative. Say what it does. No exclamation points, no marketing superlatives.
