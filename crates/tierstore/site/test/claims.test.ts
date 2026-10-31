// The statements of the pages about what tierstore does not do, and the strings of the demo, held to the code.
//
// Catches: a page that says "not built" about something the code now does, and a demo line that is no longer what the
// program prints. The site says the per-node cache is a build node (`[local_cache]` with `[cold_store]`),
// that the only counters `/metricz` has are the OCI front's and the cache's, and that the binary does not use `CachedStore`; the demo quotes the bodies of the
// probes. Each of those is read from the crate's source here, so when the code changes the test fails and says which
// page to update. The crate's source is the directory above the site's (crates/tierstore/src, and src/ once the
// crate is a repository of its own).

import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { test } from 'node:test';
import { DOCS, read, SITE } from './support';

const SRC = resolve(SITE, '..', 'src');
const isTestFile = (f: string): boolean => /(_tests|_testing|testing)\.rs$/.test(f);

/** The binary's own source: main.rs, the tiers' counters (metrics.rs) and the server module's files that are not tests. */
function binarySource(): Map<string, string> {
  const files = new Map<string, string>([['main.rs', read(SRC, 'main.rs')], ['metrics.rs', read(SRC, 'metrics.rs')]]);
  const walk = (dir: string): void => {
    for (const e of readdirSync(join(SRC, dir), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (e.isDirectory()) walk(`${dir}/${e.name}`);
      else if (e.name.endsWith('.rs') && !isTestFile(e.name)) files.set(`${dir}/${e.name}`, read(SRC, dir, e.name));
    }
  };
  walk('server');
  return files;
}

/**
 * What the compiler reads of a Rust file: the text without `#[cfg(test)]` items (the attribute, any attributes after it and
 * the item up to its closing brace or semicolon) and without comment lines, so that a literal in a test or a doc comment
 * proves nothing about the program.
 */
export function code(text: string): string {
  const lines = text.split('\n');
  const out: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    if (/^\s*\/\//.test(lines[i])) continue;
    if (!/^\s*#\[cfg\(test\)\]\s*$/.test(lines[i])) {
      out.push(lines[i]);
      continue;
    }
    let depth = 0;
    let braced = false;
    for (i++; i < lines.length; i++) {
      if (!braced && /^\s*#\[/.test(lines[i])) continue;
      for (const c of lines[i]) {
        if (c === '{') { depth++; braced = true; } else if (c === '}') depth--;
      }
      if (braced ? depth <= 0 : /;\s*$/.test(lines[i])) break;
    }
  }
  return out.join('\n');
}

/** The files that export metrics: the OCI front's, the cache's and the tiers'. */
const EXPORTERS = new Set(['server/oci/metrics.rs', 'server/local_cache/metrics.rs', 'metrics.rs', 'server/build_info.rs', 'server/mod.rs']);

/** What in the source no longer matches the pages. Each problem names the pages to change. */
export function staleClaims(src: ReadonlyMap<string, string>): string[] {
  const out: string[] = [];
  const all = [...src.entries()];
  const config = src.get('server/config.rs') ?? '';
  if (!/build-node shape/.test(config) || !/\[local_cache\]/.test(config)) {
    out.push('config.rs no longer reads a build node ([local_cache] with [cold_store]): update docs/configuration.md, docs/operations.md ("A build node"), content/why.html and content/index.html (the status table)');
  }
  for (const [name, text] of all) {
    if (!EXPORTERS.has(name) && /# TYPE |tierstore_cache_\{/.test(text)) out.push(`${name} exports metrics: the counters of the OCI front, the cache and the tiers (EXPORTERS) were the only ones; update docs/operations.md ("Metrics"), docs/metrics.md and content/why.html ("Where it stops")`);
    if (/\bCachedStore\b/.test(text)) out.push(`${name} uses CachedStore: the binary has a cache now; update content/why.html ("The per-node read-through cache is not in the binary") and content/index.html`);
  }
  const routes = new Set([...(src.get('server/mod.rs') ?? '').matchAll(/\(true, "(\/[a-z]+)"/g)].map((m) => m[1]));
  const want = ['/healthz', '/metricz', '/readyz', '/usagez'];
  if ([...routes].sort().join() !== want.sort().join()) {
    out.push(`the probe routes in server/mod.rs are ${[...routes].sort().join(', ')}, not ${want.join(', ')}: update docs/operations.md ("Probes") and content/demo.html`);
  }
  const mod = code(src.get('server/mod.rs') ?? '');
  const cli = code(src.get('server/cli.rs') ?? '');
  const info = code(src.get('server/build_info.rs') ?? '');
  // The start line, as the program formats it: `tierstore {VERSION} listen=...` (not a literal in a test).
  if (!/format!\("tierstore \{VERSION\} listen=/.test(cli)) {
    out.push('the start line "tierstore <version> listen=<address>" is no longer built by format!("tierstore {VERSION} listen=... in server/cli.rs: update content/demo.html');
  }
  // The metric names the pages give, as /metricz prints them: the type line and the sample, in code (not a doc comment).
  if (!/# TYPE tierstore_drain_late_requests_total counter\\n/.test(mod) || !/tierstore_drain_late_requests_total \{/.test(mod)) {
    out.push('server/mod.rs no longer prints the type line and the sample of tierstore_drain_late_requests_total: update content/why.html ("Where it stops")');
  }
  if (!/# TYPE tierstore_build_info gauge\\n/.test(info) || !/tierstore_build_info\{\{version=/.test(info)) {
    out.push('server/build_info.rs no longer prints the type line and the sample of tierstore_build_info: update content/why.html ("Where it stops")');
  }
  // What content/demo.html ("Stop it: the drain") says the drain does.
  if (!/if !self\.is_ready\(\) \{[^}]*from_static\("close"\)[^}]*insert\(CONNECTION/.test(mod)) {
    out.push('server/mod.rs no longer sets Connection: close on an answer while not ready: update content/demo.html ("Stop it: the drain")');
  }
  if (!/^mod linger;$/m.test(mod) || !/\bLingering::new\(/.test(mod) || !/header_timeout/.test(code(src.get('server/linger.rs') ?? ''))) {
    out.push('the lingering close (server/linger.rs, used by server/mod.rs, bounded by header_timeout) is gone: update content/demo.html ("Stop it: the drain")');
  }
  return out;
}

const page = (name: string): string => read(SITE, 'content', `${name}.html`);

test('the pages say what they say about the cache and metrics, so the code check below is not vacuous', () => {
  assert.match(read(DOCS, 'operations.md'), /the counters of the OCI read front/);
  assert.match(page('why'), /serves the counters of the OCI read front, those of the local cache of a build node and, on main, hit, miss and byte counters of the two tiers/);
  assert.match(page('why'), /The <code>tierstore<\/code> binary does not use it/);
  assert.match(page('why'), /<code>\[local_cache\]<\/code> and <code>\[cold_store\]<\/code> and no <code>\[hot_store\]<\/code>/);
  assert.match(page('index'), /a build node, file only/);
  // The metric names the page gives exist in the source, and the demo says what the drain does.
  // (that they are printed by the program, not only named in a comment, is staleClaims' check.)
  for (const name of ['tierstore_build_info', 'tierstore_drain_late_requests_total']) assert.match(page('why'), new RegExp(`<code>${name}</code>`));
  assert.match(page('demo'), /<code>Connection: close<\/code>.*lingering close.*not answered 503/);
});

test('the code still matches those statements', () => {
  assert.deepEqual(staleClaims(binarySource()), []);
});

test('the check can fail: a second metrics exporter, a CachedStore use and a dropped build-node shape are each reported with the page to change', () => {
  const real = binarySource();
  const with_ = (name: string, f: (s: string) => string): Map<string, string> => new Map([...real].map(([k, v]) => [k, k === name ? f(v) : v]));
  // Planted in a file that is no exporter, so a file added to EXPORTERS later does not hide it.
  const planted = new Map([...real, ['server/planted.rs', '# TYPE x counter']]);
  assert.match(staleClaims(planted).join('\n'), /server\/planted\.rs exports metrics.*docs\/operations\.md/);
  assert.match(staleClaims(with_('main.rs', (s) => s + '\nuse tierstore::CachedStore;')).join('\n'), /uses CachedStore.*content\/why\.html/);
  assert.match(staleClaims(with_('server/config.rs', (s) => s.replace('build-node shape', 'ok'))).join('\n'), /no longer reads a build node.*docs\/configuration\.md/);
  assert.match(staleClaims(with_('server/mod.rs', (s) => s.replace('"/usagez"', '"/usagez2"'))).join('\n'), /probe routes/);
});

test('the check can fail: a literal in a test or a comment does not stand for what the program prints', () => {
  const real = binarySource();
  const with_ = (name: string, f: (s: string) => string): Map<string, string> => new Map([...real].map(([k, v]) => [k, k === name ? f(v) : v]));
  assert.deepEqual(staleClaims(real), []);
  // The start line: the real format! changes, and a test module still has a literal of the old shape.
  const startLine = with_('server/cli.rs', (s) => s.replace('format!("tierstore {VERSION} listen=', 'format!("tierstore {VERSION} listening=') + '\n#[cfg(test)]\nmod more {\n    const A: &str = "tierstore {version} listen={}";\n}\n');
  assert.match(staleClaims(startLine).join('\n'), /start line.*content\/demo\.html/);
  // The metrics: the exposition goes and the doc comment that names the metric stays.
  const noDrain = with_('server/mod.rs', (s) => s.replace('# TYPE tierstore_drain_late_requests_total counter', '# TYPE tierstore_other counter').replace('tierstore_drain_late_requests_total {late}', 'tierstore_other {late}'));
  assert.ok(noDrain.get('server/mod.rs')?.includes('`tierstore_drain_late_requests_total`'), 'the doc comment is still there');
  assert.match(staleClaims(noDrain).join('\n'), /tierstore_drain_late_requests_total.*content\/why\.html/);
  const noInfo = with_('server/build_info.rs', (s) => s.replace('# TYPE tierstore_build_info gauge', '# TYPE tierstore_info gauge'));
  assert.match(staleClaims(noInfo).join('\n'), /tierstore_build_info.*content\/why\.html/);
  // The drain: no Connection: close while not ready, and no lingering close.
  assert.match(staleClaims(with_('server/mod.rs', (s) => s.replace('response.headers_mut().insert(CONNECTION, close);', ''))).join('\n'), /Connection: close.*content\/demo\.html/);
  assert.match(staleClaims(with_('server/mod.rs', (s) => s.replace('Lingering::new(', 'Wrapped::new('))).join('\n'), /lingering close.*content\/demo\.html/);
  assert.match(staleClaims(new Map([...real].filter(([k]) => k !== 'server/linger.rs'))).join('\n'), /lingering close.*content\/demo\.html/);
});

test('code() drops cfg(test) items and comment lines and keeps the rest', () => {
  const text = ['fn a() {}', '#[cfg(test)]', 'use x::y;', '#[cfg(test)]', '#[path = "t.rs"]', 'mod tests;', '/// doc', '#[cfg(test)]', 'mod t {', '    fn b() { }', '}', 'fn c() {}'].join('\n');
  assert.equal(code(text), 'fn a() {}\nfn c() {}');
});

test('every output line of the demo is a string the program prints', () => {
  const mod = binarySource().get('server/mod.rs') as string;
  for (const body of ['"ok"', '"ready"', '"draining"']) assert.ok(mod.includes(body), `${body} is not in server/mod.rs: update content/demo.html`);
  assert.ok(mod.includes('no usage yet, {secs}s since start'), 'the /usagez body changed: update content/demo.html and docs/operations.md');
  assert.ok(mod.includes('usage published {secs}s ago'), 'the /usagez body changed: update content/demo.html and docs/operations.md');
  const cli = binarySource().get('server/cli.rs') as string;
  for (const line of ['tierstore: testing the {name}', 'tierstore: tested the {name} in {took} ms']) {
    assert.ok(cli.includes(line), `"${line}" is not in server/cli.rs: update content/demo.html`);
  }
  // (The start line is built by format!("tierstore {VERSION} listen=... in code; staleClaims checks that.)
  const demo = page('demo');
  for (const shown of ['tierstore: testing the hot tier', 'tierstore: tested the cold tier', 'tierstore &lt;version&gt; listen=127.0.0.1:9000', '<span class="o">ok</span>', '<span class="o">ready</span>', '<span class="o">draining</span>']) {
    assert.ok(demo.includes(shown), `the demo no longer shows ${shown}`);
  }
});
