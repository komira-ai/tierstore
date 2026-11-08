// The home page's picture of requests through tierstore, and the scroll reveal.
//
// Catches: a picture that needs a script or an inline style, that has no text alternative, that keeps moving under
// prefers-reduced-motion, that has no still frame, that can shift the layout, or whose keyframes are missing; a
// reveal that hides content when scripts are off or motion is reduced.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';
import { assetDir, buildReal, SITE } from './support';

const dist = buildReal('hero');
const home = readFileSync(join(dist, 'index.html'), 'utf8');
const css = (name: string): string => readFileSync(join(dist, assetDir(dist), name), 'utf8');
const site = css('site.css');
const theme = css('theme.css');

test('the picture is inline SVG with a title and a description, and no script, style attribute or event handler', () => {
  const svg = /<svg class="flow-svg"[\s\S]*?<\/svg>/.exec(home)?.[0] ?? '';
  assert.match(svg, /role="img" aria-labelledby="flow-title flow-desc"/);
  assert.match(svg, /<title id="flow-title">[^<]+<\/title>/);
  assert.match(svg, /<desc id="flow-desc">[^<]+<\/desc>/);
  assert.doesNotMatch(svg, /\sstyle=|\son[a-z]+=|<script|<animate|<set /);
});

test('the picture shows the three things tierstore does: a hit, a miss read from the cold store, a promotion into the hot tier', () => {
  for (const cls of ['dot-hit', 'dot-miss', 'dot-promote', 'lbl-hit', 'lbl-miss', 'lbl-promote']) assert.ok(home.includes(cls), cls);
  for (const text of ['hot tier', 'cold store', 'S3 endpoint', 'tierstore']) assert.ok(home.includes(`>${text}</text>`), text);
});

test('the picture keeps its space before it loads: width and height attributes and a width of 100% with automatic height', () => {
  assert.match(home, /<svg class="flow-svg" viewBox="0 0 640 400" width="640" height="400"/);
  assert.match(site, /\.flow-svg \{[^}]*width: 100%; height: auto;/);
});

test('every animation the picture names has keyframes, and they run only when motion is welcome', () => {
  const names = ['flow-hit', 'flow-miss', 'flow-promote', 'lbl-hit', 'lbl-miss', 'lbl-promote', 'obj-in', 'obj-pulse'];
  const motion = /@media \(prefers-reduced-motion: no-preference\) \{[\s\S]*?\n\}/.exec(site)?.[0] ?? '';
  for (const n of names) {
    assert.match(site, new RegExp(`@keyframes ${n} \\{`), `${n} has no keyframes`);
    assert.ok(motion.includes(`animation-name: ${n}`), `${n} is not run inside the no-preference block`);
  }
  assert.equal(/animation-name/.test(site.replace(motion, '')), false, 'an animation outside the no-preference block');
});

test('with reduced motion the still frame has its dots in place and every label visible', () => {
  assert.match(site, /\.dot-hit \{ transform: translate\(470px, 88px\)/);
  assert.match(site, /\.dot-miss \{ transform: translate\(470px, 312px\)/);
  assert.match(site, /\.dot-promote \{ transform: translate\(630px, 200px\)/);
  assert.equal(/\.lbl[^{]*\{[^}]*opacity: 0/.test(site.replace(/@keyframes[\s\S]*?\n\}/g, '')), false, 'a label is hidden in the still frame');
  assert.match(theme, /@media \(prefers-reduced-motion: reduce\) \{\s*\*, \*::before, \*::after \{ animation: none !important/);
});

test('the keyframes of komira.ai are in the theme', () => {
  for (const k of ['k-fade', 'k-rise', 'k-spin', 'k-grow']) assert.match(theme, new RegExp(`@keyframes ${k} \\{`), k);
});

test('nothing is hidden when scripts are off: the reveal rule needs .js and .is-pre, which only the script adds', () => {
  assert.match(theme, /\.js \.k-rv\.is-pre \{ opacity: 0;/);
  assert.equal(/class="[^"]*is-pre/.test(home), false);
  assert.equal(/<html[^>]*class=/.test(home), false);
});

// reveal.js run against a stand-in page: the compiled module with its `export` words taken off.
const REVEAL = readFileSync(join(SITE, 'build', 'browser', 'reveal.js'), 'utf8').replace(/^export /gm, '');

interface Stub {
  added: string[];
  classes: Map<string, Set<string>>;
  observed: string[];
  fire: (id: string) => void;
}

function run(opts: { reduced?: boolean; observer?: boolean; tops?: Record<string, number> }): Stub {
  const stub: Stub = { added: [], classes: new Map(), observed: [], fire: () => {} };
  const tops = opts.tops ?? { above: 10, below: 2000 };
  const elements = Object.keys(tops).map((id) => {
    const set = new Set<string>();
    stub.classes.set(id, set);
    return { id, getBoundingClientRect: () => ({ top: tops[id] }), classList: { add: (c: string) => set.add(c), remove: (c: string) => set.delete(c) } };
  });
  let callback: (entries: Array<{ isIntersecting: boolean; target: unknown }>) => void = () => {};
  class IO {
    constructor(cb: typeof callback) {
      callback = cb;
    }
    observe(el: { id: string }): void {
      stub.observed.push(el.id);
    }
    unobserve(): void {}
  }
  stub.fire = (id) => callback([{ isIntersecting: true, target: elements.find((e) => e.id === id) }]);
  const win: Record<string, unknown> = { innerHeight: 800, matchMedia: () => ({ matches: opts.reduced === true }) };
  if (opts.observer !== false) win.IntersectionObserver = IO;
  const doc = { documentElement: { classList: { add: (c: string) => stub.added.push(c) } }, querySelectorAll: () => elements };
  runInNewContext(REVEAL, { document: doc, window: win, IntersectionObserver: IO });
  return stub;
}

test('reveal: a block below the first screen is hidden until it scrolls in, and one above it is never hidden', () => {
  const s = run({});
  assert.deepEqual(s.added, ['js']);
  assert.deepEqual(s.observed, ['below']);
  assert.equal(s.classes.get('above')?.has('is-pre'), false);
  assert.equal(s.classes.get('below')?.has('is-pre'), true);
  s.fire('below');
  assert.equal(s.classes.get('below')?.has('is-pre'), false);
});

test('reveal: with reduced motion, or no IntersectionObserver, the page is left alone', () => {
  for (const o of [{ reduced: true }, { observer: false }]) {
    const s = run(o);
    assert.deepEqual(s.added, []);
    assert.deepEqual(s.observed, []);
    assert.equal(s.classes.get('below')?.has('is-pre'), false);
  }
});
