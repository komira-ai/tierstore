// The docs pages are the operator guide (crates/tierstore/docs/*.md), rendered by tools/docs.ts.
//
// Catches: a page of the guide that does not reach the site (or one that is there without a file behind it), a link to
// another page of the guide that is dead, markup the renderer does not read that would be shown wrongly, text that is
// not escaped, and a change to the guide that does not change the page. The statements of the guide are held to the
// code by a test of the crate (src/server/docs_tests.rs); this holds the site to the guide.

import assert from 'node:assert/strict';
import { cpSync, existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { test } from 'node:test';
import { buildSite } from '../tools/build';
import { listFiles } from '../tools/check-dist';
import { loadDocs, renderMarkdown, slug } from '../tools/docs';
import { guideStems } from '../tools/manifest';
import { buildReal, copyTree, DOCS, REPO, scratch, SITE } from './support';

const stems = guideStems(DOCS);
/** The built page of a stem: the landing page is /docs/ itself. */
const pageOf = (stem: string): string => (stem === 'index' ? 'docs/index.html' : `docs/${stem}/index.html`);
const dist = buildReal('docs');
const built = (route: string): string => readFileSync(join(dist, route.replace(/^\//, ''), 'index.html'), 'utf8');

/** A build of the real site with `docs` as the guide. */
function buildWith(name: string, docs: string): string {
  const out = join(scratch(name), 'dist');
  buildSite({ browserDir: join(SITE, 'build', 'browser'), staticDir: join(SITE, 'static'), contentDir: join(SITE, 'content'), docsDir: docs, outDir: out, check: true });
  return out;
}

/** A copy of the guide to change. */
function copyDocs(name: string): string {
  const dir = join(scratch(name), 'docs');
  cpSync(DOCS, dir, { recursive: true });
  return dir;
}

/** What is wrong with the files under docs/ of a built tree: exactly the page and the Markdown of each guide file. */
function docsTreeProblems(tree: string): string[] {
  const want = new Set([...stems.map(pageOf), ...stems.map((s) => `docs/${s}.md`)]);
  const have = new Set(listFiles(tree).filter((f) => f.startsWith('docs/')));
  return [...[...want].filter((f) => !have.has(f)).map((f) => `missing ${f}`), ...[...have].filter((f) => !want.has(f)).map((f) => `unexpected ${f}`)];
}

test('every file of docs/ and docs/examples/ is a page of the site and has its Markdown beside it, and every page is in the sidebar of every other', () => {
  assert.ok(stems.length >= 12, `the guide has ${stems.length} pages`);
  assert.ok(stems.includes('index') && stems.includes('examples/cache-hot-cold'));
  const nav = (html: string): string => /<nav class="docs-nav"[\s\S]*?<\/nav>/.exec(html)?.[0] ?? '';
  for (const stem of stems) {
    assert.ok(existsSync(join(dist, pageOf(stem))), `docs/${stem}.md has no page`);
    assert.ok(existsSync(join(dist, 'docs', `${stem}.md`)), `docs/${stem}.md is not served as Markdown`);
    assert.ok(nav(readFileSync(join(dist, pageOf('overview')), 'utf8')).includes(`href="${stem === 'index' ? '/docs/' : `/docs/${stem}/`}"`), `the sidebar does not list ${stem}`);
  }
  assert.deepEqual(docsTreeProblems(dist), [], 'the built docs/ is a page and a Markdown file per guide file, nothing else');
});

test('an extra docs directory or a stray file under docs/ is refused (the check can fail)', () => {
  const extraDir = copyTree(dist, 'docs-extra-dir');
  mkdirSync(join(extraDir, 'docs', 'extra'));
  writeFileSync(join(extraDir, 'docs', 'extra', 'index.html'), '<p>x</p>');
  assert.deepEqual(docsTreeProblems(extraDir), ['unexpected docs/extra/index.html']);
  for (const stray of ['x.js', join('overview', 'extra.json')]) {
    const tree = copyTree(dist, 'docs-stray');
    writeFileSync(join(tree, 'docs', stray), '{}');
    assert.deepEqual(docsTreeProblems(tree), [`unexpected docs/${stray}`]);
  }
  const gone = copyTree(dist, 'docs-gone');
  unlinkSync(join(gone, 'docs', 'keys', 'index.html'));
  unlinkSync(join(gone, 'docs', 'examples', 'cache-hot-cold.md'));
  assert.deepEqual(docsTreeProblems(gone), ['missing docs/keys/index.html', 'missing docs/examples/cache-hot-cold.md']);
});

test('the guide is read in its order (the landing page, the reference, the examples, the guide), and a page the site has not heard of follows by name', () => {
  const order = loadDocs(DOCS).map((d) => d.stem);
  assert.deepEqual(order.slice(0, 7), ['index', 'configuration', 'examples/directory-as-bucket', 'examples/cache-in-front-of-s3', 'examples/cache-hot-cold', 'overview', 'configure']);
  const dir = copyDocs('docs-order');
  writeFileSync(join(dir, 'zzz.md'), '# A page\n\nText.\n');
  const after = loadDocs(dir).map((d) => d.stem);
  assert.equal(after[after.length - 1], 'zzz');
});

test('a page added to docs/ is built, and a change to the guide changes its page', () => {
  const dir = copyDocs('docs-add');
  writeFileSync(join(dir, 'metrics.md'), '# Metrics\n\nThe counters.\n\n## Names\n\nA name.\n');
  const out = buildWith('docs-add-out', dir);
  assert.match(readFileSync(join(out, 'docs', 'metrics', 'index.html'), 'utf8'), /<h2 id="names">Names<\/h2>/);
  assert.match(readFileSync(join(out, 'docs', 'overview', 'index.html'), 'utf8'), /href="\/docs\/metrics\/"/, 'the sidebar lists the new page');
  const text = readFileSync(join(dir, 'limits.md'), 'utf8');
  writeFileSync(join(dir, 'limits.md'), text.replace('One bucket, SigV4 in the header, virtual-hosted-style only for the domains you list.', 'A planted sentence.'));
  assert.match(readFileSync(join(buildWith('docs-change', dir), 'docs', 'limits', 'index.html'), 'utf8'), /A planted sentence\./);
});

test('a link to a page of the guide is a link when the page is there and its label when it is not (the metrics page, until it lands)', () => {
  const md = 'See [the metrics](metrics.md#names) and [keys](keys.md#rotation).\n';
  const without = renderMarkdown(`# T\n\n${md}`, new Set(['keys']));
  assert.match(without.body, /See the metrics and <a href="\/docs\/keys\/#rotation">keys<\/a>\./);
  const withIt = renderMarkdown(`# T\n\n${md}`, new Set(['keys', 'metrics']));
  assert.match(withIt.body, /<a href="\/docs\/metrics\/#names">the metrics<\/a>/);
});

test('text is escaped, in prose, code, tables and fences; a quote in a link target cannot leave its attribute', () => {
  const md = ['# T <x>', '', 'a <commit> & "q" `<b>` **<i>**', '', '| a | b |', '|---|---|', '| `<c>` | <d> |', '', '```text', '<pre> & "x"', '```', ''].join('\n');
  const { body } = renderMarkdown(md, new Set());
  assert.match(body, /<h1>T &lt;x&gt;<\/h1>/);
  assert.match(body, /a &lt;commit&gt; &amp; &quot;q&quot; <code>&lt;b&gt;<\/code> <strong>&lt;i&gt;<\/strong>/);
  assert.match(body, /<td><code>&lt;c&gt;<\/code><\/td><td>&lt;d&gt;<\/td>/);
  assert.match(body, /<pre>&lt;pre&gt; &amp; &quot;x&quot;<\/pre>/);
  const quoted = renderMarkdown('# T\n\n[x](https://a.example/"onclick=1)\n', new Set()).body;
  assert.match(quoted, /<a href="https:\/\/a\.example\/&quot;onclick=1">x<\/a>/);
  assert.equal(/<(?!\/?(?:h1|p|code|strong|div|table|thead|tbody|tr|th|td|pre)\b)/.test(body), false, body);
});

test('markup the renderer does not read is an error, not a page that is quietly wrong', () => {
  assert.throws(() => renderMarkdown('# T\n\n  - nested\n', new Set()), /indented/);
  assert.throws(() => renderMarkdown('# T\n\n[x](http://insecure.example/)\n', new Set()), /only https links/);
  assert.throws(() => renderMarkdown('# T\n\n[x](nope.txt)\n', new Set()), /only https links/);
  assert.throws(() => renderMarkdown('# T\n\n```sh\nunclosed\n', new Set()), /not closed/);
  assert.throws(() => renderMarkdown('# T\n\n| a | b |\n| c | d |\n', new Set()), /separator/);
  assert.throws(() => renderMarkdown('# T\n\n| a | b |\n|---|---|\n| c |\n', new Set()), /cells/);
  assert.throws(() => renderMarkdown('Text\n', new Set()), /no # heading/);
  assert.throws(() => renderMarkdown('# T\n\n# U\n', new Set()), /second # heading/);
  assert.throws(() => renderMarkdown('# T\n\nan **bold\n', new Set()), /does not read/);
});

test('headings get the anchors the test of the crate computes, repeats are numbered, and a "Not in a release yet" section is marked', () => {
  assert.equal(slug('What tierstore requires of a store'), 'what-tierstore-requires-of-a-store');
  assert.equal(slug('`[hot_store]` and `[cold_store]`'), 'hot_store-and-cold_store');
  const { body } = renderMarkdown('# T\n\n## Not in a release yet\n\nx\n\n## Not in a release yet\n\ny\n', new Set());
  assert.match(body, /<h2 id="not-in-a-release-yet">Not in a release yet <span class="tag tag-planned">planned, not built<\/span><\/h2>/);
  assert.match(body, /<h2 id="not-in-a-release-yet-2">/);
  assert.match(built('/docs/configuration/'), /<h2 id="hot_store-and-cold_store">/);
  assert.match(built('/docs/operations/'), /<h2 id="what-tierstore-requires-of-a-store">/);
});

test('the first paragraph is the lede, and the page names the guide in its navigation with the current page marked', () => {
  const page = built('/docs/keys/');
  assert.match(page, /<h1>Keys and roles<\/h1>\n<p class="lede">/);
  assert.match(page, /<a href="\/docs\/keys\/" aria-current="page">Keys and roles<\/a>/);
  assert.match(page, /<a href="\/docs\/" aria-current="page">Docs<\/a>/, 'the header marks Docs as the current section');
});

test('the workflow runs the site tests when the guide or the crate source changes, on a pull request and on a push', () => {
  const wf = readFileSync(join(REPO, '.github', 'workflows', 'tierstore-site.yml'), 'utf8');
  const docs = relative(REPO, DOCS).split(sep).join('/');
  // test/claims.test.ts reads the crate's src/ (the directory beside docs/): a code change that makes a page wrong must fail on its own pull request.
  const src = `${docs.replace(/\/docs$/, '')}/src`;
  for (const dir of [docs, src]) {
    const glob = `      - ${dir}/**\n`;
    assert.equal(wf.split(glob).length - 1, 2, `the workflow's paths: filters (pull_request and push) each name ${glob.trim()}`);
  }
});

test('on a phone the guide menu is a disclosure that the page script closes, and with no script it is the open list; a wide screen shows the list with no summary', () => {
  for (const route of ['/docs/', '/docs/keys/', '/docs/examples/cache-hot-cold/']) {
    const html = built(route);
    assert.match(html, /<details class="docs-menu" open>\n<summary>Docs menu<\/summary>\n<nav class="docs-nav" aria-label="Guide">/, route);
    assert.match(html, /\/docs-menu\.js"><\/script>/, route);
    assert.ok(html.includes('<p class="docs-nav-title">Configure</p>') && html.includes('<p class="docs-nav-title">Examples</p>') && html.includes('<p class="docs-nav-title">More guides</p>'), route);
  }
  const js = readFileSync(join(SITE, 'build', 'browser', 'docs-menu.js'), 'utf8');
  assert.match(js, /\(min-width: 60rem\)/);
  assert.match(js, /menu\.open = wide\.matches/);
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  assert.match(css, /@media \(min-width: 60rem\) \{[^@]*\.docs-menu summary \{ display: none; \}/);
});

test('a table of the guide labels its middle columns, and on a phone its rows are stacked with the label before each cell', () => {
  const html = built('/docs/');
  assert.match(html, /<td data-label="Type, default">/);
  assert.doesNotMatch(html, /<td data-label="What">/, 'the last column is the text and has no label');
  assert.match(html, /<tr><td><code>hot_store\.kind<\/code><\/td><td>the kind of the hot tier/, 'a two-column table has no labels');
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  assert.match(css, /\.docs-body \.table-wrap td\[data-label\]::before \{ content: attr\(data-label\) ": "/);
  assert.match(css, /\.docs-body \.table-wrap tr, \.docs-body \.table-wrap td \{ display: block; \}/);
});

test('a code block wraps its long lines and keeps its text clear of the Copy button: on a wide screen by padding, on a phone by a strip of its own', () => {
  const css = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  assert.match(css, /\.code pre \{[^}]*padding: 1rem 5\.5rem 1rem 1rem;[^}]*white-space: pre-wrap; overflow-wrap: anywhere;/);
  assert.match(css, /@media \(max-width: 40rem\) \{[^@]*\.code pre \{ padding: 2\.9rem 1rem 1rem; \}/);
  assert.match(css, /\.code button\.copy \{ position: absolute; top: 0\.5rem; right: 0\.5rem;/);
});
