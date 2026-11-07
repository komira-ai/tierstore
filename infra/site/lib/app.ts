import { App } from 'aws-cdk-lib';
import { REGION, siteConfig, zoneConfig } from './config';
import { ZoneStack } from './dns-stack';
import { SiteStack } from './site-stack';

/**
 * Adds the stacks to `app`. Everything specific to an installation is context: -c account=<12 digits>, and for the
 * site stack -c githubRepo=<owner>/<name> -c githubRepoId=<id> -c githubOwnerId=<id> (and optionally
 * -c oidcProviderArn=<arn>). The held CLI (held/steps.ts) supplies them; stacks are selected by name on the cdk command line.
 */
export function build(app: App): void {
  const get = (k: string) => app.node.tryGetContext(k);
  const zone = zoneConfig(get);
  const env = { account: zone.account, region: REGION };

  new ZoneStack(app, { env });
  if (get('githubRepoId') !== undefined) {
    new SiteStack(app, {
      env,
      config: siteConfig(get),
      oidcProviderArn:
        (get('oidcProviderArn') as string | undefined) ??
        `arn:aws:iam::${zone.account}:oidc-provider/token.actions.githubusercontent.com`,
    });
  }
}
