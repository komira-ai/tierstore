// The public-repo scan over the built output.
//
// Catches: text that must not ship in a public repository's site: a machine address, a home directory path, a host
// that is not a public one this site links to, a cloud account's resource name, an internal repository name, or a date
// from before the project went public. The site is built from page copy and recorded data (a terminal recording and a
// memory trace), and recordings are where such things leak, so the scan reads every built file, data included.
// The repository lints (crates/kbf-integration-test/tests/repo_lints.rs) scan the sources; this scans what is deployed.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { buildReal, copyTree, plant } from './support';

/** The hosts a page may name in a URL: the project's own, GitHub, the two build tools it serves, the SVG namespace. */
const PUBLIC_HOSTS = new Set(['tierstore.dev', 'kbf.dev', 'github.com', 'buck2.build', 'bazel.build', 'www.w3.org']);
/** Placeholders and loopback the configuration reference uses as examples. */
const EXAMPLE_HOSTS = new Set(['host', 'localhost', '127.0.0.1', 'frontend', 'FRONTEND', 'bucket', 's3-hot.example', 's3-cold.example', 'tierstore.example']);

/** The only 40-hex string a page may carry: the example SHA of the install page and docs/artifacts.md. */
const EXAMPLE_SHA = '0123456789abcdef0123456789abcdef01234567';

/**
 * SHA-256 digests (lowercase hex) of the names a page may not carry: the machine names of the project's own fleet.
 * The names are kept as digests so that this public file does not spell them.
 */
const INTERNAL_NAME_DIGESTS: ReadonlySet<string> = new Set<string>([
  // The whole names of one numbered shape, ten of them (shape followed by 00 to 09).
  '390d87d849b5818395fa0522a03f873b4dd1892c6dc2d02274fa9e1c5e7db26a',
  '70997b5d616f4da45090f863e47f048b7773464ffaf60575d03e613d22f8da10',
  '0f5aa9d8fdf7cd7e3d4ab8a39648513c7b06a98d8e081b591989b70839b5460c',
  '879fdcb78de039afea292497cf56033b30d351e9896e1cd37d04aae392af0576',
  'd52c6ab21a194785901db1c28f45c638bc9b1a4c18eb1d30f4b59ba9ab440b63',
  '927737f5ef57e4f6e4e3d39126f6f70d2ca434a241b63b70f0f9a38b89596360',
  '7ebda8e19caa08f4977a57e81244a48774cc181299e2ec76161017e608c8e588',
  '6fe039a3c056fe992495e12b605233da82a31392643dbd363623c0d90fffc379',
  '0ab2cfa1499fe2262dbc76f0df6dd63c4a18cb519bd9424baff0361bae54308c',
  '235eff94783530f47f3e08d8b1daa82780689002440fa70907629b4f88f9e79c',
  // The stems of three more shapes: one a prefix before a hyphen, two machine families that take a number.
  'ae238ad51a2cd16ee5edfa12986a516d39bc9d80eb04c8b7e894650a869cbcb2',
  '0f96571912ed33a1cd735cd2e659e289788af087ad8ec2b6ddaf5b661ce9591a',
  '28ce5c91a5190fc5abed868345152b60fe8d70d9c2a19340d76c012b00d25038',
]);

const sha256 = (s: string): string => createHash('sha256').update(s).digest('hex');

/** What a token is looked up as: lowercase whole, without its trailing digits, and its first hyphen-separated segment without them. */
function nameCandidates(token: string): string[] {
  const t = token.toLowerCase();
  const stripped = (x: string): string => x.replace(/\d+$/, '');
  return [t, stripped(t), stripped(t.split('-')[0])];
}

/** The findings for `text`: each token (a run of letters, digits and hyphens) one of whose candidates has its digest in `deny`. The finding gives the line and the digest, not the name. */
function hashedNames(text: string, deny: ReadonlySet<string>): string[] {
  const found: string[] = [];
  text.split('\n').forEach((line, i) => {
    for (const token of line.split(/[^A-Za-z0-9-]+/)) {
      const hit = nameCandidates(token).map(sha256).find((d) => deny.has(d));
      if (token !== '' && hit !== undefined) found.push(`an internal host name (line ${i + 1}, sha256 ${hit.slice(0, 12)})`);
    }
  });
  return found;
}

function scanText(text: string, code = false, deny: ReadonlySet<string> = INTERNAL_NAME_DIGESTS): string[] {
  const found: string[] = hashedNames(text, deny);
  for (const m of text.matchAll(/\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/g)) {
    const parts = [m[1], m[2], m[3], m[4]].map(Number);
    if (parts.every((p) => p <= 255)) {
      const a = m[0];
      if (a !== '127.0.0.1' && a !== '0.0.0.0' && !/^(?:192\.0\.2|198\.51\.100|203\.0\.113)\./.test(a)) found.push(`an address, ${a}`);
    }
  }
  for (const m of text.matchAll(/(?<![.\/-])\/(?:home|Users|root)\/[A-Za-z0-9._-]+/g)) found.push(`a home path, ${m[0]}`);
  for (const m of text.matchAll(/\bbuck-out\/\S*/g)) found.push(`a build-farm path, ${m[0]}`);
  for (const m of text.matchAll(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}/g)) found.push(`an email address or user@host, ${m[0]}`);
  for (const m of text.matchAll(/\b[a-z][a-z0-9+.-]*:\/\/([A-Za-z0-9.-]+)/g)) {
    const host = m[1];
    if (!PUBLIC_HOSTS.has(host) && !EXAMPLE_HOSTS.has(host)) found.push(`a host, ${host}`);
  }
  // Program text is skipped here: `c.local` is a property of an object, not a host (recorded data is not program text).
  for (const m of code ? [] : text.matchAll(/\b[\w.-]+\.(?:internal|local|lan|corp|amazonaws\.com|cloudfront\.net)\b/g)) found.push(`a private or cloud host name, ${m[0]}`);
  for (const m of text.matchAll(/arn:aws[\w-]*:\S+/g)) found.push(`a cloud resource name, ${m[0]}`);
  for (const m of text.matchAll(new RegExp('komira-' + 'internal', 'g'))) found.push(`an internal repository name, ${m[0]}`);
  for (const m of text.matchAll(/\b(20(?:[01]\d|2[0-5])-\d\d-\d\d|2026-0[1-8]-\d\d)\b/g)) found.push(`a date before the project was public, ${m[1]}`);
  if (/\bCEO\b/.test(text)) found.push('an internal role, CEO');
  for (const m of text.matchAll(/\b[0-9a-f]{40}\b/g)) if (m[0] !== EXAMPLE_SHA) found.push(`a commit SHA, ${m[0]}`);
  return found;
}

const dist = buildReal('hygiene');
const scanTree = (dir: string, deny: ReadonlySet<string> = INTERNAL_NAME_DIGESTS): string[] => listFiles(dir).flatMap((f) => scanText(readFileSync(join(dir, f), 'utf8'), /^assets\/[0-9a-f]+\/[a-z]+\.js$/.test(f), deny).map((p) => `${f}: ${p}`));

test('no built file carries an address, a home path, a non-public host, an internal name or an early date', () => {
  assert.deepEqual(scanTree(dist), []);
});

test('the scan finds each kind of leak (it can fail)', () => {
  const ip = ['10', '1', '2', '3'].join('.');
  const home = '/ho' + 'me/alice';
  const cases: [string, RegExp][] = [
    [`ssh ${ip}`, /an address/],
    [`cd ${home}/src`, /a home path/],
    ['at /kbf' + '/root/komira/x.zig:1', /a home path/],
    ['in buck-' + 'out/v2/tmp/x', /a build-farm path/],
    ['mail ' + 'a@b' + '.example', /an email address/],
    ['see https://build-7.example.org/x', /a host, build-7\.example\.org/],
    ['at ip-1-2-3-4.compute.internal', /a private or cloud host name/],
    ['arn:aws:iam::123456789012:role/x', /a cloud resource name/],
    ['komira-' + 'internal#12', /an internal repository name/],
    ['on ' + '2026-0' + '3-14 we', /a date before/],
    ['the C' + 'EO said', /an internal role/],
    ['at ' + 'abcdef0123456789abcdef0123456789abcdef01', /a commit SHA/],
  ];
  for (const [text, why] of cases) assert.match(scanText(text).join('\n'), why, text);
  assert.deepEqual(scanText('https://github.com/komira-ai/kbf and http://127.0.0.1:8990 and 2026-10-01 and ' + EXAMPLE_SHA), []);
});

test('a leak in the recorded data or in the page copy is found in the built tree', () => {
  const t = copyTree(dist, 'hygiene-planted');
  plant(t, 'index.html', '<h1', `<p>${['10', '9', '8', '7'].join('.')}</p><h1`);
  assert.match(scanTree(t).join('\n'), /index\.html: an address/);
});

/** A deny set of made-up names (never the real set), one for each form a candidate takes. */
const TEST_DENY: ReadonlySet<string> = new Set(['zebra-07', 'quokka', 'narwhal'].map(sha256));

test('a name in a deny set is refused on a page by its whole form, by its form without trailing digits and by its first segment', () => {
  const refused = (page: string): string => scanText(page, false, TEST_DENY).join('\n');
  assert.match(refused('<p>run it on zebra-07 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on ZEBRA-07 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on quokka9 today</p>'), /internal host name/);
  assert.match(refused('<p>run it on narwhal-edge-3 today</p>'), /internal host name/);
  assert.match(refused('<code>narwhal-4</code>'), /internal host name/);
  assert.deepEqual(scanText('<p>zebra-08, quokkas and narwhals</p>', false, TEST_DENY), []);
});

test('a name in a deny set that a built page carries is found by the tree scan', () => {
  const t = copyTree(dist, 'hygiene-planted-name');
  plant(t, 'index.html', '<h1', '<p>on zebra-07 only</p><h1');
  assert.match(scanTree(t, TEST_DENY).join('\n'), /index\.html: .*internal host name/);
  assert.deepEqual(scanTree(dist, TEST_DENY), []);
});

test('the real deny set is not empty and holds only SHA-256 digests in lowercase hex', () => {
  assert.ok(INTERNAL_NAME_DIGESTS.size > 0);
  for (const d of INTERNAL_NAME_DIGESTS) assert.match(d, /^[0-9a-f]{64}$/);
});

test('the words node, nodes and node.js are not refused', () => {
  assert.deepEqual(scanText('<p>a node, three nodes, node.js, Node and the nodes-per-rack setting</p>'), []);
});

test('a name in a deny set followed by more hyphenated words is refused, and a longer name that only starts like it is not', () => {
  const refused = (page: string): string => scanText(page, false, TEST_DENY).join('\n');
  assert.match(refused('<p>on zebra-07-edge</p>'), /internal host name/);
  assert.match(refused('<p>on Zebra-07-a-b.</p>'), /internal host name/);
  assert.match(refused('<p>on quokka9-x-y</p>'), /internal host name/);
  assert.match(refused('<p>on narwhal-a-b-c</p>'), /internal host name/);
  assert.deepEqual(scanText('<p>zebra-070-edge, quokkas-per-rack, narwhals-a-b, zebra-0-b</p>', false, TEST_DENY), []);
});
