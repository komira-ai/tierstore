// The files for programs that read the site: /llms.txt (the llmstxt.org format: a title, a summary, and links to the
// pages as Markdown), /llms-full.txt (every page of the docs in one file), and the Markdown of each docs page at
// /docs/<name>.md and /docs/examples/<name>.md (build.ts writes those from rawMarkdown).

import { type Doc, LANDING, rawMarkdown } from './docs';
import { DOMAIN, PACKAGING_URL, RELEASES_URL, REPO_URL, SITE_NAME, WHITE_PAPER_URL } from './pages';

const BASE = `https://${DOMAIN}`;

/** The path a docs page's Markdown is served at. */
export const rawPath = (stem: string): string => `docs/${stem}.md`;

const SUMMARY = 'tierstore is an open source S3 server in front of a small, fast store and a large, cheap one. Clients see one bucket and use any S3 client; new objects are written to the hot tier and copied to the cold tier in the background, and the hot tier removes the oldest objects the cold tier already holds when it fills. A tier is a directory or a self-hosted S3-compatible store (MinIO, RustFS) reached over http. It is configured by one TOML file.';

const line = (d: Doc): string => `- [${d.title}](${BASE}/${rawPath(d.stem)}): ${d.summary.replace(/\s+/g, ' ')}`;

export function llmsTxt(docs: readonly Doc[]): string {
  const examples = docs.filter((d) => d.stem.startsWith('examples/'));
  const guide = docs.filter((d) => !d.stem.startsWith('examples/'));
  return [
    `# ${SITE_NAME}`,
    '',
    `> ${SUMMARY}`,
    '',
    `The pages below are plain Markdown. ${BASE}/llms-full.txt has all of them in one file. Everything in the docs is checked against the source by a test of the repository; what is planned and not built is under a heading that says so.`,
    '',
    '## Docs',
    '',
    ...guide.map(line),
    '',
    '## Examples',
    '',
    ...examples.map(line),
    '',
    '## Optional',
    '',
    `- [Technical report](${WHITE_PAPER_URL}): why tierstore exists and how its tiers work`,
    `- [Install](${BASE}/install/): install commands and how to verify a download`,
    `- [Source](${REPO_URL}): the repository`,
    `- [Releases](${RELEASES_URL}): the tarballs, their checksums and signatures`,
    `- [Packaging notes](${PACKAGING_URL}): the example unit and the stock SDK settings`,
    '',
  ].join('\n');
}

export function llmsFull(docs: readonly Doc[]): string {
  const parts = docs.map((d) => `<!-- ${BASE}${d.stem === LANDING ? '/docs/' : `/docs/${d.stem}/`} (${BASE}/${rawPath(d.stem)}) -->\n\n${rawMarkdown(d).trim()}\n`);
  return [`# ${SITE_NAME}: the docs in one file`, '', `> ${SUMMARY}`, '', ...parts.flatMap((p) => [p, '---', ''])].join('\n').replace(/\n---\n$/, '\n');
}
