// Where tierstore is downloaded from and how a download is verified: this repository, with the signer workflow the
// release document names.
//
// Catches: an install page, an install guide or a README that sends the reader to another repository's releases or to
// another repository's verification steps, and a verify command whose signer differs from the one docs/ci-and-release.md
// gives (a download would then be checked against the wrong workflow, or not at all).

import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { join, relative } from 'node:path';
import { test } from 'node:test';
import { DOCS, read, REPO, SITE, SITE_REL } from './support';

const SIGNER = 'komira-ai/tierstore/.github/workflows/artifacts.yml';
const VERIFY_FLAGS = ['--repo komira-ai/tierstore', `--signer-workflow ${SIGNER}`, '--source-ref refs/heads/main', '--source-digest <commit>', '--deny-self-hosted-runners'];

const pages: Array<[string, string]> = [
  ['site install page', read(SITE, 'content', 'install.html')],
  ['site home page', read(SITE, 'content', 'index.html')],
  ['install guide', read(DOCS, 'install.md')],
  ['repository README', read(REPO, 'README.md')],
];

test('the signer named in the release document is a workflow of this repository', () => {
  assert.ok(existsSync(`${REPO}/.github/workflows/artifacts.yml`));
  assert.ok(read(REPO, 'docs', 'ci-and-release.md').includes(`--signer-workflow ${SIGNER}`));
});

test('the release document gives the verify command with the flags of the install guide', () => {
  const doc = read(REPO, 'docs', 'ci-and-release.md');
  for (const flag of VERIFY_FLAGS) assert.ok(doc.includes(flag), `docs/ci-and-release.md lacks ${flag}`);
});

test('the install page and the install guide give the verify command with this repository as signer', () => {
  for (const [name, text] of pages.slice(0, 1).concat(pages.slice(2, 3))) {
    for (const flag of VERIFY_FLAGS) assert.ok(text.replace(/&lt;/g, '<').replace(/&gt;/g, '>').includes(flag), `${name} lacks ${flag}`);
    assert.match(text, /gh attestation verify/, `${name} lacks the command`);
  }
});

test('downloads point to this repository\'s releases and no page sends the reader to kbf for a release or a verification', () => {
  assert.match(pages[0][1], /github\.com\/komira-ai\/tierstore\/releases/);
  assert.match(pages[2][1], /github\.com\/komira-ai\/tierstore\/releases/);
  for (const [name, text] of pages) {
    assert.doesNotMatch(text, /komira-ai\/kbf\/(releases|blob)/, `${name} links to a kbf release or document`);
    assert.doesNotMatch(text, /kbf (release|repository)/i, `${name} says tierstore comes with a kbf release`);
    assert.doesNotMatch(text, /(releases|code|docs) (are|is) in kbf/i, `${name} says the releases are in kbf`);
  }
});

test('the install guide says versions up to 0.2.0 were built in kbf', () => {
  assert.match(pages[2][1], /up to (and including )?0\.2\.0[^.]*built in kbf|built in kbf[^.]*0\.2\.0/i);
  assert.match(pages[0][1], /0\.2\.0/);
});

// The three checks of a download (integrity, then authenticity with gh or with cosign), held to the signer above.
//
// Catches: an install page or guide that drops the checksum line or its --ignore-missing (the check then fails on the
// files of the other platform), a cosign command without --type slsaprovenance1 (cosign then refuses every bundle), with
// another issuer, or with an identity regexp that names another workflow or branch than the gh command and
// docs/ci-and-release.md (or matches more than that one signer), and an offline gh check without its bundle or its
// trusted root.

/** GitHub Actions' OIDC issuer, which signs the certificate of every attestation (xtask's bundles::ISSUER). */
const ISSUER = 'https://token.actions.githubusercontent.com';

const INSTALL: Array<[string, string]> = [pages[0], pages[2]];
const RELEASE_DOC = read(REPO, 'docs', 'ci-and-release.md');

/** The command lines of a page: markup out, entities decoded, continued lines joined, a leading `$ ` prompt dropped. */
function commands(page: string): string[] {
  return page
    .replace(/<\/?(?:span|pre|div|code|p|h3|li)\b[^>]*>/g, '')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&amp;/g, '&')
    .replace(/\\\n\s*/g, ' ')
    .split('\n')
    .map((l) => l.trim().replace(/^\$ /, ''));
}

/** The command lines of `page` that start with `start` and hold `has`. */
const lines = (page: string, start: string, has = ''): string[] => commands(page).filter((c) => c.startsWith(start) && c.includes(has));

/** The value after `flag` in a command line, single quotes taken off. */
function valueOf(line: string, flag: string): string {
  const m = new RegExp(`(?:^| )${flag} ('[^']*'|\\S+)`).exec(line);
  assert.ok(m !== null, `no ${flag} in: ${line}`);
  return m[1].replace(/^'(.*)'$/, '$1');
}

/** The signer workflow and branch of the online gh command of `page`. */
function signerOf(page: string): { workflow: string; ref: string } {
  const [line] = lines(page, 'gh attestation verify', '--signer-workflow');
  assert.ok(line !== undefined, 'no gh attestation verify command');
  return { workflow: valueOf(line, '--signer-workflow'), ref: valueOf(line, '--source-ref') };
}

test('the install page and the install guide give the cosign check with the provenance type, the Actions issuer and the signer workflow on main', () => {
  assert.ok(existsSync(`${REPO}/.github/workflows/artifacts.yml`), 'the signer workflow the regexp names is not in the repository');
  for (const [name, text] of INSTALL) {
    const found = lines(text, 'cosign verify-blob-attestation');
    assert.equal(found.length, 1, `${name}: one cosign command`);
    const [line] = found;
    assert.equal(valueOf(line, '--bundle'), '<tarball>.sigstore.json', name);
    assert.equal(valueOf(line, '--type'), 'slsaprovenance1', name);
    assert.equal(valueOf(line, '--certificate-oidc-issuer'), ISSUER, name);
    assert.match(valueOf(line, '--certificate-identity-regexp'), /artifacts\\\.yml@refs\/heads\/main\$$/, name);
    assert.ok(line.endsWith(' <tarball>'), `${name}: the cosign command checks <tarball>`);
  }
});

test('the identity regexp of the cosign check is exactly the signer and branch that the gh command and the release document pin', () => {
  const pinned = signerOf(RELEASE_DOC);
  assert.deepEqual(pinned, { workflow: SIGNER, ref: 'refs/heads/main' });
  const identity = `https://github.com/${pinned.workflow}@${pinned.ref}`;
  for (const [name, text] of INSTALL) {
    assert.deepEqual(signerOf(text), pinned, `${name}: the gh command and docs/ci-and-release.md name another signer`);
    const [line] = lines(text, 'cosign verify-blob-attestation');
    const regexp = valueOf(line, '--certificate-identity-regexp');
    // Anchored at both ends, and naming the signer and branch once its escapes are taken off.
    assert.equal(regexp.replace(/\\\./g, '.'), `^${identity}$`, name);
    // The certificate identity of the real signer matches; a renamed workflow, another branch or a look-alike does not.
    const re = new RegExp(regexp);
    assert.ok(re.test(identity), name);
    for (const other of [
      identity.replace('artifacts.yml', 'artifacts-yml'),
      identity.replace('artifacts.yml', 'release.yml'),
      identity.replace('refs/heads/main', 'refs/heads/main2'),
      identity.replace('refs/heads/main', 'refs/tags/main'),
      identity.replace('komira-ai/tierstore/', 'komira-ai/tierstore-fork/'),
      `${identity}/x`,
    ]) assert.ok(!re.test(other), `${name}: the regexp also accepts ${other}`);
  }
});

test('the checksum line of the install page and the install guide skips the files of the other platform', () => {
  for (const [name, text] of INSTALL) {
    assert.deepEqual(lines(text, 'sha256sum'), ['sha256sum -c --ignore-missing SHA256SUMS'], name);
  }
});

test('the offline gh check of the install page and the install guide takes the bundle and a trusted root fetched beforehand', () => {
  for (const [name, text] of INSTALL) {
    assert.deepEqual(lines(text, 'gh attestation trusted-root'), ['gh attestation trusted-root > trusted_root.jsonl'], name);
    const found = lines(text, 'gh attestation verify', '--custom-trusted-root');
    assert.equal(found.length, 1, `${name}: one offline gh command`);
    const [line] = found;
    assert.equal(valueOf(line, '--bundle'), '<tarball>.sigstore.json', name);
    assert.equal(valueOf(line, '--custom-trusted-root'), 'trusted_root.jsonl', name);
    for (const flag of VERIFY_FLAGS) assert.ok(line.includes(flag), `${name}: the offline command lacks ${flag}`);
  }
});

test('every document of the repository that the install pages, the packaging notes and the site scan name exists', () => {
  const files: Array<[string, string]> = [
    ['crates/tierstore/docs/install.md', DOCS],
    [`${SITE_REL}/content/install.html`, SITE],
    ['packaging/README.md', `${REPO}/packaging`],
    ['packaging/tierstore.service', `${REPO}/packaging`],
    [`${SITE_REL}/test/hygiene.test.ts`, SITE],
  ];
  let named = 0;
  for (const [file, dir] of files) {
    const text = read(REPO, file);
    assert.ok(!text.includes('docs/artifacts.md'), `${file} names docs/artifacts.md, which is not a page of this repository`);
    // A path from the repository root (`docs/x.md`), a link to a file of this repository, and a relative Markdown link.
    const paths = [
      ...[...text.matchAll(/(?<![\w/.-])docs\/[a-z0-9-]+\.md\b/g)].map((m) => join(REPO, m[0])),
      ...[...text.matchAll(/github\.com\/komira-ai\/tierstore\/blob\/main\/([^\s"')#]+)/g)].map((m) => join(REPO, m[1])),
      ...[...text.matchAll(/\]\(([^)\s:#]+)(?:#[^)]*)?\)/g)].map((m) => join(dir, m[1])),
    ];
    for (const p of paths) assert.ok(existsSync(p), `${file} names ${relative(REPO, p)}, which does not exist`);
    named += paths.length;
  }
  assert.ok(named >= 6, `only ${named} paths were found to check`);
});

// The verify block as a whole: each command checked on its own line, and the three checks in their order.
//
// Catches: an online gh command that loses --source-digest or --deny-self-hosted-runners while the offline command
// below it still has them (a page-wide search would pass), an offline gh command without the tarball to check, and an
// integrity check moved after an authenticity check, or the cosign check moved before the gh one.

/** The online gh command's file: the x86_64 tarball of a commit, as the release document gives it. */
const ONLINE_FILE = 'tierstore-<commit>-linux-x86_64.tar.gz';

/** The file a `gh attestation verify` line checks: its first word after the subcommand. */
const operandOf = (line: string): string => line.slice('gh attestation verify '.length).split(' ')[0];

/** What is wrong with the verify block given by `cmds` (the command lines of a page, in page order); empty when nothing is. */
function verifyProblems(cmds: string[]): string[] {
  const problems: string[] = [];
  const at = (pred: (c: string) => boolean): number[] => cmds.flatMap((c, i) => (pred(c) ? [i] : []));
  const sums = at((c) => c.startsWith('sha256sum'));
  const online = at((c) => c.startsWith('gh attestation verify') && !c.includes('--bundle'));
  const cosign = at((c) => c.startsWith('cosign verify-blob-attestation'));
  const offline = at((c) => c.startsWith('gh attestation verify') && c.includes('--bundle'));
  for (const [what, found] of [['sha256sum', sums], ['online gh', online], ['cosign', cosign], ['offline gh', offline]] as const) {
    if (found.length !== 1) problems.push(`${found.length} ${what} commands, not one`);
  }
  if (problems.length > 0) return problems;
  const [o, f] = [cmds[online[0]], cmds[offline[0]]];
  if (operandOf(o) !== ONLINE_FILE) problems.push(`the online gh command checks ${operandOf(o)}, not ${ONLINE_FILE}`);
  if (operandOf(f) !== '<tarball>') problems.push(`the offline gh command checks ${operandOf(f)}, not <tarball>`);
  for (const flag of VERIFY_FLAGS) {
    if (!` ${o} `.includes(` ${flag} `)) problems.push(`the online gh command lacks ${flag}`);
    if (!` ${f} `.includes(` ${flag} `)) problems.push(`the offline gh command lacks ${flag}`);
  }
  if (!(sums[0] < online[0] && online[0] < cosign[0] && cosign[0] < offline[0])) {
    problems.push('the checks are not in the order: integrity, authenticity with gh, authenticity without gh');
  }
  return problems;
}

test('the install page and the install guide give each verify command whole, and the three checks in order', () => {
  for (const [name, text] of INSTALL) assert.deepEqual(verifyProblems(commands(text)), [], name);
});

test('the verify block check can fail: a flag dropped from one gh command, a missing tarball, a check out of order', () => {
  for (const [name, text] of INSTALL) {
    const cmds = commands(text);
    const isOnline = (c: string): boolean => c.startsWith('gh attestation verify') && !c.includes('--bundle');
    const isOffline = (c: string): boolean => c.startsWith('gh attestation verify') && c.includes('--bundle');
    const said = (mutated: string[], what: string): boolean => verifyProblems(mutated).some((p) => p.includes(what));
    for (const flag of VERIFY_FLAGS) {
      const dropped = cmds.map((c) => (isOnline(c) ? c.replace(` ${flag}`, '') : c));
      assert.ok(said(dropped, `the online gh command lacks ${flag}`), `${name}: dropping ${flag} from the online command passes`);
    }
    const noFile = cmds.map((c) => (isOffline(c) ? c.replace(' <tarball> ', ' ') : c));
    assert.ok(said(noFile, 'the offline gh command checks --repo, not <tarball>'), `${name}: an offline command without its tarball passes`);
    const sums = cmds.findIndex((c) => c.startsWith('sha256sum'));
    const late = [...cmds.slice(0, sums), ...cmds.slice(sums + 1), cmds[sums]];
    assert.ok(said(late, 'not in the order'), `${name}: the integrity check last passes`);
    const swapped = cmds.map((c) => (isOnline(c) ? cmds.find((d) => d.startsWith('cosign')) ?? c : c.startsWith('cosign') ? cmds.find(isOnline) ?? c : c));
    assert.ok(said(swapped, 'not in the order'), `${name}: the cosign check before the gh check passes`);
  }
});
