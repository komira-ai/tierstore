// The docs pages: every crates/tierstore/docs/*.md becomes one page at /docs/<name>/, and /docs/ lists them. The
// Markdown is the operator guide itself (a test of the crate holds it to the code), so the site cannot say anything the
// guide does not.
//
// The renderer reads the subset the guide uses, and nothing else: '#' to '###' headings, paragraphs, '-' and '1.'
// lists of one line per item, '>' notes, pipe tables, ``` fences, `code`, **bold** and [text](target) links. It
// escapes all text, makes no markup from anything else, and throws on a construct it does not read, so a page that
// uses more is a build error and not a page that is quietly wrong. No dependency.
//
// Three more things come from the same Markdown. A ```diagram fence is drawn as an SVG (diagram.ts). docs/examples/*.md
// are the pages under /docs/examples/. docs/index.md is the landing page, /docs/: the tables of every configuration
// table are copied into it from configuration.md when the pages are loaded, so the list on the landing page cannot
// differ from the reference. The Markdown each page is made from is also served as it is (rawMarkdown), for programs.

import { existsSync, readdirSync, readFileSync } from 'fs';
import { join } from 'path';
import { describeDiagram, parseDiagram, renderDiagram } from './diagram';
import { escapeText } from './layout';
import { DOMAIN, type Page } from './pages';

/** The guide's pages after the configuration pages, in the order they read; any other page of docs/ follows, by name. */
export const DOC_ORDER: readonly string[] = ['overview', 'configure', 'install', 'keys', 'operations', 'troubleshooting', 'limits', 'metrics', 'update-hook', 'clients'];
/** The examples, in the order the pages and the cards list them. */
export const EXAMPLE_ORDER: readonly string[] = ['directory-as-bucket', 'cache-in-front-of-s3', 'cache-hot-cold'];
/** The page that is /docs/ itself, and the page that is the reference of every key. */
export const LANDING = 'index';
export const REFERENCE = 'configuration';
/** The sections of the reference whose tables the landing page lists, in order, and the heading each has. */
export const TABLE_SECTIONS: readonly string[] = ['`[[interface]]`', '`[server]`', '`[hot_store]` and `[cold_store]`', '`[local_cache]`', '`[[policy]]`', '`params.upstream`'];

/** The title of the sections that say what is planned and not in the code. */
export const NOT_YET = 'Not in a release yet';

export interface Doc {
  /** The file's name without '.md': 'overview', 'index' (the landing page) or 'examples/cache-hot-cold'. */
  stem: string;
  /** The text of its '# ' heading. */
  title: string;
  /** Its first paragraph as plain text, for the page's description. */
  summary: string;
  /** The inside of <main>, without the docs navigation. */
  body: string;
  /** The Markdown the page is made from (the landing page with its tables copied in). */
  md: string;
}

/** The anchor of a heading, as the test of the crate makes it: letters and digits lowercased, '_' kept, ' ' and '-' as '-'. */
export function slug(heading: string): string {
  let out = '';
  for (const c of heading) {
    if (/[a-z0-9_]/.test(c)) out += c;
    else if (/[A-Z]/.test(c)) out += c.toLowerCase();
    else if (c === ' ' || c === '-') out += '-';
  }
  return out;
}

export const docRoute = (stem: string): string => (stem === LANDING ? '/docs/' : `/docs/${stem}/`);
const isExample = (stem: string): boolean => stem.startsWith('examples/');

/** `text` with its inline markup made HTML: code spans, bold and links. `known` is the set of pages a link may name. */
export function inline(text: string, known: ReadonlySet<string>): string {
  return text
    .split('`')
    .map((part, i) => (i % 2 === 1 ? `<code>${escapeText(part)}</code>` : plain(part, known)))
    .join('');
}

function plain(text: string, known: ReadonlySet<string>): string {
  let out = escapeText(text);
  out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_m, label: string, target: string) => link(label, target, known));
  if (/\*\*|\]\(/.test(out)) throw new Error(`docs: markup this renderer does not read in: ${text}`);
  return out;
}

/** The fence languages the build highlights, and the language each is highlighted as. */
const FENCE_LANGS: Readonly<Record<string, string | undefined>> = { toml: 'toml', sh: 'sh', python: 'python', js: 'js', go: 'go', rust: 'rust', java: 'java' };

function link(label: string, target: string, known: ReadonlySet<string>): string {
  // The text was escaped before the links were read: undo that for the target, which is escaped again as an attribute.
  const t = target.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
  // An address on this site is a link inside it, so that the same Markdown reads right served as it is.
  const own = new RegExp(`^https://${DOMAIN.replace(/\./g, '\\.')}(/[A-Za-z0-9_/.#-]*)$`).exec(t);
  if (own !== null) return `<a href="${escapeText(own[1])}">${label}</a>`;
  if (/^https:\/\//.test(t)) return `<a href="${escapeText(t)}">${label}</a>`;
  const m = /^([a-z0-9_-]+(?:\/[a-z0-9_-]+)?)\.md(#[a-z0-9_-]+)?$/.exec(t);
  if (m === null) throw new Error(`docs: a link to ${t}: only https links and links to another page of docs/ are read`);
  // A page that is not there yet (the metrics reference, until it lands) is its label and not a dead link.
  if (!known.has(m[1])) return label;
  return `<a href="${docRoute(m[1])}${m[2] ?? ''}">${label}</a>`;
}

function cells(row: string): string[] {
  const trimmed = row.trim().replace(/^\|/, '').replace(/\|$/, '');
  return trimmed.split('|').map((c) => c.trim());
}

/** The inside of a page for `md`: the HTML, and the title and summary. `known` is the set of stems of the docs. */
export function renderMarkdown(md: string, known: ReadonlySet<string>, name = 'a page'): Omit<Doc, 'stem' | 'md'> {
  const lines = md.replace(/\n$/, '').split('\n');
  const out: string[] = [];
  const ids = new Map<string, number>();
  let title = '';
  let summary = '';
  let i = 0;
  const fail = (why: string): never => {
    throw new Error(`docs: ${name}, line ${i + 1}: ${why}`);
  };
  const para: string[] = [];
  const flush = (): void => {
    if (para.length === 0) return;
    const text = para.join(' ');
    const first = title !== '' && summary === '';
    if (first) summary = text.replace(/`/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1').replace(/\*\*/g, '');
    out.push(`<p${first && out.length === 1 ? ' class="lede"' : ''}>${inline(text, known)}</p>`);
    para.length = 0;
  };
  for (; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === '') {
      flush();
      continue;
    }
    const fence = /^```(.*)$/.exec(line);
    if (fence !== null) {
      flush();
      const body: string[] = [];
      for (i++; i < lines.length && !lines[i].startsWith('```'); i++) body.push(lines[i]);
      if (i >= lines.length) fail('a code fence that is not closed');
      // A fence in a language the site highlights (toml, sh) is marked so the build colours it (highlight.ts).
      const info = fence[1].trim().split(/\s+/)[0];
      if (info === 'diagram') {
        out.push(renderDiagram(parseDiagram(body.join('\n'), name)));
        continue;
      }
      const lang = FENCE_LANGS[info];
      out.push(`<div class="code"><pre${lang === undefined ? '' : ` data-lang="${lang}"`}>${escapeText(body.join('\n'))}</pre></div>`);
      continue;
    }
    const heading = /^(#{1,3}) (.+)$/.exec(line);
    if (heading !== null) {
      flush();
      const level = heading[1].length;
      const text = heading[2].trim();
      if (level === 1) {
        if (title !== '') fail('a second # heading');
        title = text.replace(/`/g, '');
        out.push(`<h1>${inline(text, known)}</h1>`);
        continue;
      }
      let id = slug(text);
      const seen = (ids.get(id) ?? 0) + 1;
      ids.set(id, seen);
      if (seen > 1) id = `${id}-${seen}`;
      const tag = text === NOT_YET ? ' <span class="tag tag-planned">planned, not built</span>' : '';
      out.push(`<h${level} id="${id}">${inline(text, known)}${tag}</h${level}>`);
      continue;
    }
    if (line.startsWith('> ')) {
      flush();
      const quote: string[] = [];
      for (; i < lines.length && lines[i].startsWith('> '); i++) quote.push(lines[i].slice(2));
      i--;
      out.push(`<div class="notice notice-warn"><p>${inline(quote.join(' '), known)}</p></div>`);
      continue;
    }
    const item = /^(- |\d+\. )/.exec(line);
    if (item !== null) {
      flush();
      const ordered = item[1] !== '- ';
      const items: string[] = [];
      for (; i < lines.length && /^(- |\d+\. )/.test(lines[i]); i++) items.push(lines[i].replace(/^(- |\d+\. )/, ''));
      i--;
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>`, ...items.map((t) => `<li>${inline(t, known)}</li>`), `</${tag}>`);
      continue;
    }
    if (line.startsWith('|')) {
      flush();
      const rows: string[] = [];
      for (; i < lines.length && lines[i].startsWith('|'); i++) rows.push(lines[i]);
      i--;
      if (rows.length < 3 || !/^\|[\s:|-]+\|$/.test(rows[1])) fail('a table needs a header row and a separator row');
      const head = cells(rows[0]);
      const html = ['<div class="table-wrap"><table>', `<thead><tr>${head.map((c) => `<th>${inline(c, known)}</th>`).join('')}</tr></thead>`, '<tbody>'];
      for (const row of rows.slice(2)) {
        const c = cells(row);
        if (c.length !== head.length) fail(`a table row of ${c.length} cells under a header of ${head.length}`);
        // A middle column carries its header as a label, so that the row can be stacked on a phone (key over its type and default over its text).
        html.push(`<tr>${c.map((x, k) => `<td${k > 0 && k < head.length - 1 && head[k] !== '' ? ` data-label="${escapeText(head[k].replace(/`/g, ''))}"` : ''}>${inline(x, known)}</td>`).join('')}</tr>`);
      }
      html.push('</tbody></table></div>');
      out.push(...html);
      continue;
    }
    if (/^\s/.test(line)) fail('an indented line (nested lists and indented code are not read)');
    para.push(line);
  }
  flush();
  if (title === '') fail('no # heading');
  return { title, summary, body: out.join('\n') };
}

/** The sections of `configuration.md` named in TABLE_SECTIONS, as Markdown: a '###' heading for each and its tables, and nothing of the prose. */
export function configTables(configMd: string): string {
  const sections = new Map<string, string[]>();
  let current: string[] | null = null;
  let inFence = false;
  for (const line of configMd.split('\n')) {
    if (line.startsWith('```')) inFence = !inFence;
    const h = !inFence ? /^## (.+)$/.exec(line) : null;
    if (h !== null) {
      current = TABLE_SECTIONS.includes(h[1]) ? [] : null;
      if (current !== null) sections.set(h[1], current);
    } else if (current !== null && !inFence && line.startsWith('|')) {
      current.push(line);
    }
  }
  const out: string[] = [];
  for (const title of TABLE_SECTIONS) {
    const rows = sections.get(title);
    if (rows === undefined || rows.length === 0) throw new Error(`docs: configuration.md has no table under "## ${title}"`);
    out.push(`### ${title}`, '');
    // The tables of one section are told apart by the blank line the source has between them: a table ends at the first row that is not a row.
    out.push(...rows, '');
  }
  return out.join('\n');
}

/** The landing page's Markdown with the tables copied in before its "## Examples". */
export function landingMarkdown(indexMd: string, configMd: string): string {
  const marker = '\n## Examples\n';
  const at = indexMd.indexOf(marker);
  if (at < 0) throw new Error('docs: index.md has no "## Examples" section to put the configuration tables before');
  return `${indexMd.slice(0, at)}\n${configTables(configMd)}${indexMd.slice(at)}`;
}

/** The stems of the .md files under `dir` (and of dir/examples as 'examples/<name>'), in the order the site lists them. */
function stemsOf(dir: string): string[] {
  const top = readdirSync(dir).filter((f) => f.endsWith('.md')).map((f) => f.replace(/\.md$/, ''));
  const exDir = join(dir, 'examples');
  const examples = existsSync(exDir) ? readdirSync(exDir).filter((f) => f.endsWith('.md')).map((f) => `examples/${f.replace(/\.md$/, '')}`) : [];
  const rank = (s: string, order: readonly string[]): number => (order.includes(s) ? order.indexOf(s) : order.length);
  const guide = top.filter((s) => s !== LANDING && s !== REFERENCE).sort((a, b) => rank(a, DOC_ORDER) - rank(b, DOC_ORDER) || a.localeCompare(b));
  examples.sort((a, b) => rank(a.slice('examples/'.length), EXAMPLE_ORDER) - rank(b.slice('examples/'.length), EXAMPLE_ORDER) || a.localeCompare(b));
  const lead = [LANDING, REFERENCE].filter((s) => top.includes(s));
  return [...lead, ...examples, ...guide];
}

/** Every page of `dir` (docs/*.md and docs/examples/*.md): the landing page, the reference, the examples, then the rest of the guide. */
export function loadDocs(dir: string): Doc[] {
  const stems = stemsOf(dir);
  const known = new Set(stems);
  const config = readFileSync(join(dir, `${REFERENCE}.md`), 'utf8');
  return stems.map((stem) => {
    const source = readFileSync(join(dir, `${stem}.md`), 'utf8');
    const md = stem === LANDING ? landingMarkdown(source, config) : source;
    return { stem, md, ...renderMarkdown(md, known, `docs/${stem}.md`) };
  });
}

/** The name the sidebar gives a page, when it is not the page's own title. */
const SIDEBAR_LABELS: Readonly<Record<string, string>> = { [LANDING]: 'Configure', [REFERENCE]: 'Configuration reference' };
const label = (d: Doc): string => SIDEBAR_LABELS[d.stem] ?? d.title;

/** The navigation of the guide in three groups (configure, examples, the rest of the guide), the current page marked. */
function docsNav(docs: readonly Doc[], current: string | null): string {
  const item = (d: Doc): string => `<li><a href="${docRoute(d.stem)}"${d.stem === current ? ' aria-current="page"' : ''}>${escapeText(label(d))}</a></li>`;
  const group = (title: string, list: readonly Doc[]): string => `<p class="docs-nav-title">${title}</p>\n<ul>\n${list.map(item).join('\n')}\n</ul>`;
  const lead = docs.filter((d) => d.stem === LANDING || d.stem === REFERENCE);
  const examples = docs.filter((d) => isExample(d.stem));
  const guide = docs.filter((d) => !isExample(d.stem) && !lead.includes(d));
  // A <details> that is open: on a phone src/docs-menu.js closes it and the summary opens it; with no script, or on a wide screen, it is the open list.
  return `<details class="docs-menu" open>\n<summary>Docs menu</summary>\n<nav class="docs-nav" aria-label="Guide">\n${group('Configure', lead)}\n${group('Examples', examples)}\n${group('More guides', guide)}\n</nav>\n</details>`;
}

/** The examples as cards, each with its picture small: for the docs landing page and the home page. */
export function exampleCards(docs: readonly Doc[]): string {
  const cards = docs.filter((d) => isExample(d.stem)).map((d) => {
    const fence = /```diagram\n([\s\S]*?)```/.exec(d.md);
    if (fence === null) throw new Error(`docs: ${d.stem} has no diagram`);
    const picture = renderDiagram(parseDiagram(fence[1], d.stem), false, true);
    return `<li class="card"><a href="${docRoute(d.stem)}">${picture}<h3>${escapeText(d.title)}</h3><p>${escapeText(d.summary)}</p></a></li>`;
  });
  return `<ul class="cards">\n${cards.join('\n')}\n</ul>`;
}

/** The landing page's examples list, which the Markdown writes as links, as cards. */
function withCards(body: string, docs: readonly Doc[]): string {
  const at = body.indexOf('<h2 id="examples">');
  if (at < 0) throw new Error('docs: the landing page has no Examples section');
  const list = /<ul>[\s\S]*?<\/ul>/.exec(body.slice(at));
  if (list === null) throw new Error('docs: the Examples section of the landing page has no list');
  const from = at + list.index;
  return `${body.slice(0, from)}${exampleCards(docs)}${body.slice(from + list[0].length)}`;
}

/** The pages of the docs: /docs/ is the landing page, then one for each file. Each carries its prerendered <main> in `html`. */
export function docsPages(docs: readonly Doc[]): Page[] {
  return docs.map((d) => {
    const body = d.stem === LANDING ? withCards(d.body, docs) : d.body;
    const description = d.summary.length > 200 ? `${d.summary.slice(0, 197)}...` : d.summary;
    return {
      route: docRoute(d.stem),
      source: `docs-${d.stem.replace('/', '-')}`,
      title: d.title,
      description,
      nav: 'docs' as const,
      scripts: ['copy.js', 'docs-menu.js'],
      bands: false,
      html: `<div class="docs-layout">\n${docsNav(docs, d.stem)}\n<div class="docs-body">\n${body}\n</div>\n</div>`,
    };
  });
}

/**
 * The Markdown of a page as it is served at /docs/<stem>.md: the links between pages written so that they work from
 * where the file is (an example is one directory down), and a diagram as the text of what it shows.
 */
export function rawMarkdown(d: Doc): string {
  const depth = isExample(d.stem) ? 1 : 0;
  const md = d.md.replace(/```diagram\n([\s\S]*?)```/g, (_m, src: string) => describeDiagram(parseDiagram(src, d.stem)));
  return md.replace(/\]\(([a-z0-9_/-]+\.md)(#[a-z0-9_-]*)?\)/g, (_m, target: string, frag: string | undefined) => {
    // A target is written from docs/ ('configuration.md', 'examples/cache-hot-cold.md'); from an example, one level up.
    const t = depth === 1 ? (target.startsWith('examples/') ? target.slice('examples/'.length) : `../${target}`) : target;
    return `](${t}${frag ?? ''})`;
  });
}
