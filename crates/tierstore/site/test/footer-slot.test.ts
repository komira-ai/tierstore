// The optional footer link.
//
// Catches: a link, a name or a placeholder for the optional footer document reaching a built page while WHITE_PAPER_URL
// is null, and a footer that stops showing the link once the URL is set. While it is null the site says nothing about
// it: no visible placeholder, no hidden element, no navigation entry.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { footer } from '../tools/layout';
import { NAV, WHITE_PAPER_URL } from '../tools/pages';
import { buildReal } from './support';

const dist = buildReal('footer-slot');

test('while WHITE_PAPER_URL is null, no built file mentions the footer document', () => {
  assert.equal(WHITE_PAPER_URL, null, 'the URL is set: also add a page that links to it, and update this test');
  const hits = listFiles(dist).filter((f) => /white[\s_-]*paper/i.test(readFileSync(join(dist, f), 'utf8')));
  assert.deepEqual(hits, []);
  assert.equal(NAV.some((n) => /paper/i.test(n.label + n.route)), false);
});

test('the footer has the link when there is a URL and not when there is none (the check can fail)', () => {
  assert.doesNotMatch(footer(null), /paper/i);
  assert.match(footer('https://example.org/paper.html'), /<a href="https:\/\/example\.org\/paper\.html">White paper<\/a>/);
});
