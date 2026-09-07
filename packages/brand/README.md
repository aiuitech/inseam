# @inseam/brand

The inseam brand for Tailwind v4 and shadcn apps: `theme.css`, the assets, and the components tuned to them. Documented in [docs/brand/package.md](../../docs/brand/package.md); the brand itself is [docs/brand/README.md](../../docs/brand/README.md).

```css
@import "tailwindcss";
@import "@inseam/brand/theme.css";
```

```tsx
import { Button } from "@inseam/brand/components/ui/button"
```

Consumed as `workspace:*` inside this repo and as a git dependency (`git+ssh://git@github.com/aiuitech/inseam.git#path:packages/brand`) from outside it. No build step: Vite compiles the source directly.
