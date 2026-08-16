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
          // No sidebar config: Starlight autogenerates it from the synced tree.
          // get-started is the entry point handed to agents; keep it at the
          // top of llms.txt so a bare docs link is enough to bootstrap one.
          plugins: [starlightLlmsTxt({ promote: ['index', 'get-started'] })],
      }),
	],

  adapter: cloudflare(),
});
