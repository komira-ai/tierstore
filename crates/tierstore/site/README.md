# site: the tierstore.dev website

The stack is plain TypeScript compiled by `tsc`, a build program (`tools/`) that prerenders the pages, no framework, no bundler, no
runtime dependencies. `package.json` lists two development packages, pinned exactly; Node is the exact version in
`.node-version`.

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm run typecheck
npm test            # compiles, then runs test/*.test.ts
npm run build       # writes dist/
npm run golden      # after a change meant to alter a hand-made page or asset: rewrites test/golden.manifest
```

## Why it is here

The site lives with the crate (`crates/tierstore/site`). The tests find the repository root by looking for
`.github/workflows` above them, and compute the site's path from it (`test/support.ts`);
`.github/workflows/tierstore-site.yml` names the directory in three places (the `paths:` filters, `working-directory`
and `node-version-file`), `.github/workflows/tierstore-site-publish.yml` in those, in the artifact path and in the two
files its deploy job checks out, and `test/supply-chain.test.ts` checks that the workflows and the site agree. The test
`csp.test.ts` reads the Content-Security-Policy from `served-policy.ts`, a copy of the policy the site's hosting sets as
the header (`CONTENT_SECURITY_POLICY` in `infra/site/lib/site-stack.ts`); the infrastructure's own test fails when the two
differ, so a change is made in both in the same pull request.

| Path | What |
|---|---|
| `../docs/*.md`, `../docs/examples/*.md` | the operator guide, rendered by `tools/docs.ts` into `/docs/<name>/` and `/docs/examples/<name>/` (a page for each file, so a new file is a new page; `Not in a release yet` sections are marked). `../docs/index.md` is the landing page, `/docs/`: the tables of the configuration reference are copied into it from `configuration.md` when the pages are loaded (`TABLE_SECTIONS`), and its Examples list is drawn as cards. A ```` ```diagram ```` fence is a small description of boxes and arrows that `tools/diagram.ts` draws as an inline SVG (see Diagrams). Each page is also served as the Markdown it is made from, at `/docs/<name>.md` and `/docs/examples/<name>.md`, with `llms.txt` (the llmstxt.org format) and `llms-full.txt` (every page in one file) from `tools/llms.ts`; `check-dist.ts` and `publish.ts` let exactly `.md` and `.txt` through besides the pages, scripts and styles, and `publish.ts` uploads them as `text/markdown; charset=utf-8` and `text/plain; charset=utf-8` with the pages' short cache. The crate's test `src/server/docs_examples_tests.rs` loads every TOML file of the landing page and the examples through the configuration loader. The crate's test `src/server/docs_tests.rs` holds the guide to the code, so the pages cannot say what the code does not. A minimal renderer for the subset the guide uses, no dependency; it refuses what it does not read |
| `content/*.html` | the page copy: the inside of each page's `<main>`. Each claim comes from the crate's documentation, `packaging/README.md` or the code; a page says where |
| `served-policy.ts` | the Content-Security-Policy the site is served under, a copy of the policy `infra/site` sets as the header (`infra/site/test/site-stack.test.ts` holds the two equal) |
| `static/theme.css` | the theme (the komira.ai look); `test/theme.test.ts` pins its sha256 |
| `static/site.css` | this site's own rules, loaded after the theme |
| `static/install.sh` | the install script, served at `https://tierstore.dev/install.sh`: POSIX `sh`, flags only (`--version`, `--prefix`, `--system`, `--base-url`). It reads `releases/latest.json` (or takes the version), downloads the platform's tarball and `SHA256SUMS` from `releases/<V>/`, refuses unless the SHA-256 matches, and places the binary; it starts nothing. The build copies it to `dist/install.sh` byte for byte; `check-dist.ts` lets this one `.sh` file through (at the root, starting `#!/bin/sh`, within 32 KiB) and `publish.ts` uploads it as `text/x-shellscript` with the pages' short cache. Its behaviour is held by `crates/xtask/tests/install_script.rs`, which runs it with `sh` against a fake release tree and a fake `curl`; `test/golden.manifest` pins its digest, so an edit to the script is followed by `npm run golden` |
| `src/copy.ts`, `src/reveal.ts`, `src/docs-menu.ts`, `src/tabs.ts` (with `src/tab-logic.ts`) | copy buttons on command blocks; scroll reveal; the docs menu (open in the markup, closed below 60rem by the script); tabs (the arrow keys, the remembered choice in `localStorage`, all in `try`/`catch`) |
| `tools/` | `pages.ts` (routes, titles, the white-paper URL), `layout.ts` (the page shell, with the `> tierstore` mark), `build.ts`, `diagram.ts`, `llms.ts`, `check-dist.ts`, `links.ts`, `csp.ts`, `publish.ts` (uploads a built `dist/`; see Deploying) |
| `test/` | golden output (`golden.manifest` guards the hand-made pages and assets; the files made from the guide (`docs/<name>/index.html`, `docs/index.html`, `docs/<name>.md`, `llms.txt`, `llms-full.txt`) are left out of it and held by `docs.test.ts`, `examples.test.ts` and the crate's `docs_tests.rs`, so a guide edit needs no new manifest; the home page's Examples band takes each example's title, first paragraph and diagram, so an edit to those moves the home page's line), the docs pages (`docs.test.ts`), the examples, the diagrams and the files for programs (`examples.test.ts`), links, Content-Security-Policy, theme digest, the optional footer link (`footer-slot.test.ts`), the "not built" statements and demo strings held to the crate source (`claims.test.ts`), public-repo scan, supply-chain policy, the publish program against stand-ins for the token service and the `aws` CLI (`publish.test.ts`) |

## The demo

There is no recording: a recorded session of tierstore does not exist in the repository, and the page may not carry
output that nobody produced. `content/demo.html` is a transcript whose output lines are the strings the program prints,
read from its source, and each block names the file. Values that differ on every run are written as `<N>` and
`<version>`.

## The technical report

`REPORT_URL` in `tools/pages.ts` is the report on tierstore at the main site (`https://komira.ai/research/tierstore`). It is the last of the three links in the header (`NAV`: Install, Docs, Why) and is in the footer too. The report itself is maintained with the main site, not in this repository. The Demo and `llms.txt` are in the footer (`FOOTER_LINKS`). `/get-started/` and `/getting-started/` are stub pages that link and redirect to `/install/`, and `/why/` is a stub that links and redirects to the report, so no address ever breaks.

## Install

`/install/` has the four ways to install in tabs (Script, Homebrew, apt, Tarball), one command block each, with when each is available (the first three from the next release; the tarball from GitHub works today), then a link to the docs, then Verify: the checks of a download (SHA256SUMS, `gh attestation verify`, cosign). `test/install.test.ts` and `crates/xtask` hold those commands to what a release is checked with.

## Diagrams

`tools/diagram.ts` draws a ```` ```diagram ```` fence of a docs page: `alt:` (the caption), `node <id> <column 0-2> <row, in halves> "label" "second line" [main|hot|remote|ghost]` and `edge <from> <to> "label" [both] [dashed]`. A box off the grid, a name used twice, an arrow to nothing or words too long for a box is a build error. The picture is an inline SVG that scales with the page; its words are set larger under 40rem so that they stay readable on a phone, and `test/examples.test.ts` holds its colours to 4.5:1.

## Look and motion

The look is komira.ai's: palette, type scale, link underline, code panel and the `k-fade`, `k-rise`, `k-spin`, `k-grow` keyframes. The face is a system sans-serif stack closest to Sohne; no font file is served. The home page's picture of requests (inline SVG, CSS keyframes, no script) shows a still frame under `prefers-reduced-motion`. `src/reveal.ts` is the scroll reveal; nothing is hidden without it. Code blocks marked `data-lang` are coloured by the build (`tools/highlight.ts`) and `<div class="tabs">` groups become ARIA tabs (`tools/tabs.ts`; `src/tabs.ts` shows the tab list, moves with the arrow keys and remembers the choice; with no script every panel is on the page, stacked). The languages are toml, sh, rust, python, js, go and java; the colours are in `static/site.css` and `test/highlight.test.ts` holds them to 4.5:1 on the code panel. The theme has no dark set, so there is none.

## Deploying

A push to `main` that changes the site is published by `.github/workflows/tierstore-site-publish.yml`, and so is a manual run of
that workflow on `main`. A GitHub environment holds the settings, the job proves its identity
to AWS with an OpenID Connect token (no stored key), and a role that can only write this site's bucket and invalidate its
distribution does the upload. A pull request never reaches it.

| Job | Holds | Does |
|---|---|---|
| `build` | `contents: read`; no environment, no token | the steps of `tierstore-site.yml` (install with scripts off, type-check, the tests, the build), then hands `dist/` alone on as an artifact |
| `deploy` | `contents: read` and `id-token: write`; environment `prod-site-tierstore`; runs only for a push or a manual run on `main` and `needs` the build | checks out the commit's `tools/publish.ts` and `.node-version` (sparse, no credentials kept), downloads the pages, and runs the program from source with Node's type stripping: no install, no build |

`tools/publish.ts` is what holds the credentials, so it never comes from the build job, which runs dependency code. It imports
`node:` built-ins only and uses only syntax Node can strip (`erasableSyntaxOnly` in `tsconfig.json`).

The publish program asks the runner for the job's token, exchanges it with `aws sts assume-role-with-web-identity` for credentials
that last an hour, uploads the assets (immutable) then the pages (short cache) then `version.txt`, sweeps what the build no
longer has, and invalidates the distribution. It refuses a `dist/` with fewer than 12 pages or 24 files (`MIN_HTML_PAGES` and `MIN_FILES`; a test keeps them under the real site and above half of it), so
an empty build can never make the sweep empty the bucket, and a file type other than `.html`, `.js`, `.css`, `.md` and `.txt` (the sync
gives only those a content type and a cache rule; `check-dist` already refuses others). `releases/` is never touched. Because the repository and its Actions logs are
public, the role, the bucket (its name ends in the account id) and the distribution are masked before the first command.

A run that is skipped, replaced or red does not publish later by itself: GitHub keeps only the newest pending run of the
workflow's group, so a pending run can be replaced by a later push or dispatch, and a run that failed stays failed. After any
skipped, replaced or failed run, run the workflow again by hand from `main` (Actions, tierstore-site-publish, Run workflow). A
merge that changes nothing under the site's paths does not start it at all.

The settings are three variables of the environment `prod-site-tierstore`, set once by whoever sets the site up
(`TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID`, `TIERSTORE_SITE_PUBLISH_ROLE_ARN`); there is no secret. An unset
variable is an empty flag, which the program refuses. The environment, the role and its trust, the bucket, the distribution and
the certificate are created once by an operator with their own AWS credentials, from this repository's `infra/site` (its README lists the steps); the workflow only publishes.

The workflow lint (`cargo test -p xtask --test workflows`) allows `id-token: write` for this job and this environment alone.

The output and its rules are those of `site/README.md`: `dist/` holds the pages and `assets/<id>/`, served under
`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'`.

## Theme

`static/theme.css` is the komira.ai look shared by the open-source sites. A change to it moves its digest in `test/theme.test.ts` in the same commit.
