// The top navigation, the footer and the pages that moved.
//
// Catches: a header that has more or fewer than the three links the site promises (Install, Docs, Why) or in another
// order, a Why link that is not the paper's address or that opens a new window, a footer that lost a page the
// navigation does not carry, and an old address (/get-started/, /getting-started/, /why/) that stops answering or no
// longer sends the reader on.

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

test('every page has exactly three links in the top navigation, in order: Install, Docs, Why', () => {
  const want = [['/install/', 'Install'], ['/docs/', 'Docs'], [WHITE_PAPER_URL, 'Why']];
  for (const p of PAGES) assert.deepEqual(links(navOf(page(p.route))), want, p.route);
  assert.deepEqual(links(navOf(page('/docs/examples/cache-hot-cold/'))), want);
  assert.equal(NAV.length, 3);
  for (const gone of ['Get Started', 'Technical Report']) assert.ok(!navOf(page('/')).includes(gone), `${gone} is back in the header`);
});

test('the Why link is an ordinary link: no new window, no script', () => {
  const nav = navOf(page('/'));
  assert.doesNotMatch(nav, /target=|rel=|onclick/);
});

test('Install is marked as the current page on its own page, Docs on every page of the docs, and nothing else is marked', () => {
  const current = (route: string): string[] => [...navOf(page(route)).matchAll(/<a href="([^"]*)" aria-current="page">/g)].map((m) => m[1]);
  assert.deepEqual(current('/install/'), ['/install/']);
  assert.deepEqual(current('/docs/'), ['/docs/']);
  assert.deepEqual(current('/docs/examples/directory-as-bucket/'), ['/docs/']);
  assert.deepEqual(current('/docs/keys/'), ['/docs/']);
  for (const route of ['/', '/demo/', '/404.html']) assert.deepEqual(current(route), [], route);
});

test('the footer carries what is not in the navigation, the releases, the source and the report', () => {
  const html = footer(WHITE_PAPER_URL);
  const hrefs = links(html).map(([href]) => href);
  for (const l of FOOTER_LINKS) assert.ok(hrefs.includes(l.href), `${l.label} is not in the footer`);
  assert.deepEqual(
    links(html).map(([, text]) => text),
    ['Demo', 'For programs: llms.txt', 'Releases', 'Source on GitHub', 'Technical report'],
  );
  assert.ok(hrefs.includes(WHITE_PAPER_URL));
});

test('the old addresses answer with a page that links to where the content went and sends a browser there', () => {
  for (const [route, target] of [['/get-started/', '/install/'], ['/getting-started/', '/install/'], ['/why/', WHITE_PAPER_URL]] as const) {
    const html = page(route);
    assert.ok(html.includes(`<meta http-equiv="refresh" content="0; url=${target}">`), route);
    assert.ok(html.includes('<meta name="robots" content="noindex">'), route);
    assert.ok(links(html).some(([href]) => href === target || href.startsWith(`${target}#`)), `${route} has a link to ${target}`);
  }
});

test('Install has the four install tabs, the Next link to the docs and the Verify section after them', () => {
  const html = page('/install/');
  const labels = [...(/<div class="tab-list"[^>]*>([\s\S]*?)<\/div>/.exec(html)?.[1] ?? '').matchAll(/role="tab"[^>]*>([^<]+)<\/button>/g)].map((m) => m[1]);
  assert.deepEqual(labels, ['Script', 'Homebrew', 'apt', 'Tarball']);
  assert.equal(html.match(/role="tabpanel"/g)?.length, 4);
  assert.match(html.replace(/<[^>]+>/g, ''), /curl -fsSL https:\/\/tierstore\.dev\/install\.sh \| sh/);
  const at = (needle: string): number => html.indexOf(needle);
  assert.ok(at('role="tabpanel"') < at('Next: ') && at('Next: ') < at('id="verify"'), 'the order is the tabs, Next, then Verify');
  assert.match(html, /<strong>Next: <a href="\/docs\/">configure it<\/a>/);
  for (const id of ['integrity', 'authenticity-gh', 'authenticity-cosign', 'offline']) assert.ok(html.includes(`id="${id}"`), `no #${id}`);
});
