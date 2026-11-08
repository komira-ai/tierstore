// The pages of the site: where each is served, which file under content/ holds its <main>, what its head says and
// which scripts it loads. The content files are the page copy as written for the site (content/*.html: the inside of
// <main>); the document around them is layout.ts.

export type NavKey = 'start';

export interface Page {
  /** The URL path: '/' or '/name/' or '/404.html'. */
  route: string;
  /** content/<source>.html, the inside of <main>. */
  source: string;
  /** The <title> without the site suffix. */
  title: string;
  description: string;
  /** The navigation entry this page is. */
  nav?: NavKey;
  /** Browser modules (names under assets/<id>/) this page loads, in order. */
  scripts: readonly string[];
  /** The home runs in bands that each carry their own .wrap; every other page sits in one. */
  bands: boolean;
  noindex?: boolean;
  /** A page that moved: the route it now lives at. The page says so, links there and sends a browser there. */
  redirect?: string;
  /** The inside of <main>, when the page is generated (docs.ts) and not copy under content/. */
  html?: string;
}

export const SITE_NAME = 'tierstore';
export const DOMAIN = 'tierstore.dev';
/** Where the source and the releases are. */
export const REPO_URL = 'https://github.com/komira-ai/tierstore';
export const CRATE_URL = `${REPO_URL}/tree/main/crates/tierstore`;
export const PACKAGING_URL = `${REPO_URL}/blob/main/packaging/README.md`;
export const RELEASES_URL = `${REPO_URL}/releases`;

/** The technical report on tierstore, at the main site. The header and the footer link to it; it opens as any link does. */
export const WHITE_PAPER_URL = 'https://komira.ai/whitepapers/tierstore';

/** The top navigation, in order: exactly these two. Get Started is a page of this site and the report is not. */
export const NAV: readonly { key: NavKey | null; label: string; href: string }[] = [
  { key: 'start', label: 'Get Started', href: '/get-started/' },
  { key: null, label: 'Technical Report', href: WHITE_PAPER_URL },
];

/** The footer's links to this site's other pages, in order (the source and the report follow them). */
export const FOOTER_LINKS: readonly { label: string; href: string }[] = [
  { label: 'Docs', href: '/docs/' },
  { label: 'Demo', href: '/demo/' },
  { label: 'Why tierstore', href: '/why/' },
];

export const PAGES: readonly Page[] = [
  {
    route: '/',
    source: 'index',
    title: 'A hot tier over a cold store, behind one S3 endpoint',
    description: 'tierstore is an S3 server over a hot and a cold tier of S3-compatible stores. Open source, Apache 2.0.',
    scripts: ['copy.js', 'reveal.js'],
    bands: true,
  },
  { route: '/get-started/', source: 'get-started', title: 'Get started', description: 'Install tierstore, write a configuration file, start it and put and get an object.', nav: 'start', scripts: ['copy.js', 'reveal.js', 'tabs.js'], bands: false },
  { route: '/install/', source: 'moved-install', title: 'Install', description: 'The install steps are on the Get started page.', scripts: ['copy.js'], bands: false, noindex: true, redirect: '/get-started/#install' },
  { route: '/getting-started/', source: 'moved-getting-started', title: 'Get started', description: 'The first steps are on the Get started page.', scripts: ['copy.js'], bands: false, noindex: true, redirect: '/get-started/' },
  { route: '/demo/', source: 'demo', title: 'Demo', description: 'A start, the probes and a drain of tierstore, as the code prints them.', scripts: ['copy.js', 'reveal.js'], bands: false },
  { route: '/why/', source: 'why', title: 'Why tierstore', description: 'The problem tierstore solves and how the tiers work.', scripts: ['copy.js', 'reveal.js'], bands: false },
  {
    route: '/404.html',
    source: '404',
    title: 'Page not found',
    description: 'There is no such page on tierstore.dev.',
    scripts: ['copy.js'],
    bands: false,
    noindex: true,
  },
];

/** The file a route is written to: '/' is index.html, '/a/b/' is a/b/index.html, '/404.html' is itself. */
export function outPath(route: string): string {
  const rel = route.replace(/^\//, '');
  return rel === '' || rel.endsWith('/') ? `${rel}index.html` : rel;
}

/**
 * The pages of the page copy name each other as '<source>.html' (the copy was written for a flat directory). Writes
 * those as the routes they are served at; an internal '.html' link that names no page is an error.
 */
export function rewriteLinks(html: string, pages: readonly Page[]): string {
  const bySource = new Map(pages.map((p) => [p.source, p.route]));
  return html.replace(/(\bhref=")([a-z0-9-]+)\.html(#[^"]*)?"/g, (_m, pre: string, name: string, frag: string | undefined) => {
    const route = bySource.get(name);
    if (route === undefined) throw new Error(`pages: a link to ${name}.html, which is no page`);
    return `${pre}${route}${frag ?? ''}"`;
  });
}
