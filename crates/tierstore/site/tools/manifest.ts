// The manifest of a built tree: every file's path and sha256, one per line, sorted, except the files made from the guide (the docs pages and
// their Markdown, llms.txt and llms-full.txt; docs.test.ts and examples.test.ts hold them). test/golden.manifest is its committed copy; `npm run golden` rewrites it from a fresh build after a change meant to alter the output.

import { createHash } from 'crypto';
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from './build';
import { listFiles } from './check-dist';

/** The stems of the guide: one per crates/tierstore/docs/<stem>.md and docs/examples/<name>.md (as 'examples/<name>'); 'index' is the landing page. */
export function guideStems(docsDir: string): string[] {
  const top = readdirSync(docsDir).filter((f) => f.endsWith('.md')).map((f) => f.replace(/\.md$/, ''));
  const examples = existsSync(join(docsDir, 'examples')) ? readdirSync(join(docsDir, 'examples')).filter((f) => f.endsWith('.md')).map((f) => `examples/${f.replace(/\.md$/, '')}`) : [];
  return [...top, ...examples].sort();
}

/** The files made from the guide, which are not in the manifest (docs.test.ts and examples.test.ts hold them): the page of each stem
 *  (docs/<stem>/index.html, and docs/index.html for the landing page), its Markdown (docs/<stem>.md), and the two files that list
 *  them for programs. Everything else under docs/ and the rest of the tree is in it. */
export function isGuidePage(file: string, stems: readonly string[]): boolean {
  if (file === 'llms.txt' || file === 'llms-full.txt') return true;
  return stems.some((s) => file === `docs/${s}.md` || file === (s === 'index' ? 'docs/index.html' : `docs/${s}/index.html`));
}

export function manifestOf(dir: string, stems: readonly string[]): string {
  return listFiles(dir).filter((f) => !isGuidePage(f, stems)).map((f) => `${createHash('sha256').update(readFileSync(join(dir, f))).digest('hex')}  ${f}\n`).join('');
}

if (require.main === module) {
  const site = resolve(__dirname, '..', '..', '..');
  const out = join(site, 'build', 'golden-dist');
  buildSite({ browserDir: join(site, 'build', 'browser'), staticDir: join(site, 'static'), contentDir: join(site, 'content'), docsDir: join(site, '..', 'docs'), outDir: out });
  writeFileSync(join(site, 'test', 'golden.manifest'), manifestOf(out, guideStems(join(site, '..', 'docs'))));
}
