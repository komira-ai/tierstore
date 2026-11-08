// Syntax highlighting and tabs, done by the build.
//
// Catches: a configuration block that comes out as plain text, a token that is wrapped but not escaped (markup from
// the example reaching the page), a prompt mark that is lost, a language the highlighter does not know passing
// silently, and tabs that hide content when no stylesheet is applied.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { highlight, highlightBlocks } from '../tools/highlight';
import { MAX_TABS, renderTabs } from '../tools/tabs';
import { buildReal } from './support';

test('toml: tables, keys, strings, numbers and booleans each get their class', () => {
  const out = highlight('toml', '[server]\nproxy_id = "a-1" # name\nn = 8\nok = true\n');
  assert.ok(out.includes('<span class="h-t">[server]</span>'));
  assert.ok(out.includes('<span class="h-a">proxy_id</span>'));
  assert.ok(out.includes('<span class="h-s">&quot;a-1&quot;</span>'));
  assert.ok(out.includes('<span class="h-c"># name</span>'));
  assert.ok(out.includes('<span class="h-n">8</span>'));
  assert.ok(out.includes('<span class="h-k">true</span>'));
});

test('toml: an array table header and an inline table key are recognised', () => {
  const out = highlight('toml', '[[interface]]\nparams = { addresses = ["127.0.0.1:9000"] }\n');
  assert.ok(out.includes('<span class="h-t">[[interface]]</span>'));
  assert.ok(out.includes('<span class="h-a">addresses</span>'));
});

test('sh: the command, the options, a placeholder and a quoted string', () => {
  const out = highlight('sh', "gh release download --repo komira-ai/tierstore --pattern 'x-*.tar.gz' <commit>");
  assert.ok(out.startsWith('<span class="h-f">gh</span>'));
  assert.ok(out.includes('<span class="h-a">--repo</span>'));
  assert.ok(out.includes('<span class="h-s">&#39;x-*.tar.gz&#39;</span>') || out.includes("<span class=\"h-s\">'x-*.tar.gz'</span>"));
  assert.ok(out.includes('<span class="h-n">&lt;commit&gt;</span>'));
});

test('rust, python and js: keywords, calls, strings and comments', () => {
  assert.ok(highlight('rust', 'let client = Client::from_conf(config); // done').includes('<span class="h-k">let</span>'));
  assert.ok(highlight('rust', 'let client = Client::from_conf(config); // done').includes('<span class="h-f">from_conf</span>'));
  assert.ok(highlight('rust', 'let client = Client::from_conf(config); // done').includes('<span class="h-c">// done</span>'));
  assert.ok(highlight('python', 'import boto3\nboto3.client("s3")').includes('<span class="h-k">import</span>'));
  assert.ok(highlight('python', 'import boto3\nboto3.client("s3")').includes('<span class="h-f">client</span>'));
  assert.ok(highlight('js', 'const c = new S3Client({ forcePathStyle: true });').includes('<span class="h-t">S3Client</span>'));
  assert.ok(highlight('js', 'const c = new S3Client({ forcePathStyle: true });').includes('<span class="h-k">true</span>'));
});

test('a keyword inside a longer name is not a keyword', () => {
  const out = highlight('python', 'important = 1');
  assert.equal(out.includes('h-k'), false);
});

test('text from the example is escaped, never read as markup', () => {
  const out = highlight('toml', 'a = "<b>&</b>"');
  assert.ok(!out.includes('<b>'));
  assert.ok(out.includes('&lt;b&gt;&amp;&lt;/b&gt;'));
});

test('a block keeps its prompt marks and gains the hl class; a block without a language is untouched', () => {
  const html = '<div class="code"><pre data-lang="sh"><span class="p">$</span> tar -xzf a.tar.gz\n<span class="p">$</span> ./x --version</pre></div><pre>plain &lt;x&gt;</pre>';
  const out = highlightBlocks(html);
  assert.ok(out.includes('<pre class="hl"><span class="p">$</span> <span class="h-f">tar</span>'));
  assert.ok(out.includes('<span class="p">$</span> <span class="h-f">./x</span> <span class="h-a">--version</span>'));
  assert.ok(out.endsWith('<pre>plain &lt;x&gt;</pre>'));
});

test('sh: in a block with no prompt marks every line starts with a command', () => {
  const out = highlightBlocks('<pre data-lang="sh">install -d /etc/x\nsystemctl start y</pre>');
  assert.ok(out.includes('<span class="h-f">install</span>') && out.includes('<span class="h-f">systemctl</span>'));
});

test('a language the highlighter does not know is refused', () => {
  assert.throws(() => highlightBlocks('<pre data-lang="cobol">x</pre>'), /no language here/);
});

test('tabs: radios, labels and panels in order, the first checked; the panels all stay in the page', () => {
  const out = renderTabs('<div class="tabs" data-tabs="client">\n<section data-tab="Rust"><p>r</p></section>\n<section data-tab="Python"><p>p</p></section>\n</div>');
  assert.ok(out.includes('<input type="radio" class="tab-input" name="tab-client" id="tab-client-1" checked>'));
  assert.ok(out.includes('<input type="radio" class="tab-input" name="tab-client" id="tab-client-2">'));
  assert.ok(out.includes('<label for="tab-client-1">Rust</label><label for="tab-client-2">Python</label>'));
  assert.equal(out.match(/<div class="tab-panel">/g)?.length, 2);
  assert.ok(out.includes('<p>r</p>') && out.includes('<p>p</p>'));
});

test('tabs: one tab, or more than the stylesheet has rules for, is refused', () => {
  const group = (n: number): string => `<div class="tabs" data-tabs="g">${Array.from({ length: n }, (_, i) => `<section data-tab="t${i}">x</section>`).join('')}</div>`;
  assert.throws(() => renderTabs(group(1)), /1 tabs/);
  assert.throws(() => renderTabs(group(MAX_TABS + 1)), /not 2 to/);
  assert.doesNotThrow(() => renderTabs(group(MAX_TABS)));
});

test('the built Get started page: its toml, shell and client snippets are coloured, in tabs of several languages', () => {
  const dist = buildReal('highlight');
  const html = readFileSync(join(dist, 'get-started', 'index.html'), 'utf8');
  assert.ok(html.includes('<span class="h-t">[[interface]]</span>'));
  assert.ok(html.includes('<span class="h-a">proxy_id</span>'));
  for (const label of ['Rust', 'Python', 'JavaScript', 'aws CLI', 'Linux x86_64', 'Linux arm64', 'macOS (Apple silicon)', 'One machine', 'Storage node', 'Build node']) {
    assert.ok(html.includes(`>${label}</label>`), `no tab ${label}`);
  }
  assert.ok(!html.includes('data-lang='), 'a data-lang marker reached the page');
  assert.ok(!html.includes('data-tab'), 'a data-tab marker reached the page');
  const guide = readFileSync(join(dist, 'docs', 'configure', 'index.html'), 'utf8');
  assert.ok(guide.includes('<pre class="hl">') && guide.includes('<span class="h-t">[server]</span>'), 'the configuration guide is not coloured');
});
