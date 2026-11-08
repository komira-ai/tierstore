// The top navigation, the footer and the pages that moved.
//
// Catches: a header that has more or fewer than the two links the site promises, a Technical Report link that is
// not the paper's address or that opens a new window, a footer that lost a page the navigation no longer carries,
// and an old address (/install/, /getting-started/) that stops answering or no longer points at Get started.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { footer } from '../tools/layout';
import { FOOTER_LINKS, NAV, outPath, PAGES, WHITE_PAPER_URL } from '../tools/pages';
import { buildReal } from './support';

const dist = buildReal('nav');
const page = (route: string): string => readFileSync(join(dist, outPath(route)), 'utf8');
const navOf = (html: string): string => /<nav class="site-nav"[\s\S]*?<\/nav>/.exec(html)?.[0] ?? '';
const links = (html: string): Array<[string, string]> => [...html.matchAll(/<a href="([^"]*)"[^>]*>([^<]*)<\/a>/g)].map((m) => [m[1], m[2]]);

test('the technical report is the paper on the main site', () => {
  assert.equal(WHITE_PAPER_URL, 'https://komira.ai/whitepapers/tierstore');
});

test('every page has exactly two links in the top navigation: Get Started and Technical Report', () => {
  for (const p of PAGES) {
    assert.deepEqual(links(navOf(page(p.route))), [['/get-started/', 'Get Started'], [WHITE_PAPER_URL, 'Technical Report']], p.route);
  }
  assert.equal(NAV.length, 2);
});

test('the technical report link is an ordinary link: no new window, no script', () => {
  const nav = navOf(page('/'));
  assert.doesNotMatch(nav, /target=|rel=|onclick/);
});

test('Get Started is marked as the current page on its own page and on no other', () => {
  for (const p of PAGES) {
    const current = /<a href="\/get-started\/" aria-current="page">/.test(navOf(page(p.route)));
    assert.equal(current, p.route === '/get-started/', p.route);
  }
});

test('the footer carries what left the navigation, the releases, the source and the report', () => {
  const html = footer(WHITE_PAPER_URL);
  const hrefs = links(html).map(([href]) => href);
  for (const l of FOOTER_LINKS) assert.ok(hrefs.includes(l.href), `${l.label} is not in the footer`);
  assert.deepEqual(
    links(html).map(([, text]) => text),
    ['Docs', 'Demo', 'Why tierstore', 'Releases', 'Source on GitHub', 'Technical report'],
  );
  assert.ok(hrefs.includes(WHITE_PAPER_URL));
});

test('the old install and getting-started addresses answer with a page that links to Get started and sends a browser there', () => {
  for (const [route, target] of [['/install/', '/get-started/#install'], ['/getting-started/', '/get-started/']] as const) {
    const html = page(route);
    assert.ok(html.includes(`<meta http-equiv="refresh" content="0; url=${target}">`), route);
    assert.ok(html.includes('<meta name="robots" content="noindex">'), route);
    assert.ok(links(html).some(([href]) => href.startsWith('/get-started/')), `${route} has a link to Get started`);
  }
});

test('Get started has the installer, the install steps, the configuration and the client snippets', () => {
  const html = page('/get-started/');
  for (const id of ['install', 'planned', 'installer', 'configure', 'run', 'clients']) assert.ok(html.includes(`id="${id}"`), `no #${id}`);
  assert.match(html.replace(/<[^>]+>/g, ''), /curl -fsSL https:\/\/tierstore\.dev\/install\.sh \| sh/);
  assert.match(html, /id="installer">One-line installer <span class="tag tag-planned">planned, not built<\/span>/);
});
