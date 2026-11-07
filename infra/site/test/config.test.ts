import * as c from '../lib/config';

const ctx = (o: Record<string, unknown>) => (k: string) => o[k];
const GOOD = { account: '123456789012', githubRepo: 'example-org/example-repo', githubRepoId: '222', githubOwnerId: '111' };

describe('names', () => {
  test('are the ones tierstore.dev is deployed under', () => {
    expect(c.ZONE_STACK_NAME).toBe('tierstore-dns-zone');
    expect(c.SITE_STACK_NAME).toBe('tierstore-prod-site');
    expect(c.bucketName('123456789012')).toBe('tierstore-prod-site-123456789012');
    expect(c.HEADERS_POLICY_NAME).toBe('tierstore-prod-site-headers');
    expect(c.ROUTER_NAME).toBe('tierstore-prod-site-router');
    expect(c.SITE_ROLE_NAME).toBe('tierstore-prod-publish');
    expect(c.RELEASES_ROLE_NAME).toBe('tierstore-prod-releases');
    expect(c.DOMAIN).toBe('tierstore.dev');
    expect(c.REGION).toBe('us-east-1');
  });

  test('the roles trust the environments and workflow files the workflows use', () => {
    expect(c.SITE_TRUST).toEqual({ environment: 'prod-site-tierstore', workflow: '.github/workflows/tierstore-site-publish.yml' });
    expect(c.RELEASES_TRUST).toEqual({ environment: 'prod-releases-tierstore', workflow: '.github/workflows/release.yml' });
    expect(c.OTHER_ENVIRONMENTS).toEqual(['release', 'apt-publish', 'apt-signing']);
  });

  test('the variables are the ones the workflows read', () => {
    expect(c.SITE_VARIABLES).toEqual(['TIERSTORE_SITE_BUCKET', 'TIERSTORE_SITE_DISTRIBUTION_ID', 'TIERSTORE_SITE_PUBLISH_ROLE_ARN']);
    expect(c.RELEASES_VARIABLES).toEqual(['TIERSTORE_RELEASES_BUCKET', 'TIERSTORE_RELEASES_ROLE_ARN', 'TIERSTORE_RELEASES_DISTRIBUTION_ID']);
  });
});

describe('zoneConfig / siteConfig', () => {
  test('take the account and the repository from context', () => {
    expect(c.zoneConfig(ctx(GOOD))).toEqual({ account: '123456789012' });
    expect(c.siteConfig(ctx(GOOD))).toEqual({ account: '123456789012', repo: 'example-org/example-repo', repoId: '222', ownerId: '111' });
  });

  test('refuse a missing or malformed account', () => {
    expect(() => c.zoneConfig(ctx({}))).toThrow('pass -c account=<12 digits>');
    expect(() => c.zoneConfig(ctx({ account: '12345' }))).toThrow('pass -c account=<12 digits>');
  });

  test('refuse a malformed or missing repository and ids', () => {
    expect(() => c.siteConfig(ctx({ ...GOOD, githubRepo: 'nope' }))).toThrow('pass -c githubRepo=<owner>/<name>');
    expect(() => c.siteConfig(ctx({ ...GOOD, githubRepo: undefined }))).toThrow('pass -c githubRepo=');
    expect(() => c.siteConfig(ctx({ ...GOOD, githubRepoId: '0' }))).toThrow('numeric id');
    expect(() => c.siteConfig(ctx({ ...GOOD, githubOwnerId: 'x' }))).toThrow('numeric id');
    expect(() => c.siteConfig(ctx({ ...GOOD, githubRepoId: undefined }))).toThrow('numeric id');
  });
});

describe('trust', () => {
  const config = c.siteConfig(ctx(GOOD));
  const prefix = 'repo:example-org@111/example-repo@222';

  test('the immutable subject prefix names owner, owner id, repository and repository id', () => {
    expect(c.subPrefix(config)).toBe(prefix);
  });

  test('the workflow ref is the repository, the workflow file and the branch main', () => {
    expect(c.workflowRef('example-org/example-repo', c.SITE_TRUST)).toBe('example-org/example-repo/.github/workflows/tierstore-site-publish.yml@refs/heads/main');
  });

  test('the site role trusts its environment and workflow through condition keys, and the sub in both exact forms', () => {
    expect(c.trustConditions(config, c.SITE_TRUST)).toEqual({
      StringEquals: {
        'token.actions.githubusercontent.com:aud': 'sts.amazonaws.com',
        'token.actions.githubusercontent.com:sub': [
          `${prefix}:environment:prod-site-tierstore`,
          `${prefix}:environment:prod-site-tierstore:job_workflow_ref:example-org/example-repo/.github/workflows/tierstore-site-publish.yml@refs/heads/main`,
        ],
        'token.actions.githubusercontent.com:job_workflow_ref': 'example-org/example-repo/.github/workflows/tierstore-site-publish.yml@refs/heads/main',
        'token.actions.githubusercontent.com:environment': 'prod-site-tierstore',
        'token.actions.githubusercontent.com:repository_owner_id': '111',
        'token.actions.githubusercontent.com:repository_id': '222',
      },
    });
  });

  test('the releases role trusts the release environment and workflow', () => {
    const eq = c.trustConditions(config, c.RELEASES_TRUST).StringEquals;
    expect(eq['token.actions.githubusercontent.com:environment']).toBe('prod-releases-tierstore');
    expect(eq['token.actions.githubusercontent.com:job_workflow_ref']).toBe('example-org/example-repo/.github/workflows/release.yml@refs/heads/main');
  });
});
