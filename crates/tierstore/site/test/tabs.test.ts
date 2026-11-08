// The tabs: the markup the build writes and what src/tab-logic.ts does with it.
//
// Catches: a tab that is not a real button or lacks its role, selected state or panel link; a panel that is hidden or
// missing when the script does not run; arrow keys that do not move the selection (or do not wrap); a remembered tab that
// is applied when it names no tab, or a storage that throws and takes the tabs down with it; and a Get started page that
// lost one of its six client languages or four install methods.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { enhance, nextIndex, recall, remember, storageOf } from '../src/tab-logic';
import { MAX_TABS, renderTabs, slug } from '../tools/tabs';
import { buildReal } from './support';

const group = (labels: string[], name = 'client'): string =>
  `<div class="tabs" data-tabs="${name}">\n${labels.map((l) => `<section data-tab="${l}"><p>${l} body</p></section>`).join('\n')}\n</div>`;

test('markup: a tablist of buttons with role=tab, and a tabpanel for each, linked both ways', () => {
  const out = renderTabs(group(['Rust (aws-sdk-s3)', 'Python (boto3)']));
  assert.ok(out.includes('<div class="tab-list" role="tablist" aria-label="client" hidden>'));
  assert.ok(out.includes('<button type="button" role="tab" id="tab-client-rust-aws-sdk-s3" aria-controls="panel-client-rust-aws-sdk-s3" aria-selected="true" tabindex="0" data-tab="rust-aws-sdk-s3">Rust (aws-sdk-s3)</button>'));
  assert.ok(out.includes('<button type="button" role="tab" id="tab-client-python-boto3" aria-controls="panel-client-python-boto3" aria-selected="false" tabindex="-1" data-tab="python-boto3">Python (boto3)</button>'));
  assert.ok(out.includes('<div class="tab-panel" role="tabpanel" id="panel-client-python-boto3" aria-labelledby="tab-client-python-boto3" tabindex="0">'));
  assert.equal(out.match(/role="tabpanel"/g)?.length, 2);
});

test('markup: with no script every panel is in the page, shown, under its own title', () => {
  const out = renderTabs(group(['A', 'B', 'C']));
  assert.equal(out.match(/class="tab-title"/g)?.length, 3);
  assert.ok(!/class="tab-panel"[^>]* hidden/.test(out) && !/role="tabpanel"[^>]* hidden/.test(out), 'a panel is hidden in the markup');
  for (const l of ['A', 'B', 'C']) assert.ok(out.includes(`<p>${l} body</p>`));
});

test('markup: too few tabs, too many, or labels that make one id are refused', () => {
  assert.throws(() => renderTabs(group(['one'])), /1 tabs/);
  assert.throws(() => renderTabs(group(Array.from({ length: MAX_TABS + 1 }, (_, i) => `t${i}`))), /not 2 to/);
  assert.doesNotThrow(() => renderTabs(group(Array.from({ length: MAX_TABS }, (_, i) => `t${i}`))));
  assert.throws(() => renderTabs(group(['Go!', 'go'])), /one id twice/);
  assert.throws(() => renderTabs(group(['!!', 'ok'])), /empty/);
});

test('slug: lower case, runs of other characters become one dash, none at the ends', () => {
  assert.equal(slug('Python (boto3)'), 'python-boto3');
  assert.equal(slug('macOS (Apple silicon)'), 'macos-apple-silicon');
  assert.equal(slug('  AWS CLI '), 'aws-cli');
});

test('keys: arrows, Home and End move the selection, and the ends wrap; other keys are left alone', () => {
  assert.equal(nextIndex('ArrowRight', 0, 4), 1);
  assert.equal(nextIndex('ArrowDown', 3, 4), 0);
  assert.equal(nextIndex('ArrowLeft', 2, 4), 1);
  assert.equal(nextIndex('ArrowUp', 0, 4), 3);
  assert.equal(nextIndex('Home', 2, 4), 0);
  assert.equal(nextIndex('End', 0, 4), 3);
  assert.equal(nextIndex('Tab', 0, 4), null);
  assert.equal(nextIndex('a', 0, 4), null);
});

// Stand-ins for the page: just what enhance reads and writes.
class FakeEl {
  attrs = new Map<string, string>();
  dataset: Record<string, string> = {};
  tabIndex = 0;
  hidden = false;
  classes = new Set<string>();
  classList = { add: (c: string) => void this.classes.add(c) };
  listeners = new Map<string, Array<(e: never) => void>>();
  focused = false;
  setAttribute(k: string, v: string): void {
    this.attrs.set(k, v);
  }
  addEventListener(type: string, fn: (e: never) => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }
  focus(): void {
    this.focused = true;
  }
  fire(type: string, event: object = {}): void {
    for (const fn of this.listeners.get(type) ?? []) fn(event as never);
  }
}
class FakeGroup extends FakeEl {
  tabs: FakeEl[];
  panels: FakeEl[];
  list: FakeEl | null;
  constructor(names: string[], list = true) {
    super();
    this.dataset = { group: 'client' };
    this.tabs = names.map((n) => Object.assign(new FakeEl(), { dataset: { tab: n } }));
    this.panels = names.map(() => new FakeEl());
    this.list = list ? Object.assign(new FakeEl(), { hidden: true }) : null;
  }
  querySelectorAll(sel: string): FakeEl[] {
    return sel === '[role="tab"]' ? this.tabs : this.panels;
  }
  querySelector(): FakeEl | null {
    return this.list;
  }
}
const store = (init: Record<string, string> = {}): Storage & { data: Record<string, string> } => {
  const data = { ...init };
  return { data, getItem: (k: string) => data[k] ?? null, setItem: (k: string, v: string) => void (data[k] = v) } as never;
};
const shown = (g: FakeGroup): boolean[] => g.panels.map((p) => !p.hidden);
const asEl = (g: FakeGroup): HTMLElement => g as unknown as HTMLElement;

test('enhance: shows the tablist, selects the first tab and hides the other panels', () => {
  const g = new FakeGroup(['a', 'b', 'c']);
  enhance(asEl(g), store());
  assert.equal(g.list?.hidden, false);
  assert.ok(g.classes.has('tabs-on'));
  assert.deepEqual(shown(g), [true, false, false]);
  assert.deepEqual(g.tabs.map((t) => t.attrs.get('aria-selected')), ['true', 'false', 'false']);
  assert.deepEqual(g.tabs.map((t) => t.tabIndex), [0, -1, -1]);
});

test('enhance: a click selects the tab, shows its panel and remembers the choice', () => {
  const g = new FakeGroup(['a', 'b', 'c']);
  const s = store();
  enhance(asEl(g), s);
  g.tabs[2].fire('click');
  assert.deepEqual(shown(g), [false, false, true]);
  assert.equal(g.tabs[2].attrs.get('aria-selected'), 'true');
  assert.equal(s.data['tierstore.tab.client'], 'c');
});

test('enhance: an arrow key moves and focuses the next tab, wrapping, and cancels the key; other keys do nothing', () => {
  const g = new FakeGroup(['a', 'b', 'c']);
  enhance(asEl(g), store());
  let prevented = 0;
  g.tabs[0].fire('keydown', { key: 'ArrowLeft', preventDefault: () => prevented++ });
  assert.deepEqual(shown(g), [false, false, true]);
  assert.ok(g.tabs[2].focused);
  g.tabs[2].fire('keydown', { key: 'Home', preventDefault: () => prevented++ });
  assert.deepEqual(shown(g), [true, false, false]);
  g.tabs[0].fire('keydown', { key: 'x', preventDefault: () => prevented++ });
  assert.equal(prevented, 2);
  assert.deepEqual(shown(g), [true, false, false]);
});

test('enhance: the remembered tab is selected at the start, without writing it again; an unknown one is ignored', () => {
  const g = new FakeGroup(['a', 'b', 'c']);
  const s = store({ 'tierstore.tab.client': 'b' });
  enhance(asEl(g), s);
  assert.deepEqual(shown(g), [false, true, false]);
  const h = new FakeGroup(['a', 'b']);
  enhance(asEl(h), store({ 'tierstore.tab.client': 'zzz' }));
  assert.deepEqual(shown(h), [true, false]);
});

test('enhance: a group whose parts do not match is left as the markup has it, every panel shown', () => {
  const g = new FakeGroup(['a', 'b'], false);
  enhance(asEl(g), store());
  assert.deepEqual(shown(g), [true, true]);
  const h = new FakeGroup([]);
  enhance(asEl(h), store());
  assert.ok(!h.classes.has('tabs-on'));
  const k = new FakeGroup(['a', 'b']);
  k.panels.pop();
  enhance(asEl(k), store());
  assert.deepEqual(shown(k), [true]);
  assert.ok(!k.classes.has('tabs-on'));
  const d = new FakeGroup(['a', 'b']);
  delete d.dataset.group;
  enhance(asEl(d), store({ 'tierstore.tab.': 'b' }));
  assert.deepEqual(shown(d), [false, true]);
});

test('storage: a storage that throws, on reading, on writing or on being reached, costs the page nothing', () => {
  const thrower = { getItem: () => { throw new Error('denied'); }, setItem: () => { throw new Error('full'); } } as unknown as Storage;
  assert.equal(recall(thrower, 'client'), null);
  assert.doesNotThrow(() => remember(thrower, 'client', 'a'));
  assert.equal(recall(null, 'client'), null);
  assert.doesNotThrow(() => remember(null, 'client', 'a'));
  const g = new FakeGroup(['a', 'b']);
  enhance(asEl(g), thrower);
  g.tabs[1].fire('click');
  assert.deepEqual(shown(g), [false, true]);
  const blocked = Object.defineProperty({}, 'localStorage', { get: () => { throw new Error('SecurityError'); } }) as { localStorage: Storage };
  assert.equal(storageOf(blocked), null);
  const ok = store();
  assert.equal(storageOf({ localStorage: ok }), ok);
});

test('the built Get started page: six client languages and four install methods, in tabs, every panel in the page', () => {
  const dist = buildReal('tabs');
  const html = readFileSync(join(dist, 'get-started', 'index.html'), 'utf8');
  const labels = (name: string): string[] => {
    const g = new RegExp(`<div class="tabs" data-group="${name}">[\\s\\S]*?<div class="tab-list"[^>]*>([\\s\\S]*?)</div>`).exec(html)?.[1] ?? '';
    return [...g.matchAll(/role="tab"[^>]*>([^<]+)<\/button>/g)].map((m) => m[1]);
  };
  assert.deepEqual(labels('client'), ['AWS CLI', 'Python (boto3)', 'JavaScript (AWS SDK v3)', 'Go (aws-sdk-go-v2)', 'Rust (aws-sdk-s3)', 'Java (AWS SDK v2)']);
  assert.deepEqual(labels('install'), ['Tarball', 'Script (planned)', 'Homebrew (planned)', 'apt (planned)']);
  assert.deepEqual(labels('shape'), ['One machine', 'Storage node', 'Build node']);
  assert.equal(html.match(/role="tabpanel"/g)?.length, 6 + 4 + 3);
  assert.ok(!/role="tabpanel"[^>]* hidden/.test(html));
  assert.ok(!html.includes('data-tabs='), 'a data-tabs marker reached the page');
  assert.match(html, /<script type="module" src="\/assets\/[0-9a-f]{16}\/tabs\.js"><\/script>/);
  // Each client points at the endpoint, path-style, with the documented region and bucket.
  const text = html.replace(/<[^>]+>/g, '').replace(/&quot;/g, '"');
  for (const needle of ['endpoint_url="http://tierstore.example:9000"', 'forcePathStyle: true', 'o.UsePathStyle = true', 'force_path_style(true)', '.forcePathStyle(true)', '--endpoint-url http://tierstore.example:9000']) {
    assert.ok(text.includes(needle), needle);
  }
});

test('the tab script ships with the page that has tabs, and the other pages do not load it', () => {
  const dist = buildReal('tabs-scripts');
  assert.ok(readFileSync(join(dist, 'get-started', 'index.html'), 'utf8').includes('/tabs.js"'));
  assert.ok(!readFileSync(join(dist, 'why', 'index.html'), 'utf8').includes('/tabs.js"'));
});
