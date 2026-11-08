import { Refused, Tools, USAGE, checkEnvironment, main, parse, shellQuote } from '../held/steps';
import { cli } from '../held/cli';
import { ROOT, realTools } from '../held/tools';

const ACCOUNT = '123456789012'; // the AWS documentation placeholder
const REPO = 'example-org/example-repo';
const BASE = ['--account', ACCOUNT, '--profile', 'example-profile'];

interface Call { cmd: string; args: string[]; kind: 'capture' | 'stream' }

/** A fake: `answers` maps substrings of "cmd arg arg..." to stdout (or an Error to throw); anything else answers "{}". */
function fake(answers: Record<string, string | Error> = {}, failStream = '') {
  const calls: Call[] = [];
  const lines: string[] = [];
  const t: Tools = {
    capture(cmd, args) {
      calls.push({ cmd, args, kind: 'capture' });
      const line = [cmd, ...args].join(' ');
      for (const [k, v] of Object.entries(answers)) {
        if (k.startsWith('=') ? line.endsWith(k.slice(1)) : line.includes(k)) {
          if (v instanceof Error) throw v;
          return v;
        }
      }
      return '{}';
    },
    stream(cmd, args) {
      calls.push({ cmd, args, kind: 'stream' });
      if (failStream && [cmd, ...args].join(' ').includes(failStream)) throw new Error('failed');
    },
    log: (l) => lines.push(l),
  };
  return {
    t, calls, lines,
    text: () => lines.join('\n'),
    ran: (s: string) => calls.some((x) => [x.cmd, ...x.args].join(' ').includes(s)),
    streamed: (s: string) => calls.some((x) => x.kind === 'stream' && [x.cmd, ...x.args].join(' ').includes(s)),
  };
}

const notFound = Object.assign(new Error('Not Found'), { stderr: 'gh: Not Found (HTTP 404)' });
const envJson = (over: Record<string, unknown> = {}) =>
  JSON.stringify({ deployment_branch_policy: { protected_branches: false, custom_branch_policies: true }, ...over });
const polsJson = (...p: [string, string][]) => JSON.stringify({ branch_policies: p.map(([name, type]) => ({ name, type })) });
const ENV = ['environments/prod-site-tierstore', 'environments/prod-releases-tierstore', 'environments/release', 'environments/apt-publish', 'environments/apt-signing'];
const goodGithub = (): Record<string, string | Error> => {
  const out: Record<string, string> = {};
  for (const e of ENV) {
    out[`repos/${REPO}/${e}/deployment-branch-policies`] = polsJson(['main', 'branch']);
    out[`=repos/${REPO}/${e}`] = envJson();
  }
  return out;
};
const STACK = JSON.stringify([
  { OutputKey: 'BucketName', OutputValue: `tierstore-prod-site-${ACCOUNT}` },
  { OutputKey: 'DistributionId', OutputValue: 'EDIST123' },
  { OutputKey: 'DistributionDomainName', OutputValue: 'd111.cloudfront.net' },
  { OutputKey: 'PublishRoleArn', OutputValue: `arn:aws:iam::${ACCOUNT}:role/tierstore-prod-publish` },
  { OutputKey: 'ReleasesRoleArn', OutputValue: `arn:aws:iam::${ACCOUNT}:role/tierstore-prod-releases` },
]);
const REPO_JSON = JSON.stringify({ full_name: REPO, id: 222, owner: { id: 111 } });
const withCaller = (extra: Record<string, string | Error> = {}) => ({ 'get-caller-identity': `${ACCOUNT}\n`, ...extra });

describe('parse', () => {
  test('refuses an unknown or missing command with the usage', () => {
    expect(() => parse([])).toThrow(Refused);
    expect(() => parse(['nope'])).toThrow(USAGE);
  });

  test('reads each flag of each command', () => {
    expect(parse(['dns', ...BASE, '--apply'])).toEqual({ command: 'dns', apply: true, account: ACCOUNT, profile: 'example-profile' });
    expect(parse(['site', ...BASE, '--repo', REPO, '--oidc-provider-arn', 'arn:x'])).toMatchObject({ command: 'site', apply: false, repo: REPO, oidcProviderArn: 'arn:x' });
    expect(parse(['github', '--repo', REPO])).toEqual({ command: 'github', apply: false, repo: REPO });
  });

  test('refuses a missing required flag', () => {
    expect(() => parse(['dns', '--account', ACCOUNT])).toThrow('--profile is required');
    expect(() => parse(['site', ...BASE])).toThrow('--repo is required');
  });

  test('refuses a flag the command does not take, and a flag without a value', () => {
    expect(() => parse(['dns', ...BASE, '--repo', REPO])).toThrow('unknown flag for dns: --repo');
    expect(() => parse(['dns', ...BASE, '--force'])).toThrow('unknown flag for dns: --force');
    expect(() => parse(['dns', '--account'])).toThrow('--account needs a value');
    expect(() => parse(['dns', '--account', '--profile', 'p'])).toThrow('--account needs a value');
  });

  test('refuses a malformed account or repository', () => {
    expect(() => parse(['dns', '--account', '123', '--profile', 'p'])).toThrow('--account must be 12 digits');
    expect(() => parse(['github', '--repo', 'nope'])).toThrow('--repo must be <owner>/<name>');
  });

  test('vars and github only read: --apply is refused', () => {
    expect(() => parse(['vars', ...BASE, '--repo', REPO, '--apply'])).toThrow('vars has no --apply');
    expect(() => parse(['github', '--repo', REPO, '--apply'])).toThrow('github has no --apply');
  });
});

describe('shellQuote', () => {
  test('wraps in single quotes and writes an embedded quote as \'\\\'\'', () => {
    expect(shellQuote('a b')).toBe("'a b'");
    expect(shellQuote("it's")).toBe("'it'\\''s'");
  });
});

describe('every AWS step refuses an account that is not the profile\'s', () => {
  for (const argv of [['dns'], ['oidc-provider'], ['site', '--repo', REPO], ['vars', '--repo', REPO]]) {
    test(argv[0], () => {
      const f = fake({ 'get-caller-identity': '999999999999\n' });
      expect(() => main([...argv, ...BASE], f.t)).toThrow("profile 'example-profile' is account 999999999999, not the --account you passed");
      expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
      expect(f.calls).toHaveLength(1);
    });
  }
});

describe('oidc-provider', () => {
  const create = 'iam create-open-id-connect-provider --url https://token.actions.githubusercontent.com --client-id-list sts.amazonaws.com';

  test('does nothing when the GitHub provider exists', () => {
    const list = JSON.stringify({ OpenIDConnectProviderList: [{ Arn: `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com` }] });
    const f = fake(withCaller({ 'list-open-id-connect-providers': list }));
    main(['oidc-provider', ...BASE, '--apply'], f.t);
    expect(f.text()).toContain('already present');
    expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
  });

  test('a dry run prints the create command and runs nothing', () => {
    const f = fake(withCaller({ 'list-open-id-connect-providers': '{}' }));
    main(['oidc-provider', ...BASE], f.t);
    expect(f.text()).toContain(`DRY-RUN: aws ${create}`);
    expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
  });

  test('--apply creates the provider when only another provider exists', () => {
    const list = JSON.stringify({ OpenIDConnectProviderList: [{ Arn: `arn:aws:iam::${ACCOUNT}:oidc-provider/example.com` }] });
    const f = fake(withCaller({ 'list-open-id-connect-providers': list }));
    main(['oidc-provider', ...BASE, '--apply'], f.t);
    expect(f.streamed(create)).toBe(true);
  });
});

describe('dns', () => {
  test('a dry run installs, tests, and runs cdk diff only', () => {
    const f = fake(withCaller());
    main(['dns', ...BASE], f.t);
    expect(f.streamed('npm ci --ignore-scripts')).toBe(true);
    expect(f.streamed('npm test')).toBe(true);
    expect(f.streamed(`cdk diff tierstore-dns-zone --profile example-profile -c account=${ACCOUNT}`)).toBe(true);
    expect(f.ran('cdk deploy')).toBe(false);
    expect(f.text()).toContain('dry run');
  });

  test('--apply deploys the zone stack and prints its name servers', () => {
    const f = fake(withCaller({ 'describe-stacks': JSON.stringify([{ OutputKey: 'NameServers', OutputValue: 'ns-1.example,ns-2.example' }]) }));
    main(['dns', ...BASE, '--apply'], f.t);
    expect(f.streamed(`cdk deploy tierstore-dns-zone --profile example-profile --require-approval broadening -c account=${ACCOUNT}`)).toBe(true);
    expect(f.lines).toEqual(expect.arrayContaining(['ns-1.example', 'ns-2.example']));
  });

  test('a failing test run stops before cdk', () => {
    const f = fake(withCaller(), 'npm test');
    expect(() => main(['dns', ...BASE, '--apply'], f.t)).toThrow('failed');
    expect(f.ran('cdk')).toBe(false);
  });

  test('--apply with a stack that has no name servers prints an empty list rather than failing', () => {
    const f = fake(withCaller({ 'describe-stacks': '[]' }));
    main(['dns', ...BASE, '--apply'], f.t);
    expect(f.text()).toContain('Name servers of tierstore.dev');
  });
});

describe('site', () => {
  const answers = () => withCaller({
    'get-open-id-connect-provider': '{}',
    'list-hosted-zones-by-name': JSON.stringify({ HostedZones: [{ Name: 'tierstore.dev.' }] }),
    [`api repos/${REPO}`]: REPO_JSON,
    'describe-stacks': STACK,
  });
  const ctx = `-c account=${ACCOUNT} -c githubRepo=${REPO} -c githubRepoId=222 -c githubOwnerId=111 -c oidcProviderArn=arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;

  test('a dry run prints the ids it will trust and runs cdk diff only', () => {
    const f = fake(answers());
    main(['site', ...BASE, '--repo', REPO], f.t);
    expect(f.text()).toContain('repo id 222, owner id 111');
    expect(f.streamed(`cdk diff tierstore-prod-site --profile example-profile ${ctx}`)).toBe(true);
    expect(f.ran('cdk deploy')).toBe(false);
  });

  test('--apply deploys the site stack and names the next step', () => {
    const f = fake(answers());
    main(['site', ...BASE, '--repo', REPO, '--apply'], f.t);
    expect(f.streamed(`cdk deploy tierstore-prod-site --profile example-profile --require-approval broadening ${ctx}`)).toBe(true);
    expect(f.text()).toContain('https://d111.cloudfront.net/');
    expect(f.text()).toContain(`npm run held -- vars --account ${ACCOUNT} --profile example-profile --repo ${REPO}`);
  });

  test('--apply with a stack that reports no domain name still finishes', () => {
    const f = fake({ ...answers(), 'describe-stacks': '[]' });
    main(['site', ...BASE, '--repo', REPO, '--apply'], f.t);
    expect(f.text()).not.toContain('The site is served at');
  });

  test('an explicit OIDC provider ARN is checked and passed on', () => {
    const f = fake(answers());
    main(['site', ...BASE, '--repo', REPO, '--oidc-provider-arn', 'arn:aws:iam::1:oidc-provider/x'], f.t);
    expect(f.ran('--open-id-connect-provider-arn arn:aws:iam::1:oidc-provider/x')).toBe(true);
    expect(f.streamed('-c oidcProviderArn=arn:aws:iam::1:oidc-provider/x')).toBe(true);
  });

  test('refuses when the GitHub OIDC provider is absent, before any build', () => {
    const f = fake({ ...answers(), 'get-open-id-connect-provider': new Error('NoSuchEntity') });
    expect(() => main(['site', ...BASE, '--repo', REPO], f.t)).toThrow('no GitHub OIDC provider at');
    expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
  });

  test('refuses when the hosted zone does not exist yet', () => {
    const f = fake({ ...answers(), 'list-hosted-zones-by-name': JSON.stringify({ HostedZones: [{ Name: 'example.org.' }] }) });
    expect(() => main(['site', ...BASE, '--repo', REPO], f.t)).toThrow('no tierstore.dev hosted zone in this account; run dns --apply first');
  });

  test('refuses a repository whose GitHub name differs from the one given, and builds nothing', () => {
    const f = fake({ ...answers(), [`api repos/${REPO}`]: JSON.stringify({ full_name: 'other-org/example-repo', id: 5, owner: { id: 6 } }) });
    expect(() => main(['site', ...BASE, '--repo', REPO], f.t)).toThrow("resolves to 'other-org/example-repo'");
    expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
  });

  test('uses GitHub\'s own spelling of the repository in the context', () => {
    const f = fake({ ...answers(), [`api repos/${REPO}`]: JSON.stringify({ full_name: 'Example-Org/Example-Repo', id: 222, owner: { id: 111 } }) });
    main(['site', ...BASE, '--repo', REPO], f.t);
    expect(f.streamed('-c githubRepo=Example-Org/Example-Repo')).toBe(true);
  });

  test('a response without a full_name is refused', () => {
    const f = fake({ ...answers(), [`api repos/${REPO}`]: '{}' });
    expect(() => main(['site', ...BASE, '--repo', REPO], f.t)).toThrow("resolves to ''");
  });
});

describe('checkEnvironment', () => {
  test('is empty for an environment that deploys from main only', () => {
    const f = fake(goodGithub());
    expect(checkEnvironment(f.t, REPO, 'release')).toEqual([]);
  });

  test('names a missing environment (HTTP 404, in the message or on stderr)', () => {
    expect(checkEnvironment(fake({ '=environments/release': notFound }).t, REPO, 'release')).toEqual(['environment release does not exist']);
    expect(checkEnvironment(fake({ '=environments/release': new Error('gh: Not Found (HTTP 404)') }).t, REPO, 'release')).toEqual(['environment release does not exist']);
    expect(checkEnvironment(fake({ '=environments/release': 'null' }).t, REPO, 'release')).toEqual(['environment release does not exist']);
  });

  test('refuses to guess when gh fails for another reason', () => {
    expect(() => checkEnvironment(fake({ '=environments/release': new Error('HTTP 403: forbidden\nmore') }).t, REPO, 'release')).toThrow('failed, and not with a 404');
    expect(() => checkEnvironment(fake({ '=environments/release': 'not json' }).t, REPO, 'release')).toThrow('failed, and not with a 404');
    expect(() => checkEnvironment(fake({ '=environments/release': Object.assign(new Error('x'), { stderr: undefined }) }).t, REPO, 'release')).toThrow(Refused);
    expect(() => checkEnvironment(fake({ '=environments/release': Object.assign(new Error('x'), { stderr: 'gh: Not Found (HTTP 404)' }) }).t, REPO, 'release')).not.toThrow();
  });

  test('reports an environment that does not use custom branch policies only', () => {
    const f = fake({ ...goodGithub(), [`=repos/${REPO}/environments/release`]: envJson({ deployment_branch_policy: { protected_branches: true, custom_branch_policies: false } }) });
    expect(checkEnvironment(f.t, REPO, 'release')).toEqual(['environment release does not use custom branch policies only']);
    const g = fake({ ...goodGithub(), [`=repos/${REPO}/environments/release`]: JSON.stringify({ deployment_branch_policy: null }) });
    expect(checkEnvironment(g.t, REPO, 'release')).toEqual(['environment release does not use custom branch policies only']);
  });

  test('reports a policy that is not exactly the branch main', () => {
    const wrong = [polsJson(['v*', 'tag']), polsJson(['main', 'tag']), polsJson(['main', 'branch'], ['v*', 'tag']), polsJson()];
    for (const pols of wrong) {
      const f = fake({ ...goodGithub(), [`repos/${REPO}/environments/release/deployment-branch-policies`]: pols });
      expect(checkEnvironment(f.t, REPO, 'release')).toEqual(['environment release does not deploy from exactly the branch main']);
    }
    const none = fake({ ...goodGithub(), [`repos/${REPO}/environments/release/deployment-branch-policies`]: notFound });
    expect(checkEnvironment(none.t, REPO, 'release')).toEqual(['environment release does not deploy from exactly the branch main']);
    const missing = fake({ ...goodGithub(), [`repos/${REPO}/environments/release/deployment-branch-policies`]: '{}' });
    expect(checkEnvironment(missing.t, REPO, 'release')).toEqual(['environment release does not deploy from exactly the branch main']);
  });
});

describe('github', () => {
  test('passes when all five environments exist and deploy from main, and changes nothing', () => {
    const f = fake(goodGithub());
    main(['github', '--repo', REPO], f.t);
    expect(f.text()).toContain('prod-site-tierstore, prod-releases-tierstore, release, apt-publish, apt-signing');
    expect(f.calls.every((x) => x.kind === 'capture' && x.cmd === 'gh')).toBe(true);
  });

  test('names every environment that is wrong', () => {
    const f = fake({ ...goodGithub(), [`=repos/${REPO}/environments/apt-signing`]: notFound, [`repos/${REPO}/environments/release/deployment-branch-policies`]: polsJson(['v*', 'tag']) });
    expect(() => main(['github', '--repo', REPO], f.t)).toThrow(/environment apt-signing does not exist\n- environment release does not deploy from exactly the branch main|environment release does not deploy[^]*environment apt-signing does not exist/);
  });
});

describe('vars', () => {
  const answers = () => withCaller({ ...goodGithub(), 'describe-stacks': STACK });

  test('prints the six gh variable set commands, each in the environment of the job that assumes the role', () => {
    const f = fake(answers());
    main(['vars', ...BASE, '--repo', REPO], f.t);
    const set = (k: string, env: string, v: string) => `gh variable set ${k} --env ${env} --repo ${REPO} --body '${v}'`;
    expect(f.lines.filter((l) => l.startsWith('gh variable set'))).toEqual([
      set('TIERSTORE_SITE_BUCKET', 'prod-site-tierstore', `tierstore-prod-site-${ACCOUNT}`),
      set('TIERSTORE_SITE_DISTRIBUTION_ID', 'prod-site-tierstore', 'EDIST123'),
      set('TIERSTORE_SITE_PUBLISH_ROLE_ARN', 'prod-site-tierstore', `arn:aws:iam::${ACCOUNT}:role/tierstore-prod-publish`),
      set('TIERSTORE_RELEASES_BUCKET', 'prod-releases-tierstore', `tierstore-prod-site-${ACCOUNT}`),
      set('TIERSTORE_RELEASES_ROLE_ARN', 'prod-releases-tierstore', `arn:aws:iam::${ACCOUNT}:role/tierstore-prod-releases`),
      set('TIERSTORE_RELEASES_DISTRIBUTION_ID', 'prod-releases-tierstore', 'EDIST123'),
    ]);
    expect(f.text()).toContain('gh variable set TIERSTORE_SITE_PUBLISH --body true');
    expect(f.text()).toContain('https://d111.cloudfront.net/');
    expect(f.calls.filter((x) => x.kind === 'stream')).toEqual([]);
  });

  test('prints nothing when an environment is missing or deploys from another branch', () => {
    const f = fake({ ...answers(), [`=repos/${REPO}/environments/prod-releases-tierstore`]: notFound });
    expect(() => main(['vars', ...BASE, '--repo', REPO], f.t)).toThrow('environment prod-releases-tierstore does not exist');
    expect(f.lines.some((l) => l.startsWith('gh variable set'))).toBe(false);
  });

  test('refuses when the stack has no outputs yet', () => {
    const f = fake({ ...answers(), 'describe-stacks': '[]' });
    expect(() => main(['vars', ...BASE, '--repo', REPO], f.t)).toThrow('has no outputs; run site --apply first');
  });

  test('without a distribution domain name it still prints the commands', () => {
    const stack = JSON.stringify(JSON.parse(STACK).filter((o: any) => o.OutputKey !== 'DistributionDomainName'));
    const f = fake({ ...answers(), 'describe-stacks': stack });
    main(['vars', ...BASE, '--repo', REPO], f.t);
    expect(f.text()).not.toContain('The distribution answers');
  });
});

describe('cli', () => {
  test('a refusal prints "error: <message>" and sets exit code 2', () => {
    const f = fake();
    const proc: { exitCode?: string | number | null } = {};
    const err: string[] = [];
    cli(['nope'], f.t, proc, (l) => err.push(l));
    expect(proc.exitCode).toBe(2);
    expect(err[0]).toMatch(/^error: unknown or missing command/);
  });

  test('a command that succeeds leaves the exit code alone', () => {
    const f = fake(goodGithub());
    const proc: { exitCode?: string | number | null } = {};
    cli(['github', '--repo', REPO], f.t, proc, () => undefined);
    expect(proc.exitCode).toBeUndefined();
  });

  test('any other failure propagates', () => {
    const f = fake(withCaller(), 'npm test');
    expect(() => cli(['dns', ...BASE], f.t, {}, () => undefined)).toThrow('failed');
  });

  test('the default error sink is console.error', () => {
    const spy = jest.spyOn(console, 'error').mockImplementation(() => undefined);
    const proc: { exitCode?: string | number | null } = {};
    cli(['nope'], fake().t, proc);
    expect(spy).toHaveBeenCalledTimes(1);
    spy.mockRestore();
  });
});

describe('realTools', () => {
  test('capture runs the command in infra/site and returns its output', () => {
    const exec = jest.fn().mockReturnValue('out');
    const tools = realTools(exec as any, () => undefined);
    expect(tools.capture('gh', ['api', 'x'])).toBe('out');
    expect(exec).toHaveBeenCalledWith('gh', ['api', 'x'], { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    expect(ROOT.endsWith('/infra/site')).toBe(true);
  });

  test('stream shows the output of the command', () => {
    const exec = jest.fn();
    realTools(exec as any, () => undefined).stream('npm', ['test']);
    expect(exec).toHaveBeenCalledWith('npm', ['test'], { cwd: ROOT, stdio: 'inherit' });
  });

  test('log goes to the sink given, console.log by default', () => {
    const lines: string[] = [];
    realTools(jest.fn() as any, (l) => lines.push(l)).log('x');
    expect(lines).toEqual(['x']);
    const spy = jest.spyOn(console, 'log').mockImplementation(() => undefined);
    realTools().log('y');
    expect(spy).toHaveBeenCalledWith('y');
    spy.mockRestore();
  });

  test('the default exec is child_process.execFileSync', () => {
    const real = require('child_process').execFileSync;
    jest.isolateModules(() => {
      const { realTools: fresh } = require('../held/tools');
      expect(fresh().capture(process.execPath, ['-e', 'process.stdout.write("hi")'])).toBe('hi');
    });
    expect(typeof real).toBe('function');
  });
});

describe('held/main.ts', () => {
  test('forwards the command line, the real tools and the process to cli', () => {
    jest.isolateModules(() => {
      const run = jest.fn();
      const tools = { marker: 'tools' };
      jest.doMock('../held/cli', () => ({ cli: run }));
      jest.doMock('../held/tools', () => ({ realTools: () => tools }));
      require('../held/main');
      expect(run).toHaveBeenCalledWith(process.argv.slice(2), tools, process);
    });
  });
});
