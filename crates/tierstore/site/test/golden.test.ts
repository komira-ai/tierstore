// Golden output: the built tree, without the pages rendered from the guide, is exactly the committed manifest (every file's path and sha256).
//
// Catches: any change to the output nobody meant. A template byte, a content word, a stylesheet, a compiler upgrade
// that changes the emitted JavaScript, a file that appears or disappears, an asset id that moves. The manifest is
// reviewed in the pull request that changes it (`npm run golden` rewrites it). Also catches a build that is not
// deterministic (two builds of the same inputs differ), which would make a deploy upload what did not change.

import assert from 'node:assert/strict';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { guideStems, manifestOf } from '../tools/manifest';
import { buildReal, copyTree, DOCS, plant, SITE } from './support';

const stems = guideStems(DOCS);
const golden = readFileSync(join(SITE, 'test', 'golden.manifest'), 'utf8');

test('the built tree matches the committed manifest', () => {
  const got = manifestOf(buildReal('golden-a'), stems);
  assert.equal(got, golden, 'the output changed: if that is meant, run `npm run golden` and commit test/golden.manifest');
});

test('two builds of the same inputs are byte for byte the same', () => {
  assert.equal(manifestOf(buildReal('golden-b'), stems), manifestOf(buildReal('golden-c'), stems));
});

test('the manifest sees a one byte change in a page and a missing file (the check can fail)', () => {
  const dist = buildReal('golden-d');
  const clean = manifestOf(dist, stems);
  const changed = copyTree(dist, 'golden-e');
  plant(changed, 'index.html', '<h1', '<h2');
  assert.notEqual(manifestOf(changed, stems), clean);
  const missing = copyTree(dist, 'golden-f');
  unlinkSync(join(missing, '404.html'));
  assert.notEqual(manifestOf(missing, stems), clean);
});

test('the manifest ignores the files made from the guide and sees every hand-made page', () => {
  const dist = buildReal('golden-g');
  const clean = manifestOf(dist, stems);
  const guide = copyTree(dist, 'golden-h');
  for (const stem of stems) plant(guide, stem === 'index' ? 'docs/index.html' : join('docs', stem, 'index.html'), '<h1', '<h2');
  for (const file of ['llms.txt', 'llms-full.txt', 'docs/overview.md', 'docs/examples/cache-hot-cold.md']) plant(guide, file, /^/, 'planted\n');
  assert.equal(manifestOf(guide, stems), clean, 'a change to a page rendered from the guide must not change the manifest (docs.test.ts holds those pages)');
  for (const page of ['index.html', 'install/index.html', 'get-started/index.html', 'why/index.html', 'demo/index.html', '404.html']) {
    const changed = copyTree(dist, 'golden-i');
    plant(changed, page, /<(h1|main)/, '<$1 data-planted');
    assert.notEqual(manifestOf(changed, stems), clean, `a change to ${page} must change the manifest`);
  }
  assert.ok(golden.includes('  install/index.html'), 'the committed manifest lacks the install page');
  assert.ok(!stems.some((s) => golden.includes(`  docs/${s}/index.html`) || golden.includes(`  docs/${s}.md`)), 'the committed manifest lists a file made from the guide');
  assert.ok(!golden.includes('  docs/index.html') && !golden.includes('  llms'), 'the committed manifest lists the docs landing page or an llms file');
});
