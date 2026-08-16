// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';

import cloudflare from '@astrojs/cloudflare';

// Content is synced from the repo's ./docs tree by scripts/sync-content.mjs
// (run automatically before dev and build); the sidebar mirrors that tree.
// https://astro.build/config
export default defineConfig({
  site: 'https://docs.inseam.io',
  integrations: [
      starlight({
          title: 'Inseam',
          social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/aiuitech/inseam' }],
          // Top-level order and group labels are pinned here; each group's
          // contents still autogenerate from the synced docs tree, so pages
          // added under an existing folder need no config change. A new
          // top-level docs/ folder needs one line here.
          sidebar: [
              { slug: 'get-started' },
              { slug: 'cli' },
              { slug: 'configuration' },
              { label: 'Architecture', items: [{ autogenerate: { directory: 'architecture' } }] },
              { label: 'Indexing', items: [{ autogenerate: { directory: 'indexing' } }] },
              { label: 'Finder', items: [{ autogenerate: { directory: 'finder' } }] },
              { label: 'Plugins', items: [{ autogenerate: { directory: 'plugins' } }] },
              { label: 'Crates', items: [{ autogenerate: { directory: 'crates' } }], collapsed: true },
              { label: 'Skills', items: [{ autogenerate: { directory: 'skills' } }], collapsed: true },
              { label: 'Brand', items: [{ autogenerate: { directory: 'brand' } }], collapsed: true },
              { slug: 'glossary' },
          ],
          // get-started is the entry point handed to agents; keep it at the
          // top of llms.txt so a bare docs link is enough to bootstrap one.
          plugins: [starlightLlmsTxt({ promote: ['index', 'get-started'] })],
      }),
	],

  adapter: cloudflare(),
});
