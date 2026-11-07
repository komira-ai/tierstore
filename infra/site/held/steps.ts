// The held steps as a small CLI. Every step is a dry run unless --apply is given; there is no --force; every AWS step
// refuses unless the profile's account equals --account. All tool access goes through `Tools`, so the tests drive the
// steps with a fake and can see exactly which commands ran.

import * as c from '../lib/config';

export interface Tools {
  /** Run a read-only command and return its stdout. Throws on a non-zero exit. */
  capture(cmd: string, args: string[]): string;
  /** Run a command with its output shown (diff, install, tests, deploy). */
  stream(cmd: string, args: string[]): void;
  log(line: string): void;
}

export class Refused extends Error {}

export type Command = 'oidc-provider' | 'dns' | 'site' | 'vars' | 'github';

export interface Options {
  command: Command;
  apply: boolean;
  account?: string;
  profile?: string;
  repo?: string;
  oidcProviderArn?: string;
}

const ALLOWED: Record<Command, string[]> = {
  'oidc-provider': ['--account', '--profile'],
  dns: ['--account', '--profile'],
  site: ['--account', '--profile', '--repo', '--oidc-provider-arn'],
  vars: ['--account', '--profile', '--repo'],
  github: ['--repo'],
};
const REQUIRED: Record<Command, string[]> = {
  'oidc-provider': ['--account', '--profile'],
  dns: ['--account', '--profile'],
  site: ['--account', '--profile', '--repo'],
  vars: ['--account', '--profile', '--repo'],
  github: ['--repo'],
};
/** The commands that only read: they have no --apply. */
const READ_ONLY: Command[] = ['vars', 'github'];

export const USAGE = `usage: npm run held -- <command> [flags]
  oidc-provider --account <12 digits> --profile <aws profile> [--apply]   (creates the GitHub OIDC provider only if absent)
  dns    --account <12 digits> --profile <aws profile> [--apply]   (the ${c.DOMAIN} hosted zone)
  site   --account <12 digits> --profile <aws profile> --repo <owner/name> [--oidc-provider-arn <arn>] [--apply]
  vars   --account <12 digits> --profile <aws profile> --repo <owner/name>   (read-only; prints the gh commands)
  github --repo <owner/name>   (read-only; checks that the GitHub environments exist and deploy from main only)
Dry run unless --apply. There is no --force.`;

export function parse(argv: string[]): Options {
  const [command, ...rest] = argv;
  if (!command || !(command in ALLOWED)) throw new Refused(`unknown or missing command\n${USAGE}`);
  const cmd = command as Command;
  const o: Options = { command: cmd, apply: false };
  const seen = new Set<string>();
  for (let i = 0; i < rest.length; i++) {
    const f = rest[i];
    if (f === '--apply') {
      if (READ_ONLY.includes(cmd)) throw new Refused(`${cmd} has no --apply: it only reads`);
      o.apply = true;
      continue;
    }
    if (!ALLOWED[cmd].includes(f)) throw new Refused(`unknown flag for ${cmd}: ${f}\n${USAGE}`);
    const v = rest[++i];
    if (v === undefined || v.startsWith('--')) throw new Refused(`${f} needs a value`);
    seen.add(f);
    if (f === '--account') o.account = v;
    if (f === '--profile') o.profile = v;
    if (f === '--repo') o.repo = v;
    if (f === '--oidc-provider-arn') o.oidcProviderArn = v;
  }
  for (const r of REQUIRED[cmd]) if (!seen.has(r)) throw new Refused(`${r} is required`);
  if (o.account !== undefined && !/^\d{12}$/.test(o.account)) throw new Refused('--account must be 12 digits');
  if (o.repo !== undefined && !/^[\w.-]+\/[\w.-]+$/.test(o.repo)) throw new Refused('--repo must be <owner>/<name>');
  return o;
}

const aws = (o: Options, ...a: string[]) => [...a, '--profile', o.profile!];

/** Read-only: refuse unless the credentials belong to the account the operator named. */
export function checkCaller(t: Tools, o: Options): void {
  const got = t.capture('aws', aws(o, 'sts', 'get-caller-identity', '--query', 'Account', '--output', 'text')).trim();
  if (got !== o.account) throw new Refused(`profile '${o.profile}' is account ${got}, not the --account you passed`);
}

/** Local only: install the pinned dependencies and run the tests before any cdk call. */
function prepare(t: Tools): void {
  t.stream('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund']);
  t.stream('npm', ['test']);
}

/** Quote a value for a POSIX shell word: single quotes, each embedded quote written as '\''. */
export const shellQuote = (v: string) => `'${v.replace(/'/g, "'\\''")}'`;

/**
 * Read-only `gh api repos/<repo>`: the immutable numeric ids the trust needs. Refuses unless GitHub's `full_name`
 * equals the --repo given (case-insensitive), so a typo that names a real repository of another owner cannot yield a
 * role that trusts it, and prints all three values so the operator sees what the roles will trust.
 */
export function repoIds(t: Tools, repo: string): { repoId: string; ownerId: string; fullName: string } {
  const j = JSON.parse(t.capture('gh', ['api', `repos/${repo}`]));
  const fullName = String(j.full_name ?? '');
  if (fullName.toLowerCase() !== repo.toLowerCase()) {
    throw new Refused(`--repo ${repo} resolves to '${fullName}' on GitHub; pass the exact owner/name (nothing was changed)`);
  }
  const ids = { repoId: String(j.id), ownerId: String(j.owner.id), fullName };
  t.log(`# repository ${fullName}: repo id ${ids.repoId}, owner id ${ids.ownerId} (the roles will trust exactly this)`);
  return ids;
}

const stackOutputs = (t: Tools, o: Options, stack: string): Map<string, string> => {
  const outs = JSON.parse(
    t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', c.REGION, '--stack-name', stack, '--query', 'Stacks[0].Outputs', '--output', 'json')),
  ) as { OutputKey: string; OutputValue: string }[];
  return new Map(outs.map((x) => [x.OutputKey, x.OutputValue]));
};

export const OIDC_URL = `https://${c.GITHUB_OIDC_HOST}`;

/** Creates the account's GitHub OIDC provider only when absent (an account holds one per URL); never touches an existing one. */
export function oidcProvider(t: Tools, o: Options): void {
  checkCaller(t, o);
  const list = JSON.parse(t.capture('aws', aws(o, 'iam', 'list-open-id-connect-providers', '--output', 'json'))).OpenIDConnectProviderList as
    | { Arn: string }[]
    | undefined;
  const have = (list ?? []).find((p) => p.Arn.endsWith(`:oidc-provider/${c.GITHUB_OIDC_HOST}`));
  if (have) {
    t.log(`# GitHub OIDC provider already present: ${have.Arn} (nothing to do)`);
    return;
  }
  const create = aws(o, 'iam', 'create-open-id-connect-provider', '--url', OIDC_URL, '--client-id-list', c.OIDC_AUDIENCE);
  if (o.apply) {
    t.log(`+ aws ${create.join(' ')}`);
    t.stream('aws', create);
  } else {
    t.log('# dry run: nothing is executed. Re-run with --apply to act.');
    t.log(`DRY-RUN: aws ${create.join(' ')}`);
  }
}

export function dns(t: Tools, o: Options): void {
  checkCaller(t, o);
  prepare(t);
  const ctx = ['-c', `account=${o.account}`];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', c.ZONE_STACK_NAME, '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const ns = stackOutputs(t, o, c.ZONE_STACK_NAME).get('NameServers') ?? '';
    t.log(`# Name servers of ${c.DOMAIN} (copy these four, one per line, no trailing dots, to your registrar):`);
    for (const n of ns.split(',')) t.log(n);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to create the zone.');
    t.stream('npx', ['cdk', 'diff', c.ZONE_STACK_NAME, '--profile', o.profile!, ...ctx]);
  }
}

export function site(t: Tools, o: Options): void {
  checkCaller(t, o);
  const arn = o.oidcProviderArn ?? `arn:aws:iam::${o.account}:oidc-provider/${c.GITHUB_OIDC_HOST}`;
  try {
    t.capture('aws', aws(o, 'iam', 'get-open-id-connect-provider', '--open-id-connect-provider-arn', arn));
  } catch {
    throw new Refused(`no GitHub OIDC provider at ${arn}; run: npm run held -- oidc-provider (with this account and profile, then --apply) first (it is imported, never created here) or pass --oidc-provider-arn`);
  }
  const zones = JSON.parse(
    t.capture('aws', aws(o, 'route53', 'list-hosted-zones-by-name', '--dns-name', c.DOMAIN, '--max-items', '1', '--output', 'json')),
  ).HostedZones as { Name: string }[];
  if (!zones.some((z) => z.Name === `${c.DOMAIN}.`)) throw new Refused(`no ${c.DOMAIN} hosted zone in this account; run dns --apply first`);
  const { repoId, ownerId, fullName } = repoIds(t, o.repo!);
  prepare(t);
  const ctx = [
    '-c', `account=${o.account}`, '-c', `githubRepo=${fullName}`, // GitHub's own spelling: the sub claim uses it
    '-c', `githubRepoId=${repoId}`, '-c', `githubOwnerId=${ownerId}`, '-c', `oidcProviderArn=${arn}`,
  ];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', c.SITE_STACK_NAME, '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const host = stackOutputs(t, o, c.SITE_STACK_NAME).get('DistributionDomainName');
    if (host) t.log(`# The site is served at https://${host}/ (and at https://${c.DOMAIN}/ once DNS answers)`);
    t.log(`# Done. Next: npm run held -- vars --account ${o.account} --profile ${o.profile} --repo ${o.repo}`);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to deploy.');
    t.stream('npx', ['cdk', 'diff', c.SITE_STACK_NAME, '--profile', o.profile!, ...ctx]);
  }
}

/** Read-only GET of a GitHub API path under the repo; undefined for HTTP 404 (an environment that does not exist). Any other failure aborts. */
function ghGet(t: Tools, repo: string, path: string): any {
  try {
    return JSON.parse(t.capture('gh', ['api', `repos/${repo}/${path}`]));
  } catch (e) {
    const err = e as Error & { stderr?: unknown }; // execFileSync puts gh's stderr on the error; its message holds it too
    if (/\(HTTP 404\)/.test(`${err.message}\n${String(err.stderr)}`)) return undefined;
    throw new Refused(`gh api repos/${repo}/${path} failed, and not with a 404 (permission or network?): ${err.message.split('\n')[0]} (nothing was changed)`);
  }
}

/** Why an environment is not as expected: it must exist and deploy from the branch main only. Empty when it is. */
export function checkEnvironment(t: Tools, repo: string, name: string): string[] {
  const env = ghGet(t, repo, `environments/${name}`);
  if (!env) return [`environment ${name} does not exist`];
  const bad: string[] = [];
  const dbp = env.deployment_branch_policy;
  if (dbp?.protected_branches !== false || dbp?.custom_branch_policies !== true) {
    bad.push(`environment ${name} does not use custom branch policies only`);
  }
  const pols: { name: string; type: string }[] = ghGet(t, repo, `environments/${name}/deployment-branch-policies`)?.branch_policies ?? [];
  if (pols.length !== 1 || pols[0].name !== 'main' || pols[0].type !== 'branch') {
    bad.push(`environment ${name} does not deploy from exactly the branch main`);
  }
  return bad;
}

/** Read-only: every environment the workflows use exists and deploys from main only. Nothing is created here. */
export function github(t: Tools, o: Options): void {
  const names = [c.SITE_TRUST.environment, c.RELEASES_TRUST.environment, ...c.OTHER_ENVIRONMENTS];
  const bad = names.flatMap((n) => checkEnvironment(t, o.repo!, n));
  if (bad.length > 0) throw new Refused(`GitHub environments of ${o.repo} are not as expected (create or fix them by hand; nothing was changed):\n- ${bad.join('\n- ')}`);
  t.log(`# ${names.join(', ')} of ${o.repo} exist and deploy from main only.`);
}

export function vars(t: Tools, o: Options): void {
  checkCaller(t, o);
  const envs = [c.SITE_TRUST.environment, c.RELEASES_TRUST.environment];
  const bad = envs.flatMap((n) => checkEnvironment(t, o.repo!, n));
  if (bad.length > 0) throw new Refused(`GitHub environments differ from what the roles trust:\n- ${bad.join('\n- ')}\nfix them before setting variables`);
  const out = stackOutputs(t, o, c.SITE_STACK_NAME);
  const [sBucket, sDist, sRole] = c.SITE_VARIABLES;
  const [rBucket, rRole, rDist] = c.RELEASES_VARIABLES;
  const rows: [string, string, string | undefined][] = [
    [c.SITE_TRUST.environment, sBucket, out.get('BucketName')],
    [c.SITE_TRUST.environment, sDist, out.get('DistributionId')],
    [c.SITE_TRUST.environment, sRole, out.get('PublishRoleArn')],
    [c.RELEASES_TRUST.environment, rBucket, out.get('BucketName')],
    [c.RELEASES_TRUST.environment, rRole, out.get('ReleasesRoleArn')],
    [c.RELEASES_TRUST.environment, rDist, out.get('DistributionId')],
  ];
  if (rows.some(([, , v]) => !v)) throw new Refused(`stack ${c.SITE_STACK_NAME} has no outputs; run site --apply first`);
  t.log(`# Environments ${envs.join(' and ')} of ${o.repo} are as expected. Each variable belongs to the environment of the job that assumes the role. Run:`);
  for (const [env, k, v] of rows) t.log(`gh variable set ${k} --env ${env} --repo ${o.repo} --body ${shellQuote(v!)}`);
  t.log(`# Region is always ${c.REGION}. Publishing stays off until: gh variable set TIERSTORE_SITE_PUBLISH --body true --repo ${o.repo}`);
  const host = out.get('DistributionDomainName');
  if (host) t.log(`# The distribution answers at https://${host}/`);
}

export function main(argv: string[], t: Tools): void {
  const o = parse(argv);
  ({ 'oidc-provider': oidcProvider, dns, site, vars, github })[o.command](t, o);
}
