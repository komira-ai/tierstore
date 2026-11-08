// The page wrapper every page shares: the document head, a skip link, the site header (the "> tierstore" mark and the
// navigation), <main id="main"> and the footer, as THEME.md lays them out.
//
// Nothing is inline: two stylesheets (the shared theme, then the site's own) and module scripts (which a browser
// defers), no style attribute, no event handler. check-dist.ts refuses the pages if that stops being true.

import { FOOTER_LINKS, NAV, type Page, RELEASES_URL, REPO_URL, SITE_NAME, WHITE_PAPER_URL } from './pages';

/** The stylesheets every page loads, in order: the theme (byte for byte the shared file), then the site's own. */
export const STYLESHEETS = ['theme.css', 'site.css'] as const;

export interface Assets {
  /** '/assets/<id>/' */
  base: string;
}

const ESCAPES: Readonly<Record<string, string>> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
export const escapeText = (s: string): string => s.replace(/[&<>"]/g, (c) => ESCAPES[c]);

function header(page: Page): string {
  const items = NAV.map((n) => `<li><a href="${escapeText(n.href)}"${n.key !== null && page.nav === n.key ? ' aria-current="page"' : ''}>${escapeText(n.label)}</a></li>`);
  return [
    '<header class="site-header"><div class="wrap bar">',
    `<a class="brand" href="/" aria-label="${SITE_NAME} home"><span class="gt" aria-hidden="true">&gt;</span>${SITE_NAME}</a>`,
    '<nav class="site-nav" aria-label="Site"><ul>',
    ...items,
    '</ul></nav>',
    '</div></header>',
  ].join('\n');
}

/** The footer: the pages that are not in the top navigation, the releases, the source and the technical report. */
export function footer(whitePaper: string): string {
  const links = [
    ...FOOTER_LINKS.map((l) => [l.label, l.href] as const),
    ['Releases', RELEASES_URL],
    ['Source on GitHub', REPO_URL],
    ['Technical report', whitePaper],
  ];
  return [
    `<footer class="site-footer"><div class="wrap"><p>${SITE_NAME} is licensed under the Apache License 2.0.</p>`,
    `<ul>${links.map(([label, href]) => `<li><a href="${escapeText(href)}">${escapeText(label)}</a></li>`).join('')}</ul></div></footer>`,
  ].join('\n');
}

/** The complete HTML of a page; main is the page copy (its links already rewritten to routes). */
export function renderPage(page: Page, main: string, assets: Assets): string {
  const head = [
    '<meta charset="utf-8">',
    '<meta name="viewport" content="width=device-width, initial-scale=1">',
    `<title>${escapeText(`${page.title} - ${SITE_NAME}`)}</title>`,
    `<meta name="description" content="${escapeText(page.description)}">`,
    page.noindex ? '<meta name="robots" content="noindex">' : '',
    page.redirect === undefined ? '' : `<meta http-equiv="refresh" content="0; url=${escapeText(page.redirect)}">`,
    ...STYLESHEETS.map((css) => `<link rel="stylesheet" href="${assets.base}${css}">`),
    ...page.scripts.map((js) => `<script type="module" src="${assets.base}${js}"></script>`),
  ].filter((l) => l !== '');
  return [
    '<!doctype html>',
    '<html lang="en">',
    '<head>',
    ...head,
    '</head>',
    '<body>',
    '<a class="skip" href="#main">Skip to content</a>',
    header(page),
    `<main id="main"${page.bands ? '' : ' class="wrap"'}>`,
    main.replace(/\n$/, ''),
    '</main>',
    footer(WHITE_PAPER_URL),
    '</body>',
    '</html>',
    '',
  ].join('\n');
}
