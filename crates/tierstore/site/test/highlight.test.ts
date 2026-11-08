// Syntax highlighting and tabs, done by the build.
//
// Catches: a configuration block that comes out as plain text, a token that is wrapped but not escaped (markup from
// the example reaching the page), a prompt mark that is lost, a language the highlighter does not know passing
// silently, a highlight colour that cannot be read on the code panel, and a Go or Java snippet left uncoloured.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { highlight, highlightBlocks } from '../tools/highlight';
import { buildReal, SITE } from './support';

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

test('go and java: keywords, types, calls, strings and comments', () => {
  const go = highlight('go', 'client := s3.NewFromConfig(cfg) // done\nBucket: aws.String("tierstore"), nil');
  assert.ok(go.includes('<span class="h-f">NewFromConfig</span>'));
  assert.ok(go.includes('<span class="h-c">// done</span>'));
  assert.ok(go.includes('<span class="h-s">&quot;tierstore&quot;</span>'));
  assert.ok(go.includes('<span class="h-k">nil</span>'));
  assert.ok(highlight('go', 'return err').includes('<span class="h-k">return</span>'));
  assert.ok(highlight('go', 'x := `raw "text"`').includes('<span class="h-s">`raw &quot;text&quot;`</span>'));
  const java = highlight('java', 'S3Client client = S3Client.builder().forcePathStyle(true).build();');
  assert.ok(java.includes('<span class="h-t">S3Client</span>'));
  assert.ok(java.includes('<span class="h-f">builder</span>'));
  assert.ok(java.includes('<span class="h-k">true</span>'));
  assert.ok(highlight('java', 'new Foo(1); // n').includes('<span class="h-k">new</span>'));
});

test('the built Get started page: toml, shell and every client language are coloured', () => {
  const dist = buildReal('highlight');
  const html = readFileSync(join(dist, 'get-started', 'index.html'), 'utf8');
  assert.ok(html.includes('<span class="h-t">[[interface]]</span>'));
  assert.ok(html.includes('<span class="h-a">proxy_id</span>'));
  assert.ok(html.includes('<span class="h-f">curl</span> <span class="h-a">-fsSL</span>'));
  assert.ok(html.includes('<span class="h-f">LoadDefaultConfig</span>'), 'go is not coloured');
  assert.ok(html.includes('<span class="h-t">S3Client</span>'), 'java and js are not coloured');
  assert.ok(html.includes('<span class="h-k">use</span> aws_sdk_s3'), 'rust is not coloured');
  assert.ok(html.includes('<span class="h-k">import</span> boto3'), 'python is not coloured');
  assert.ok(!html.includes('data-lang='), 'a data-lang marker reached the page');
  const guide = readFileSync(join(dist, 'docs', 'configure', 'index.html'), 'utf8');
  assert.ok(guide.includes('<pre class="hl">') && guide.includes('<span class="h-t">[server]</span>'), 'the configuration guide is not coloured');
});

// WCAG 2.x contrast of the highlight colours on the code panel (site.css names both: the colours and --code-bg, the deep navy panel).
const channel = (v: number): number => (v / 255 <= 0.03928 ? v / 255 / 12.92 : ((v / 255 + 0.055) / 1.055) ** 2.4);
const luminance = (hex: string): number => {
  const n = parseInt(hex.slice(1), 16);
  return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255);
};
const contrast = (a: string, b: string): number => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

test('contrast: the formula gives 21 for black on white and 1 for a colour on itself', () => {
  assert.equal(Math.round(contrast('#000000', '#ffffff')), 21);
  assert.equal(contrast('#123456', '#123456'), 1);
});

test('every highlight colour is at least 4.5:1 (WCAG AA) on the code panel, and so are the prompt, comment and output colours', () => {
  const site = readFileSync(join(SITE, 'static', 'site.css'), 'utf8');
  const theme = readFileSync(join(SITE, 'static', 'theme.css'), 'utf8');
  const vars = new Map([...theme.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/g), ...site.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/g)].map((m) => [m[1], m[2]] as const));
  const panel = vars.get('code-bg') as string;
  assert.equal(panel, '#0d1822', 'the code panel is the deep navy of the header and the hero');
  for (const name of ['code-fg', 'on-deep-muted', 'orange']) assert.ok(contrast(vars.get(name) as string, panel) >= 4.5, `${name} on the code panel`);
  const colours = [...site.matchAll(/^\.hl \.(h-[a-z](?:, \.hl \.h-[a-z])*) \{ color: (#[0-9a-f]{6}|var\(--[a-z-]+\))/gm)];
  assert.ok(colours.length >= 5, 'the highlight rules were not found');
  const classes = new Set<string>();
  for (const m of colours) {
    for (const c of m[1].split(', .hl .')) classes.add(c);
    const colour = m[2].startsWith('var(') ? (vars.get(m[2].slice(6, -1)) as string) : m[2];
    const ratio = contrast(colour, panel);
    assert.ok(ratio >= 4.5, `${m[1]} ${colour} on the code panel is ${ratio.toFixed(2)}:1`);
  }
  for (const c of ['h-k', 'h-s', 'h-n', 'h-c', 'h-t', 'h-a', 'h-f']) assert.ok(classes.has(c), `no colour for ${c}`);
});
