// The home page and the Install page read as a newcomer's pages: a hero with a headline, a pitch, two buttons (Install, Read the docs)
// and the picture; an Examples band with three cards; plain words; one command block per install method, the long
// verification under Verify.
//
// Catches: an install block or a row of pills back in the hero; the old buttons; jargon of the code (entry format, marker format,
// reconciler, lease, water marks) in the home copy; a claim that the hot tier is an LRU; an Install page with more than one command
// block in a tab, or a method not labelled with when it is available; a tarball command that is for one platform only; prose
// wider than 640px.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { assetDir, buildReal } from './support';

const dist = buildReal('pages');
const home = readFileSync(join(dist, 'index.html'), 'utf8');
const install = readFileSync(join(dist, 'install', 'index.html'), 'utf8');
const text = (html: string): string => html.replace(/<[^>]+>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
const hero = /<section class="hero">[\s\S]*?<\/section>/.exec(home)?.[0] ?? '';

test('the hero holds a headline, one paragraph, two buttons (Install, Read the docs) and the picture, and no install block or pills', () => {
  assert.equal(hero.match(/<h1\b/g)?.length, 1);
  assert.equal(hero.match(/<p class="lede/g)?.length, 1);
  assert.deepEqual([...hero.matchAll(/<a class="(button[^"]*)" href="([^"]*)">([^<]+)<\/a>/g)].map((m) => [m[1], m[2], m[3]]), [['button button-primary', '/install/', 'Install'], ['button', '/docs/', 'Read the docs']]);
  assert.ok(hero.includes('class="flow-svg"'));
  assert.doesNotMatch(hero, /install-block|class="pill"|class="tag|<commit>|&lt;commit&gt;|class="cmd/);
});

test('the home page is in plain words: none of the code\'s own terms for how a write or a drain is kept, and the hot tier is not called an LRU', () => {
  const words = text(home);
  for (const jargon of ['entry format', 'marker format', 'reconciler', 'lease', 'water-mark', 'water mark', 'promote', 'miss\\b', 'tombstone']) {
    assert.ok(!new RegExp(`\\b${jargon}`, 'i').test(words), `the home page says "${jargon}"`);
  }
  // (The Examples band has a cache that is an LRU, as the cards say; the claim is about the rest of the page.)
  const rest = text(home.replace(/<section class="band" id="examples">[\s\S]*?<\/section>/, ''));
  assert.doesNotMatch(rest, /\bLRU\b|least recently/i, 'the hot tier evicts the oldest written, it is not an LRU');
  assert.match(words, /a build node, file only/);
  assert.match(words, /written longest ago/);
});

test('the Examples band of the home page has three cards, each a link to its example with its picture', () => {
  const band = /<section class="band" id="examples">[\s\S]*?<\/section>/.exec(home)?.[0] ?? '';
  const cards = [...band.matchAll(/<li class="card"><a href="([^"]+)">([\s\S]*?)<\/a><\/li>/g)];
  assert.deepEqual(cards.map((c) => c[1]), ['/docs/examples/directory-as-bucket/', '/docs/examples/cache-in-front-of-s3/', '/docs/examples/cache-hot-cold/']);
  for (const c of cards) assert.ok(c[2].includes('<svg class="diagram-svg"') && /<h3>[^<]+<\/h3>/.test(c[2]), c[1]);
  assert.ok(!home.includes('{{example-cards}}'), 'the placeholder reached the page');
});

test('Install is short: four tabs with one command block each, Next, then Verify; no client snippets, no configuration', () => {
  assert.deepEqual([...install.matchAll(/<h2 id="([a-z-]+)"/g)].map((m) => m[1]), ['verify']);
  const panels = [...install.matchAll(/<div class="tab-panel"[^>]*>([\s\S]*?)\n<\/div>\n(?=<div class="tab-panel"|<\/div>)/g)];
  assert.equal(panels.length, 4);
  for (const p of panels) assert.equal(p[1].match(/<div class="code">/g)?.length, 1, 'one command block in a tab');
  assert.doesNotMatch(install, /\[\[interface\]\]|aws-sdk|boto3|forcePathStyle/);
});

test('the tarball is the first tab and says it works today; every method that is not in a release yet says so in its tab and next to its command', () => {
  const labels = [...install.matchAll(/role="tab"[^>]*>([^<]+)<\/button>/g)].map((m) => m[1]);
  assert.deepEqual(labels, ['Tarball · works today', 'Script · next release', 'Homebrew · next release', 'apt · next release']);
  const t = text(install);
  const label = (needle: string): string => { const at = t.indexOf(needle); assert.ok(at > 0, needle); return t.slice(Math.max(0, at - 700), at); };
  for (const cmd of ['curl -fsSL https://tierstore.dev/install.sh | sh', 'brew install komira-ai/tap/tierstore', 'sudo apt install tierstore']) {
    assert.match(label(cmd), /Not in a release yet/, cmd);
  }
  assert.match(label('gh release download'), /works today/);
  assert.match(t, /Signed-By: \/usr\/share\/keyrings\/komira-archive-keyring\.gpg/);
  assert.match(t, /https:\/\/packages\.komira\.ai\/apt\/komira-archive-keyring\.gpg/);
});

test('every install block is followed by a version check, and the page says which tabs Verify is for', () => {
  const panels = [...install.matchAll(/<div class="tab-panel"[^>]*>([\s\S]*?)\n<\/div>\n(?=<div class="tab-panel"|<\/div>)/g)].map((m) => text(m[1]));
  assert.equal(panels.length, 4);
  for (const p of panels) assert.match(p, /tierstore --version/);
  assert.match(text(install), /Verify the tarball[\s\S]*This is for the Tarball tab only\. The script checks its own download, and Homebrew and apt install from their own repositories/);
  assert.deepEqual([...install.matchAll(/<h[34] id="([a-z-]+)"/g)].map((m) => m[1]).slice(0, 2), ['check-it', 'more-ways']);
});

test('the tarball block takes the platform of the machine it runs on: Linux or macOS, x86_64 or arm64, with its bundle and SHA256SUMS', () => {
  const all = text(install);
  const cmd = all.slice(all.indexOf('os=$(uname'), all.indexOf('./tierstore --version') + 30);
  assert.match(cmd, /uname -s \| sed 's\/Darwin\/macos\/; s\/Linux\/linux\/'/);
  assert.match(cmd, /uname -m \| sed 's\/aarch64\/arm64\/'/);
  assert.match(cmd, /--pattern "tierstore-\*-\$os-\$arch\.tar\.gz\*" --pattern SHA256SUMS/);
  assert.match(cmd, /\.\/tierstore --version/);
});

test('prose is 640px wide (40rem) and the page is 70rem', () => {
  const css = readFileSync(join(dist, assetDir(dist), 'site.css'), 'utf8');
  assert.match(css, /:root \{ --measure: 40rem; --wrap: 70rem; \}/);
  assert.match(css, /\.narrow > p, \.narrow > ul \{ max-width: var\(--measure\); \}/);
});
