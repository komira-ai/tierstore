// The docs landing page, the examples with their diagrams, and the files for programs (the Markdown of every page,
// llms.txt and llms-full.txt).
//
// Catches: a landing page whose configuration tables differ from the reference (a key that is in configuration.md and
// not on /docs/, or a changed default that did not reach it); an example page without its diagram, file, commands or
// limits, or a card that does not link to it; a diagram that cannot be drawn (a box off the grid, an arrow to nothing,
// words too long for a box) and is not an error; a diagram that needs a script, an id or a style attribute, or whose
// colours are below 4.5:1; Markdown that is served with links that do not resolve from where it is, or with the
// diagram left as a fence; an llms.txt that does not follow the llmstxt.org format or leaves a page out.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { describeDiagram, parseDiagram, renderDiagram, sizeOf } from '../tools/diagram';
import { configTables, landingMarkdown, loadDocs, rawMarkdown, TABLE_SECTIONS } from '../tools/docs';
import { llmsFull, llmsTxt } from '../tools/llms';
import { buildReal, DOCS, SITE } from './support';

const dist = buildReal('examples');
const docs = loadDocs(DOCS);
const EXAMPLES = ['directory-as-bucket', 'cache-in-front-of-s3', 'cache-hot-cold'];
const read = (...p: string[]): string => readFileSync(join(dist, ...p), 'utf8');
const text = (html: string): string => html.replace(/<[^>]+>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');

const GOOD = ['alt: A client reads from tierstore, which reads from a store.', 'node c 0 1 "Client" "any client"', 'node t 1 1 "tierstore" "S3" main', 'node s 2 0 "store" "disk" hot', 'edge c t "read" both', 'edge t s "read" dashed'].join('\n');

// ---- the diagram ----

test('a diagram description is drawn as an inline SVG: a box for each node, an arrow for each edge, its words escaped, no id, style or script', () => {
  const svg = renderDiagram(parseDiagram(GOOD));
  assert.equal(svg.match(/<g class="dg-node/g)?.length, 3);
  assert.equal(svg.match(/<g class="dg-edge/g)?.length, 2);
  assert.equal(svg.match(/<polygon/g)?.length, 3, 'one head for an arrow and two for a two-way arrow');
  assert.match(svg, /<g class="dg-node dg-main">/);
  assert.match(svg, /<g class="dg-edge dg-edge-dashed"><path class="dg-line dg-dashed"/);
  assert.doesNotMatch(svg, /\sid=|\sstyle=|\son[a-z]+=|<script|<animate|href=/);
  const { w, h } = sizeOf(parseDiagram(GOOD));
  assert.ok(svg.includes(`viewBox="0 0 ${w} ${h}"`));
  assert.match(renderDiagram(parseDiagram(GOOD.replace('"Client"', '"A<b>"'))), /A&lt;b&gt;/);
  assert.equal(renderDiagram(parseDiagram(GOOD)), renderDiagram(parseDiagram(GOOD)), 'the same description gives the same bytes');
});

test('a diagram that cannot be drawn is a build error that says why', () => {
  const bad = (change: (s: string) => string, why: RegExp): void => assert.throws(() => parseDiagram(change(GOOD), 'x.md'), why);
  bad((s) => s.replace('node c 0 1', 'node c 3 1'), /node c is in column 3/);
  bad((s) => s.replace('node c 0 1', 'node c 0 1.3'), /not a number of halves/);
  bad((s) => s.replace('node t 1 1', 'node c 1 1'), /used twice/);
  bad((s) => s.replace('node t 1 1', 'node t 0 1'), /on the place of another/);
  bad((s) => s.replace('"tierstore"', '"a label far too long"'), /words too long/);
  bad((s) => s.replace('main', 'huge'), /of kind huge/);
  bad((s) => s.replace('edge t s', 'edge t nothing'), /names nothing, which is no node/);
  bad((s) => s.replace('edge t s "read" dashed', 'edge t t "read"'), /to itself/);
  bad((s) => s.replace('"read" dashed', '"a label too long" dashed'), /longer than 10/);
  bad((s) => s.replace('dashed', 'dotted'), /flag is both or dashed/);
  bad((s) => s.replace(/^alt:.*\n/, ''), /no alt line/);
  bad((s) => `${s}\nbox x`, /cannot read/);
});

test('the picture as text, for the Markdown that programs read, says the same: the caption and each arrow', () => {
  assert.equal(describeDiagram(parseDiagram(GOOD)), 'Diagram: A client reads from tierstore, which reads from a store.\n\n- Client <-> tierstore: read\n- tierstore -> store: read (in the background)');
});

test('the colours of the diagram are at least 4.5:1 on what they are drawn on', () => {
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8') + readFileSync(join(SITE, 'static', 'theme.css'), 'utf8');
  const v = (name: string): string => (new RegExp(`--${name}:\\s*(#[0-9a-f]{6})`).exec(css)?.[1] ?? assert.fail(name));
  const lum = (hex: string): number => {
    const n = parseInt(hex.slice(1), 16);
    const c = (x: number): number => (x / 255 <= 0.03928 ? x / 255 / 12.92 : ((x / 255 + 0.055) / 1.055) ** 2.4);
    return 0.2126 * c((n >> 16) & 255) + 0.7152 * c((n >> 8) & 255) + 0.0722 * c(n & 255);
  };
  const ratio = (a: string, b: string): number => (Math.max(lum(a), lum(b)) + 0.05) / (Math.min(lum(a), lum(b)) + 0.05);
  const pairs: Array<[string, string, string]> = [
    ['name on a plain box', v('ink'), '#ffffff'],
    ['second line on a plain box', v('fg-muted'), '#ffffff'],
    ['name on the main box', v('on-deep'), v('deep-2')],
    ['second line on the main box', v('on-deep-muted'), v('deep-2')],
    ['name on the hot box', v('ink'), v('accent-soft')],
    ['second line on the hot box', v('fg-muted'), v('accent-soft')],
    ['second line on the remote box', v('fg-muted'), v('bg-soft')],
    ['second line on a freed box', v('fg-muted'), v('bg')],
    ['an arrow label', v('fg-2'), v('bg')],
    ['a dashed arrow on the page', v('accent-text'), v('bg')],
  ];
  for (const [what, fg, bg] of pairs) assert.ok(ratio(fg, bg) >= 4.5, `${what} is ${ratio(fg, bg).toFixed(2)}:1`);
});

test('on a phone the words of a diagram are set larger, so that they stay readable at the width of the screen', () => {
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  const phone = /@media \(max-width: 40rem\) \{\n  \.dg-node \.dg-name \{ font-size: (\d+)px; \}\n  \.dg-node \.dg-sub \{ font-size: ([\d.]+)px; \}\n  \.dg-edge-label \{ font-size: ([\d.]+)px; \}/.exec(css);
  assert.ok(phone !== null, 'no phone rule for the diagram');
  const base = (rule: string): number => Number(new RegExp(`^\\.${rule} \\{[^}]*?(\\d+(?:\\.\\d+)?)px`, 'm').exec(css)?.[1]);
  assert.ok(Number(phone[1]) > base('dg-node \\.dg-name'));
  assert.ok(Number(phone[2]) > base('dg-node \\.dg-sub'));
  assert.ok(Number(phone[3]) > base('dg-edge-label'));
  // At the widest, a name of the longest allowed length still fits its box at the larger size (about 0.55 em a character).
  assert.ok(11 * Number(phone[1]) * 0.55 <= 128, 'a name does not fit its box on a phone');
});

// ---- the examples ----

test('each example is a page with what it is for, a diagram, the file, the commands, what to expect and the limits, in that order', () => {
  for (const name of EXAMPLES) {
    const html = read('docs', 'examples', name, 'index.html');
    const headings = [...html.matchAll(/<h2 id="([a-z-]+)"/g)].map((m) => m[1]);
    assert.deepEqual(headings, ['what-it-is-for', 'the-file', 'commands', 'what-to-expect', 'limits'], name);
    assert.equal(html.match(/<figure class="diagram">/g)?.length, 1, name);
    assert.ok(html.indexOf('<figure class="diagram">') < html.indexOf('id="the-file"'), `${name}: the diagram comes before the file`);
    assert.match(html, /<pre class="hl"><span class="h-t">\[\[interface\]\]<\/span>/, `${name}: the file is coloured`);
    assert.ok(!html.includes('```'), `${name}: a fence reached the page`);
  }
});

test('the examples say what the code does: the hot tier is not an LRU, the cache is, a hot tier needs activating, a tier is reached over http and S3 itself is not a tier yet', () => {
  const dir = text(read('docs', 'examples', 'directory-as-bucket', 'index.html'));
  assert.match(dir, /evicts the objects that were written \(or copied back\) longest ago, and only those that the cold tier already holds\. It is not an LRU/);
  assert.match(dir, /tierstore's own flat layout: names are percent-encoded and a 32-byte tag comes before each body\. It cannot serve a tree of files that already exists/);
  const cache = text(read('docs', 'examples', 'cache-in-front-of-s3', 'index.html'));
  assert.match(cache, /evicts the least recently used objects first/);
  assert.match(cache, /http:\/\/minio\.example:9000/);
  assert.match(cache, /Amazon S3 itself is not supported as a store yet/);
  assert.doesNotMatch(cache, /format-epoch|--activate/, 'a build node has no hot tier to activate');
  const both = text(read('docs', 'examples', 'cache-hot-cold', 'index.html'));
  assert.match(both, /`?local_cache\.write_through`? is true whether it is left out or given, and `?false`? is refused/);
  assert.match(both, /It is not an LRU/);
  for (const name of ['directory-as-bucket', 'cache-hot-cold']) {
    assert.doesNotMatch(text(read('docs', 'examples', name, 'index.html')), /format-epoch|--activate/, `${name}: a new hot tier needs no activation step`);
  }
});

test('each diagram shows the arrows its page names, in one vocabulary: solid is a request, dashed is the background copy; the cache is its own box; no box for freed space', () => {
  const arrows = (name: string): string[] => [...read('docs', 'examples', name, 'index.html').matchAll(/<text class="dg-edge-label"[^>]*>([^<]+)<\/text>/g)].map((m) => m[1]);
  const dashed = (name: string): number => read('docs', 'examples', name, 'index.html').match(/dg-edge dg-edge-dashed/g)?.length ?? 0;
  const boxes = (name: string): string[] => [...read('docs', 'examples', name, 'index.html').matchAll(/<text class="dg-name"[^>]*>([^<]+)<\/text>/g)].map((m) => m[1]);
  assert.deepEqual(arrows('directory-as-bucket'), ['read/write', 'write', 'read miss', 'copy']);
  assert.deepEqual(boxes('directory-as-bucket'), ['Clients', 'tierstore', 'hot dir', 'cold dir']);
  assert.deepEqual(arrows('cache-hot-cold'), ['read/write', 'read/write', 'miss/write', 'copy']);
  assert.deepEqual(boxes('cache-hot-cold'), ['Clients', 'tierstore', 'local cache', 'hot dir', 'cold store']);
  assert.deepEqual(arrows('cache-in-front-of-s3'), ['read', 'read', 'read miss', 'keep copy']);
  assert.deepEqual(boxes('cache-in-front-of-s3'), ['Clients', 'tierstore', 'local cache', 'S3 store']);
  // The only dashed arrow is the copy from the hot tier to the cold one; a read from the cold tier is a request, so it is solid.
  assert.equal(dashed('directory-as-bucket'), 1);
  assert.equal(dashed('cache-hot-cold'), 1);
  assert.equal(dashed('cache-in-front-of-s3'), 0);
  for (const name of EXAMPLES) assert.doesNotMatch(read('docs', 'examples', name, 'index.html'), /freed|dg-ghost/, name);
});

test('a diagram sits in a box that scrolls sideways and can be focused; a card shows an icon of it, with boxes and arrows and no words', () => {
  const page = read('docs', 'examples', 'cache-hot-cold', 'index.html');
  assert.match(page, /<div class="diagram-scroll" role="group" aria-label="Diagram; on a narrow screen it scrolls sideways" tabindex="0">/);
  assert.match(page, /<p class="diagram-hint">Swipe sideways to see the whole picture\.<\/p>/);
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  assert.match(css, /\.diagram-hint \{ display: none;/);
  assert.match(css, /\.diagram-scroll \.diagram-svg \{ min-width: 28rem; \}/);
  const home = read('index.html');
  const icons = [...home.matchAll(/<figure class="diagram diagram-icon">([\s\S]*?)<\/figure>/g)].map((m) => m[1]);
  assert.equal(icons.length, 3);
  for (const i of icons) {
    assert.doesNotMatch(i, /<text/);
    assert.ok((i.match(/<g class="dg-node/g)?.length ?? 0) >= 3 && (i.match(/<g class="dg-edge/g)?.length ?? 0) >= 2);
  }
  const GHOSTED = ['alt: x', 'node a 0 1 "A" "a"', 'node b 1 1 "B" "b"', 'node g 2 0 "G" "g" ghost', 'edge a b "go"', 'edge b g "gone"'].join('\n');
  const icon = renderDiagram(parseDiagram(GHOSTED), false, true);
  assert.equal(icon.match(/<g class="dg-node/g)?.length, 2, 'a ghost box is left out of an icon');
  assert.equal(icon.match(/<g class="dg-edge/g)?.length, 1, 'and so is the arrow to it');
  assert.match(renderDiagram(parseDiagram(GHOSTED)), /<text class="dg-name"[^>]*>G<\/text>/, 'the full picture keeps it');
});

// ---- the landing page ----

test('the landing page starts with Configure tierstore: one smallest file, each key in a line, then the commands', () => {
  const html = read('docs', 'index.html');
  assert.match(html, /<h1>Configure tierstore<\/h1>/);
  const headings = [...html.matchAll(/<h2 id="([a-z-]+)"/g)].map((m) => m[1]);
  assert.deepEqual(headings, ['pick-a-shape', 'the-smallest-working-file', 'all-configuration-options', 'examples']);
  const first = html.slice(html.indexOf('id="the-smallest-working-file"'), html.indexOf('id="all-configuration-options"'));
  assert.equal(first.match(/<pre class="hl">/g)?.length, 3, 'the file, the commands to start it and the put and get');
  for (const key of ['interface.type', 'interface.params', 'server.proxy_id', 'server.bucket', 'server.access_key_file', 'hot_store.kind', 'hot_store.path', 'hot_store.budget', 'cold_store.kind', 'cold_store.path']) {
    assert.ok(first.includes(`<code>${key}</code>`), `${key} is not explained`);
  }
  const pick = html.slice(html.indexOf('id="pick-a-shape"'), html.indexOf('id="the-smallest-working-file"'));
  for (const name of EXAMPLES) assert.ok(pick.includes(`href="/docs/examples/${name}/"`), `${name} is not linked in Pick a shape`);
  assert.match(text(pick), /AWS S3 is not supported as a tier yet, and TLS to tiers is not built/);
  assert.doesNotMatch(text(first), /format-epoch|--activate/, 'the first start of a new hot tier writes its format itself');
});

test('every key of every table of the reference is on the landing page with its type, default and meaning, from configuration.md', () => {
  const config = readFileSync(join(DOCS, 'configuration.md'), 'utf8');
  const landing = read('docs', 'index.html');
  const tables = configTables(config);
  const rows = tables.split('\n').filter((l) => l.startsWith('| `'));
  assert.ok(rows.length >= 150, `only ${rows.length} rows of keys`);
  for (const title of TABLE_SECTIONS) assert.ok(tables.includes(`### ${title}\n`), title);
  for (const row of rows) {
    const key = /^\| `([^`]+)`/.exec(row)?.[1] ?? '';
    assert.ok(landing.includes(`<td><code>${key.replace(/&/g, '&amp;')}</code></td>`) || landing.includes(`<code>${key}</code>`), `${key} is not on the landing page`);
  }
  for (const id of ['interface', 'server', 'hot_store-and-cold_store', 'local_cache', 'policy', 'paramsupstream']) assert.ok(landing.includes(`<h3 id="${id}">`), id);
  // A default, and its meaning, as the reference gives them.
  assert.match(landing, /<td><code>hot_store\.high_water<\/code><\/td><td data-label="Type, default">percent, 80<\/td><td>above this share of the budget, eviction begins<\/td>/);
  assert.match(landing, /<td><code>policy\.cache<\/code><\/td><td data-label="Type, default">boolean, <code>false<\/code><\/td>/);
});

test('a change to a table of the reference changes the landing page, and a missing table is an error', () => {
  const config = readFileSync(join(DOCS, 'configuration.md'), 'utf8');
  const index = readFileSync(join(DOCS, 'index.md'), 'utf8');
  const changed = config.replace('| `hot_store.high_water` | percent, 80 |', '| `hot_store.high_water` | percent, 85 |');
  assert.notEqual(changed, config);
  assert.match(landingMarkdown(index, changed), /\| `hot_store\.high_water` \| percent, 85 \|/);
  assert.throws(() => configTables(config.replace('## `[server]`', '## `[servers]`')), /no table under "## `\[server\]`"/);
  assert.throws(() => landingMarkdown(index.replace('\n## Examples\n', '\n## Samples\n'), config), /no "## Examples" section/);
});

test('the landing page lists the examples as cards, each a link with its diagram, title and one line', () => {
  const html = read('docs', 'index.html');
  const cards = [...html.matchAll(/<li class="card"><a href="([^"]+)">([\s\S]*?)<\/a><\/li>/g)];
  assert.deepEqual(cards.map((c) => c[1]), EXAMPLES.map((n) => `/docs/examples/${n}/`));
  for (const c of cards) assert.ok(c[2].includes('<svg class="diagram-svg"') && !c[2].includes('<figcaption>') && /<h3>[^<]+<\/h3><p>[^<]+<\/p>/.test(c[2]), c[1]);
  assert.match(html, /<h2 id="examples">Examples<\/h2>\n<p>Three complete files/);
});

// ---- the Markdown and the files for programs ----

test('each page is served as the Markdown it is made from, with links that resolve from where it is and the diagram as text', () => {
  for (const d of docs) {
    const raw = read('docs', `${d.stem}.md`);
    assert.equal(raw, rawMarkdown(d), d.stem);
    assert.ok(!raw.includes('```diagram'), `${d.stem}: a diagram fence in the Markdown`);
  }
  const example = read('docs', 'examples', 'cache-hot-cold.md');
  assert.match(example, /^# A cache in front of a hot and a cold store\n/);
  assert.match(example, /^Diagram: Clients read and write through tierstore, which has three tiers in one process\./m);
  assert.match(example, /\]\(\.\.\/operations\.md#what-tierstore-requires-of-a-store\)/, 'a link to a page of docs/ goes one directory up');
  const landing = read('docs', 'index.md');
  assert.match(landing, /\]\(examples\/cache-hot-cold\.md\)/);
  assert.match(landing, /^### `\[server\]`$/m, 'the landing Markdown has the tables of the reference');
  assert.match(read('docs', 'examples', 'directory-as-bucket.md'), /\]\(cache-hot-cold\.md\)/, 'a link from an example to another example is a sibling');
});

test('every relative link of the served Markdown names a file that is served', () => {
  const have = new Set<string>();
  for (const d of docs) have.add(`docs/${d.stem}.md`);
  let named = 0;
  for (const d of docs) {
    const dir = d.stem.includes('/') ? 'docs/examples/' : 'docs/';
    for (const m of read('docs', `${d.stem}.md`).matchAll(/\]\(([^)\s:#]+\.md)(?:#[^)]*)?\)/g)) {
      const target = new URL(m[1], `https://x.example/${dir}`).pathname.slice(1);
      named++;
      // The metrics page is linked before it is a page in some trees; every other link must resolve.
      assert.ok(have.has(target), `${d.stem}.md links to ${m[1]}, which is not served (${target})`);
    }
  }
  assert.ok(named > 20, `only ${named} links were checked`);
});

test('llms.txt follows llmstxt.org: a title, a one-paragraph summary in a quote, sections of links, every doc and example as .md', () => {
  const llms = read('llms.txt');
  assert.equal(llms, llmsTxt(docs));
  const lines = llms.split('\n');
  assert.equal(lines[0], '# tierstore');
  assert.match(lines[2], /^> tierstore is an open source S3 server in front of a small, fast store and a large, cheap one\./);
  assert.deepEqual(lines.filter((l) => l.startsWith('## ')), ['## Docs', '## Examples', '## Optional']);
  const links = [...llms.matchAll(/^- \[([^\]]+)\]\((https:\/\/[^)]+)\): .+$/gm)].map((m) => m[2]);
  for (const d of docs) assert.ok(links.includes(`https://tierstore.dev/docs/${d.stem}.md`), `llms.txt does not link ${d.stem}`);
  const examples = llms.slice(llms.indexOf('## Examples'), llms.indexOf('## Optional'));
  assert.equal(examples.match(/^- \[/gm)?.length, 3);
  assert.ok(llms.includes('(https://komira.ai/research/tierstore)'));
  assert.ok(llms.endsWith('\n') && !llms.includes('\n\n\n'));
});

test('llms-full.txt has every page of the docs, in the order of the sidebar, each after a line that names its address', () => {
  const full = read('llms-full.txt');
  assert.equal(full, llmsFull(docs));
  let at = 0;
  for (const d of docs) {
    const marker = `<!-- https://tierstore.dev${d.stem === 'index' ? '/docs/' : `/docs/${d.stem}/`} (https://tierstore.dev/docs/${d.stem}.md) -->`;
    const found = full.indexOf(marker, at);
    assert.ok(found >= at, `${d.stem} is missing or out of order`);
    at = found;
    assert.ok(full.includes(`# ${d.title}\n`), d.title);
  }
  assert.ok(full.length > 100_000, `${full.length} bytes: the pages are in it whole`);
});
