import { execFileSync } from 'child_process';
import * as path from 'path';
import { Tools } from './steps';

/** The directory every command runs in: infra/site (the parent of held/). */
export const ROOT = path.resolve(__dirname, '..');

/** The real tools: processes run in infra/site. `exec` is execFileSync, a parameter so a test can stand in for it. */
export function realTools(exec: typeof execFileSync = execFileSync, log: (line: string) => void = console.log): Tools {
  return {
    capture: (cmd, args) => exec(cmd, args, { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }),
    stream: (cmd, args) => {
      exec(cmd, args, { cwd: ROOT, stdio: 'inherit' });
    },
    log,
  };
}
