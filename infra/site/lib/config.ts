// Nothing here is a secret, an account id or the repository's numeric ids: the account, the repository and its
// numeric ids are CDK context (-c account=... -c githubRepo=... -c githubRepoId=... -c githubOwnerId=...), supplied by
// the held CLI (held/steps.ts) from read-only `gh api` / `aws sts` calls, and never committed.

export const REGION = 'us-east-1'; // ACM certificates for CloudFront must live here, so every stack does
export const DOMAIN = 'tierstore.dev';
export const SITE_ID = 'tierstore';

export const GITHUB_OIDC_HOST = 'token.actions.githubusercontent.com';
export const OIDC_AUDIENCE = 'sts.amazonaws.com';
export const WORKFLOW_REF = 'refs/heads/main';
export const RELEASES_PREFIX = 'releases/';

/**
 * A GitHub environment a role trusts, and the workflow file (under .github/workflows/) that runs in it. Both are part
 * of the contract with the workflow: renaming either there is a change to the role's trust here, in the same pull request.
 */
export interface Trust {
  environment: string;
  workflow: string;
}
/** The site publish job: build, test, upload the pages. */
export const SITE_TRUST: Trust = { environment: 'prod-site-tierstore', workflow: '.github/workflows/tierstore-site-publish.yml' };
/** The release job: uploads the release files. It can write releases/ and nothing else. */
export const RELEASES_TRUST: Trust = { environment: 'prod-releases-tierstore', workflow: '.github/workflows/release.yml' };
/** Every environment the held CLI checks (it creates none): the two above plus the ones the release workflow uses. */
export const OTHER_ENVIRONMENTS = ['release', 'apt-publish', 'apt-signing'];

/**
 * Every name the infrastructure gets. CloudFormation replaces a resource whose logical id or physical name changes, so
 * these never change once deployed.
 */
export const ZONE_STACK_NAME = `${SITE_ID}-dns-zone`;
export const SITE_STACK_NAME = `${SITE_ID}-prod-site`;
export const bucketName = (account: string): string => `${SITE_ID}-prod-site-${account}`;
export const HEADERS_POLICY_NAME = `${SITE_ID}-prod-site-headers`;
export const ROUTER_NAME = `${SITE_ID}-prod-site-router`;
export const SITE_ROLE_NAME = `${SITE_ID}-prod-publish`;
export const RELEASES_ROLE_NAME = `${SITE_ID}-prod-releases`;

/** The GitHub variables `vars` prints, by environment. */
export const SITE_VARIABLES = ['TIERSTORE_SITE_BUCKET', 'TIERSTORE_SITE_DISTRIBUTION_ID', 'TIERSTORE_SITE_PUBLISH_ROLE_ARN'] as const;
export const RELEASES_VARIABLES = ['TIERSTORE_RELEASES_BUCKET', 'TIERSTORE_RELEASES_ROLE_ARN', 'TIERSTORE_RELEASES_DISTRIBUTION_ID'] as const;

export interface ZoneConfig {
  account: string;
}

export interface SiteConfig extends ZoneConfig {
  /** `owner/name` of the repository whose workflows may assume the roles. */
  repo: string;
  /** Immutable numeric ids (`gh api repos/<repo> --jq '.id,.owner.id'`). */
  repoId: string;
  ownerId: string;
}

const ACCOUNT = /^\d{12}$/;
const REPO = /^[\w.-]+\/[\w.-]+$/;
const NUMERIC_ID = /^[1-9]\d*$/;

export function zoneConfig(get: (key: string) => unknown): ZoneConfig {
  const account = String(get('account'));
  if (!ACCOUNT.test(account)) throw new Error('pass -c account=<12 digits>');
  return { account };
}

export function siteConfig(get: (key: string) => unknown): SiteConfig {
  const base = zoneConfig(get);
  const repo = String(get('githubRepo'));
  const repoId = String(get('githubRepoId'));
  const ownerId = String(get('githubOwnerId'));
  if (!REPO.test(repo)) throw new Error('pass -c githubRepo=<owner>/<name>');
  if (!NUMERIC_ID.test(repoId) || !NUMERIC_ID.test(ownerId)) {
    throw new Error('pass -c githubRepoId=<numeric id> -c githubOwnerId=<numeric id> (gh api repos/<repo> --jq ".id,.owner.id")');
  }
  return { ...base, repo, repoId, ownerId };
}

/** `job_workflow_ref` of a trust: <repo>/<workflow file>@refs/heads/main. */
export const workflowRef = (repo: string, t: Trust): string => `${repo}/${t.workflow}@${WORKFLOW_REF}`;

/** The immutable-subject prefix of the repository: repo:<owner>@<ownerId>/<name>@<repoId>. */
export function subPrefix(c: Pick<SiteConfig, 'repo' | 'repoId' | 'ownerId'>): string {
  const [owner, name] = c.repo.split('/');
  return `repo:${owner}@${c.ownerId}/${name}@${c.repoId}`;
}

/**
 * The trust conditions of a role. The four claims AWS supports as condition keys (environment, job_workflow_ref,
 * repository_id, repository_owner_id) hold whatever the repository's `sub` template is, so this works with the default
 * immutable template and asks for no change to it. `sub` is listed as well, in the two exact forms the default
 * immutable template can produce for an environment job (without and with the workflow), as a further, narrower check.
 */
export function trustConditions(c: SiteConfig, t: Trust): Record<string, Record<string, string | string[]>> {
  const prefix = subPrefix(c);
  const workflow = workflowRef(c.repo, t);
  return {
    StringEquals: {
      [`${GITHUB_OIDC_HOST}:aud`]: OIDC_AUDIENCE,
      [`${GITHUB_OIDC_HOST}:sub`]: [`${prefix}:environment:${t.environment}`, `${prefix}:environment:${t.environment}:job_workflow_ref:${workflow}`],
      [`${GITHUB_OIDC_HOST}:job_workflow_ref`]: workflow,
      [`${GITHUB_OIDC_HOST}:environment`]: t.environment,
      [`${GITHUB_OIDC_HOST}:repository_owner_id`]: c.ownerId,
      [`${GITHUB_OIDC_HOST}:repository_id`]: c.repoId,
    },
  };
}
