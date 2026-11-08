// The home page and Get started read as a newcomer's pages: a hero with a headline, a pitch, two buttons and the picture; plain
// words; a short install block per method; the long verification in the install guide.
//
// Catches: an install block or a row of pills back in the hero; jargon of the code (entry format, marker format, reconciler,
// lease, water marks) in the home copy; a Get started page that repeats the verification, the release-history note or the
// naming of <commit> that the install guide holds; a tarball command that is for one platform only; prose wider than 640px.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { assetDir, buildReal } from './support';

const dist = buildReal('pages');
const home = readFileSync(join(dist, 'index.html'), 'utf8');
const started = readFileSync(join(dist, 'get-started', 'index.html'), 'utf8');
const text = (html: string): string => html.replace(/<[^>]+>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
const hero = /<section class="hero">[\s\S]*?<\/section>/.exec(home)?.[0] ?? '';

test('the hero holds a headline, one paragraph, two buttons and the picture, and no install block or pills', () => {
  assert.equal(hero.match(/<h1\b/g)?.length, 1);
  assert.equal(hero.match(/<p class="lede/g)?.length, 1);
  assert.deepEqual([...hero.matchAll(/<a class="(button[^"]*)"[^>]*>([^<]+)<\/a>/g)].map((m) => [m[1], m[2]]), [['button button-primary', 'Get started'], ['button', 'See a start and a drain']]);
  assert.ok(hero.includes('class="flow-svg"'));
  assert.doesNotMatch(hero, /install-block|class="pill"|class="tag|<commit>|&lt;commit&gt;|class="cmd/);
});

test('the home page is in plain words: none of the code\'s own terms for how a write or a drain is kept', () => {
  const words = text(home);
  for (const jargon of ['entry format', 'marker format', 'reconciler', 'lease', 'water-mark', 'water mark', 'promote', 'miss\\b', 'tombstone']) {
    assert.ok(!new RegExp(`\\b${jargon}`, 'i').test(words), `the home page says "${jargon}"`);
  }
  assert.match(words, /a build node, file only/);
});

test('Get started is short: install tabs, configure, run, clients, and a link to the install guide for the checks', () => {
  assert.deepEqual([...started.matchAll(/<h2 id="([a-z-]+)"/g)].map((m) => m[1]), ['install', 'configure', 'run', 'clients']);
  assert.doesNotMatch(started, /id="planned"|id="release-files"|Where the code lives|full 40-hex|sigstore|SHA256SUMS/);
  assert.match(started, /<a href="\/docs\/install\/#verify">Verify the download<\/a>/);
});

test('the tarball block takes the platform of the machine it runs on: Linux or macOS, x86_64 or arm64', () => {
  const block = /<div class="code"><pre class="hl">([\s\S]*?)<\/pre>/.exec(started)?.[1] ?? '';
  const cmd = text(block);
  assert.match(cmd, /uname -s \| sed 's\/Darwin\/macos\/; s\/Linux\/linux\/'/);
  assert.match(cmd, /uname -m \| sed 's\/aarch64\/arm64\/'/);
  assert.match(cmd, /--pattern "tierstore-\*-\$os-\$arch\.tar\.gz"/);
  assert.match(cmd, /\.\/tierstore --version/);
});

test('every planned install method is marked coming soon next to its one command', () => {
  for (const cmd of ['curl -fsSL https://tierstore.dev/install.sh | sh', 'brew install komira-ai/tap/tierstore', 'sudo apt install tierstore']) {
    const at = text(started).indexOf(cmd);
    assert.ok(at > 0, cmd);
    assert.match(text(started).slice(Math.max(0, at - 260), at), /Coming soon/, cmd);
  }
});

test('prose is 640px wide (40rem) and the page is 70rem', () => {
  const css = readFileSync(join(dist, assetDir(dist), 'site.css'), 'utf8');
  assert.match(css, /:root \{ --measure: 40rem; --wrap: 70rem; \}/);
  assert.match(css, /\.narrow > p, \.narrow > ul \{ max-width: var\(--measure\); \}/);
});
