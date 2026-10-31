// Where tierstore is downloaded from and how a download is verified: this repository, with the signer workflow the
// release document names.
//
// Catches: an install page, an install guide or a README that sends the reader to another repository's releases or to
// another repository's verification steps, and a verify command whose signer differs from the one docs/ci-and-release.md
// gives (a download would then be checked against the wrong workflow, or not at all).

import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { test } from 'node:test';
import { DOCS, read, REPO, SITE } from './support';

const SIGNER = 'komira-ai/tierstore/.github/workflows/artifacts.yml';
const VERIFY_FLAGS = ['--repo komira-ai/tierstore', `--signer-workflow ${SIGNER}`, '--source-ref refs/heads/main', '--source-digest <commit>'];

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

test('the install page and the install guide give the verify command with this repository as signer', () => {
  for (const [name, text] of pages.slice(0, 1).concat(pages.slice(2, 3))) {
    for (const flag of VERIFY_FLAGS) assert.ok(text.replace(/&lt;/g, "<").replace(/&gt;/g, ">").includes(flag), `${name} lacks ${flag}`);
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
