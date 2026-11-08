// The install script (static/install.sh, served at /install.sh) in the build: copied to the root of dist/ byte for byte,
// and the one non-page file check-dist.ts lets through. Its behaviour is held by crates/xtask/tests/install_script.rs.
//
// Catches: a build that does not ship the script, or ships a changed copy; a dist/ without it passing the check; a second
// .sh file, the script below the root, or a file under that name that is not a shell script (or is too large) passing.

import assert from 'node:assert/strict';
import { readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { checkDist, INSTALL_SCRIPT, INSTALL_SCRIPT_MAX_BYTES } from '../tools/check-dist';
import { buildReal, copyTree, SITE } from './support';

const dist = buildReal('install-script-build');

test('the build puts static/install.sh at the root of dist/, byte for byte, and the result passes the check', () => {
  assert.equal(INSTALL_SCRIPT, 'install.sh');
  assert.deepEqual(readFileSync(join(dist, 'install.sh')), readFileSync(join(SITE, 'static', 'install.sh')));
  checkDist(dist);
});

test('the shipped script is a POSIX sh script within the budget', () => {
  const text = readFileSync(join(dist, 'install.sh'), 'utf8');
  assert.ok(text.startsWith('#!/bin/sh\n'));
  assert.ok(Buffer.byteLength(text) < INSTALL_SCRIPT_MAX_BYTES);
});

function refused(name: string, edit: (t: string) => void, why: RegExp): void {
  test(`check-dist refuses ${name}`, () => {
    const t = copyTree(dist, `install-script-${name.replace(/[^a-z0-9]/gi, '_')}`);
    edit(t);
    assert.throws(() => checkDist(t), why);
  });
}

refused('a dist/ without install.sh', (t) => rmSync(join(t, 'install.sh')), /missing install\.sh/);
refused('install.sh that is not a shell script', (t) => writeFileSync(join(t, 'install.sh'), '<html></html>\n'), /install\.sh: does not start with #!\/bin\/sh/);
refused('an install.sh over the budget', (t) => writeFileSync(join(t, 'install.sh'), `#!/bin/sh\n${'#'.repeat(INSTALL_SCRIPT_MAX_BYTES)}\n`), /install\.sh is \d+ bytes; the limit is/);
refused('a second shell script', (t) => writeFileSync(join(t, 'other.sh'), '#!/bin/sh\n'), /other\.sh: only \.html, \.js and \.css files are published/);
refused('install.sh below the root', (t) => writeFileSync(join(t, 'docs', 'install.sh'), '#!/bin/sh\n'), /docs\/install\.sh: only \.html, \.js and \.css files are published/);
