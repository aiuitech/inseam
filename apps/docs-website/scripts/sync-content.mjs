// Syncs the repo's ./docs tree (the single source of truth, plain markdown
// with no frontmatter) into src/content/docs, injecting the frontmatter
// Starlight requires. Runs before dev and build; the output is generated and
// gitignored — edit ../../docs, never src/content/docs.
//
// Transforms per file:
// - title: lifted from the first `# ` heading (removed from the body, since
//   Starlight renders the title itself); falls back to the filename.
// - README.md becomes index.md (the section landing page).
// - Intra-docs `*.md` links are rewritten to site-root slug paths; links
//   escaping ./docs (e.g. ../design) are rewritten to GitHub.
import { cp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const site = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repo = path.dirname(path.dirname(site));
const source = path.join(repo, 'docs');
const dest = path.join(site, 'src/content/docs');
const github = 'https://github.com/aiuitech/inseam/blob/main';

const mdFiles = (await readdir(source, { recursive: true })).filter(
	(f) => f.endsWith('.md') && !path.basename(f).startsWith('.'),
);

await rm(dest, { recursive: true, force: true });
await mkdir(dest, { recursive: true });

for (const rel of mdFiles) {
	const raw = await readFile(path.join(source, rel), 'utf8');
	const { title, body } = splitTitle(raw, rel);
	const outRel = rel.replace(/README\.md$/, 'index.md');
	const out = path.join(dest, outRel);
	await mkdir(path.dirname(out), { recursive: true });
	await writeFile(
		out,
		`---\ntitle: ${JSON.stringify(title)}\n---\n\n${rewriteLinks(body, rel)}`,
	);
}
console.log(`synced ${mdFiles.length} pages from ${path.relative(site, source)}`);

function splitTitle(raw, rel) {
	// Generated pages open with a <!-- GENERATED --> comment; look past it.
	const match = raw.match(/^(\s*(?:<!--[^]*?-->\s*)*)# (.+)\n+/);
	if (match)
		return { title: match[2].trim(), body: raw.slice(match[0].length) };
	return { title: path.basename(rel, '.md'), body: raw };
}

function rewriteLinks(body, rel) {
	return body.replace(
		/\]\(([^)\s]+?\.md)(#[^)]*)?\)/g,
		(whole, target, anchor = '') => {
			if (/^[a-z]+:/.test(target)) return whole; // absolute URL, leave it
			const resolved = path.join(path.dirname(rel), target);
			if (resolved.startsWith('..')) return `](${github}/${path.join('docs', resolved)})`;
			const slug = resolved.replace(/(?:^|\/)README\.md$/, '').replace(/\.md$/, '');
			return `](/${slug}${slug ? '/' : ''}${anchor})`;
		},
	);
}
