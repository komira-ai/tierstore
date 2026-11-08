// Syntax highlighting, done when the pages are built: a code block marked `<pre data-lang="toml">` comes out with its
// tokens wrapped in `<span class="h-k">` and the like, so the page needs no script and no inline style. A small
// scanner per language, no dependency; text it does not know is left as it is.
//
// Classes: h-k keyword or boolean, h-s string, h-n number or placeholder, h-c comment, h-t table header or type,
// h-a key or option, h-f command or function.

import { escapeText } from './layout';

export type Lang = 'toml' | 'sh' | 'rust' | 'python' | 'js' | 'go' | 'java';
export const LANGS: readonly Lang[] = ['toml', 'sh', 'rust', 'python', 'js', 'go', 'java'];

type Rule = readonly [cls: string, re: RegExp];

const words = (list: string): string => `\\b(?:${list.split(' ').join('|')})\\b`;
const DQ = '"(?:[^"\\\\\\n]|\\\\.)*"';
const SQ = "'(?:[^'\\\\\\n]|\\\\.)*'";

const RULES: Readonly<Record<Lang, readonly Rule[]>> = {
  toml: [
    ['h-c', /#[^\n]*/y],
    ['h-t', /(?<![^\n])[ \t]*\[\[?[^\]\n]+\]\]?/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-s', new RegExp(SQ, 'y')],
    ['h-k', new RegExp(words('true false'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-a', /[A-Za-z_][\w-]*(?=\s*=)/y],
  ],
  sh: [
    ['h-c', /(?<![^\s])#[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-s', /'[^']*'/y],
    ['h-n', /<[a-z][a-z0-9-]*>/y],
    ['h-a', /(?<![^\s])--?[A-Za-z][\w-]*/y],
    ['h-k', /\$\{?[A-Za-z_]\w*\}?/y],
    ['h-k', /\b[A-Z][A-Z0-9_]*(?==)/y],
  ],
  rust: [
    ['h-c', /\/\/[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-k', new RegExp(words('use let mut fn pub struct impl true false self as mod const static match if else for in async await move'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-f', /[a-z_]\w*!/y],
    ['h-f', /[a-z_]\w*(?=\()/y],
    ['h-t', /[A-Z][A-Za-z0-9_]*/y],
  ],
  python: [
    ['h-c', /#[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-s', new RegExp(SQ, 'y')],
    ['h-k', new RegExp(words('import from as def return with for in if else True False None print'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-t', /[A-Z][A-Za-z0-9_]*/y],
    ['h-f', /[A-Za-z_]\w*(?=\()/y],
  ],
  js: [
    ['h-c', /\/\/[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-s', new RegExp(SQ, 'y')],
    ['h-k', new RegExp(words('import from const let var new await async true false of return export'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-t', /[A-Z][A-Za-z0-9_]*/y],
    ['h-f', /[A-Za-z_$][\w$]*(?=\()/y],
  ],
  go: [
    ['h-c', /\/\/[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-s', /`[^`]*`/y],
    ['h-k', new RegExp(words('package import func var const type struct interface return if else for range switch case default go defer nil true false'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-f', /[A-Za-z_]\w*(?=\()/y],
    ['h-t', /[A-Z][A-Za-z0-9_]*/y],
  ],
  java: [
    ['h-c', /\/\/[^\n]*/y],
    ['h-s', new RegExp(DQ, 'y')],
    ['h-k', new RegExp(words('import package public private protected static final class interface new return void if else for try catch throws true false null var'), 'y')],
    ['h-n', /\d[\d_]*(?:\.\d+)?\b/y],
    ['h-f', /[A-Za-z_]\w*(?=\()/y],
    ['h-t', /[A-Z][A-Za-z0-9_]*/y],
  ],
};

/** In a shell block with no prompt marks, every line starts with a command. */
const LINE_COMMAND: Rule = ['h-f', /(?<![^\n])[A-Za-z_./][\w./-]*/y];

/** `text` (plain, not escaped) as escaped HTML with its tokens in spans. */
export function highlight(lang: Lang, text: string, lineCommands = false): string {
  const rules = lang === 'sh' && lineCommands ? [LINE_COMMAND, ...RULES.sh] : RULES[lang];
  let out = '';
  let plain = '';
  const flush = (): void => {
    out += escapeText(plain);
    plain = '';
  };
  let i = 0;
  // A shell command line starts with the command: the first word of the text.
  if (lang === 'sh') {
    const first = /^(\s*)([A-Za-z_./][\w./-]*)/.exec(text);
    if (first !== null) {
      out += `${escapeText(first[1])}<span class="h-f">${escapeText(first[2])}</span>`;
      i = first[0].length;
    }
  }
  while (i < text.length) {
    let hit: [string, string] | null = null;
    for (const [cls, re] of rules) {
      re.lastIndex = i;
      const m = re.exec(text);
      if (m !== null && m[0] !== '') {
        hit = [cls, m[0]];
        break;
      }
    }
    if (hit === null) {
      // Words are skipped whole, so a keyword inside a longer name is not a keyword.
      const word = /^[\w$]+/.exec(text.slice(i));
      const step = word === null ? 1 : word[0].length;
      plain += text.slice(i, i + step);
      i += step;
      continue;
    }
    flush();
    out += `<span class="${hit[0]}">${escapeText(hit[1])}</span>`;
    i += hit[1].length;
  }
  flush();
  return out;
}

const unescape = (s: string): string => s.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');

/**
 * The page copy with every `<pre data-lang="x">` block highlighted. The block's text is escaped HTML and may hold the
 * prompt marks (`<span class="p">$</span>`), which stay as they are; the text between them is highlighted.
 */
export function highlightBlocks(html: string): string {
  return html.replace(/<pre data-lang="([a-z]+)">([\s\S]*?)<\/pre>/g, (_m, lang: string, inner: string) => {
    if (!(LANGS as readonly string[]).includes(lang)) throw new Error(`highlight: a code block in ${lang}, which is no language here`);
    const parts = inner.split(/(<span class="[a-z]+">[\s\S]*?<\/span>)/);
    const body = parts.map((p, i) => (i % 2 === 1 ? p : highlight(lang as Lang, unescape(p), parts.length === 1))).join('');
    return `<pre class="hl">${body}</pre>`;
  });
}
