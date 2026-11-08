import { App } from 'aws-cdk-lib';
import { build } from '../lib/app';

const ACCOUNT = '123456789012';
const appWith = (context: Record<string, unknown>) => new App({ context });
const names = (app: App) => app.node.children.map((n) => n.node.id).sort();

describe('build', () => {
  test('without a repository only the zone stack exists', () => {
    const app = appWith({ account: ACCOUNT });
    build(app);
    expect(names(app)).toEqual(['tierstore-dns-zone']);
  });

  test('with the repository ids both stacks exist, and the OIDC provider defaults to the account GitHub provider', () => {
    const app = appWith({ account: ACCOUNT, githubRepo: 'example-org/example-repo', githubRepoId: '222', githubOwnerId: '111' });
    // The zone lookup needs credentials and a cache: a context entry stands in for it.
    app.node.setContext(
      `hosted-zone:account=${ACCOUNT}:domainName=tierstore.dev:region=us-east-1`,
      { Id: '/hostedzone/Z0TESTZONE', Name: 'tierstore.dev.' },
    );
    build(app);
    expect(names(app)).toEqual(['tierstore-dns-zone', 'tierstore-prod-site']);
    const json = JSON.stringify(app.synth().getStackByName('tierstore-prod-site').template);
    expect(json).toContain(`arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`);
  });

  test('an explicit OIDC provider ARN is used', () => {
    const arn = `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;
    const app = appWith({ account: ACCOUNT, githubRepo: 'example-org/example-repo', githubRepoId: '222', githubOwnerId: '111', oidcProviderArn: arn });
    app.node.setContext(
      `hosted-zone:account=${ACCOUNT}:domainName=tierstore.dev:region=us-east-1`,
      { Id: '/hostedzone/Z0TESTZONE', Name: 'tierstore.dev.' },
    );
    build(app);
    expect(JSON.stringify(app.synth().getStackByName('tierstore-prod-site').template)).toContain(arn);
  });

  test('a missing account is refused', () => {
    expect(() => build(appWith({}))).toThrow('pass -c account=<12 digits>');
  });
});

describe('bin/site.ts', () => {
  test('builds the app it creates', () => {
    jest.isolateModules(() => {
      const built = jest.fn();
      jest.doMock('../lib/app', () => ({ build: built }));
      require('../bin/site');
      expect(built).toHaveBeenCalledTimes(1);
      expect(built.mock.calls[0][0]).toBeInstanceOf(require('aws-cdk-lib').App);
    });
  });
});
