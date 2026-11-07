import * as fs from 'fs';
import * as path from 'path';
import * as vm from 'vm';
import * as ts from 'typescript';
import { App } from 'aws-cdk-lib';
import { Match, Template } from 'aws-cdk-lib/assertions';
import * as c from '../lib/config';
import { ZoneStack } from '../lib/dns-stack';
import { CONTENT_SECURITY_POLICY, NONCURRENT_VERSION_DAYS, ROUTER_SOURCE, SiteStack } from '../lib/site-stack';

const ACCOUNT = '123456789012'; // the AWS documentation placeholder, not a real account
// Fake repository and ids: the stack takes them as context, none is built in.
const CONFIG: c.SiteConfig = { account: ACCOUNT, repo: 'example-org/example-repo', repoId: '222', ownerId: '111' };
const PROVIDER = `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;
const env = { account: ACCOUNT, region: 'us-east-1' };

const stack = new SiteStack(new App(), { env, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' });
const t = Template.fromStack(stack);

/** The statements of the inline policy of the role named `roleName`, by sid. */
function statements(roleName: string): Record<string, any> {
  const roles = t.findResources('AWS::IAM::Role', { Properties: { RoleName: roleName } });
  const [logicalId] = Object.keys(roles);
  const out: Record<string, any> = {};
  for (const p of Object.values(t.findResources('AWS::IAM::Policy')) as any[]) {
    if (!p.Properties.Roles.some((r: any) => r.Ref === logicalId)) continue;
    for (const s of p.Properties.PolicyDocument.Statement) out[s.Sid] = s;
  }
  return out;
}
const resourceText = (s: any) => JSON.stringify(s.Resource);

describe('names', () => {
  test('the stacks are named tierstore-dns-zone and tierstore-prod-site', () => {
    expect(new ZoneStack(new App(), { env }).stackName).toBe('tierstore-dns-zone');
    expect(stack.stackName).toBe('tierstore-prod-site');
  });

  test('the bucket, headers policy, router and roles carry the tierstore names', () => {
    t.hasResourceProperties('AWS::S3::Bucket', { BucketName: `tierstore-prod-site-${ACCOUNT}` });
    t.hasResourceProperties('AWS::CloudFront::ResponseHeadersPolicy', { ResponseHeadersPolicyConfig: Match.objectLike({ Name: 'tierstore-prod-site-headers' }) });
    t.hasResourceProperties('AWS::CloudFront::Function', { Name: 'tierstore-prod-site-router' });
    t.hasResourceProperties('AWS::IAM::Role', { RoleName: 'tierstore-prod-publish' });
    t.hasResourceProperties('AWS::IAM::Role', { RoleName: 'tierstore-prod-releases' });
  });
});

describe('deployment guards', () => {
  test('a region other than us-east-1 is refused', () => {
    expect(() => new SiteStack(new App(), { env: { account: ACCOUNT, region: 'eu-west-1' }, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' })).toThrow('must be deployed to us-east-1');
  });

  test('an OIDC provider ARN that is not the GitHub provider is refused', () => {
    expect(() => new SiteStack(new App(), { env, config: CONFIG, oidcProviderArn: `arn:aws:iam::${ACCOUNT}:oidc-provider/example.com`, hostedZoneId: 'Z0TESTZONE' })).toThrow('oidcProviderArn must be');
  });

  test('an unresolved region (no env) passes the region guard', () => {
    expect(() => new SiteStack(new App(), { config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' })).not.toThrow();
  });

  test('without a hosted zone id the zone is looked up by name', () => {
    const app = new App();
    const looked = new SiteStack(app, { env, config: CONFIG, oidcProviderArn: PROVIDER });
    expect(Template.fromStack(looked).findResources('AWS::Route53::RecordSet')).not.toEqual({});
  });
});

describe('bucket', () => {
  test('is private, encrypted, versioned, owner-enforced and retained', () => {
    t.hasResourceProperties('AWS::S3::Bucket', {
      PublicAccessBlockConfiguration: { BlockPublicAcls: true, BlockPublicPolicy: true, IgnorePublicAcls: true, RestrictPublicBuckets: true },
      BucketEncryption: { ServerSideEncryptionConfiguration: [{ ServerSideEncryptionByDefault: { SSEAlgorithm: 'AES256' } }] },
      VersioningConfiguration: { Status: 'Enabled' },
      OwnershipControls: { Rules: [{ ObjectOwnership: 'BucketOwnerEnforced' }] },
    });
    t.hasResource('AWS::S3::Bucket', { DeletionPolicy: 'Retain', UpdateReplacePolicy: 'Retain' });
  });

  test('only noncurrent versions expire; no rule expires current objects (releases are kept forever)', () => {
    const b = Object.values(t.findResources('AWS::S3::Bucket'))[0] as any;
    for (const r of b.Properties.LifecycleConfiguration.Rules) {
      expect(r.ExpirationInDays).toBeUndefined();
      expect(r.ExpirationDate).toBeUndefined();
      expect(r.NoncurrentVersionExpiration).toEqual({ NoncurrentDays: NONCURRENT_VERSION_DAYS });
      expect(r.AbortIncompleteMultipartUpload).toEqual({ DaysAfterInitiation: 7 });
    }
    expect(NONCURRENT_VERSION_DAYS).toBe(365);
  });

  test('the bucket policy denies non-TLS requests and grants CloudFront GetObject from this distribution only', () => {
    const pol = Object.values(t.findResources('AWS::S3::BucketPolicy'))[0] as any;
    const all: any[] = pol.Properties.PolicyDocument.Statement;
    const deny = all.filter((s) => s.Effect === 'Deny');
    expect(deny).toHaveLength(1);
    expect(deny[0].Condition).toEqual({ Bool: { 'aws:SecureTransport': 'false' } });
    const allow = all.filter((s) => s.Effect === 'Allow');
    expect(allow).toHaveLength(1);
    expect(allow[0].Principal).toEqual({ Service: 'cloudfront.amazonaws.com' });
    expect(allow[0].Action).toBe('s3:GetObject');
    expect(JSON.stringify(allow[0].Condition.StringEquals['AWS:SourceArn'])).toContain(':distribution/');
  });
});

describe('cloudfront', () => {
  test('uses origin access control, not an identity', () => {
    t.resourceCountIs('AWS::CloudFront::OriginAccessControl', 1);
    t.resourceCountIs('AWS::CloudFront::CloudFrontOriginAccessIdentity', 0);
  });

  test('the distribution serves tierstore.dev with TLS 1.2, HTTP/3, https redirect, GET/HEAD, and 403/404 as the 404 page', () => {
    t.hasResourceProperties('AWS::CloudFront::Distribution', {
      DistributionConfig: Match.objectLike({
        Aliases: ['tierstore.dev'],
        DefaultRootObject: 'index.html',
        HttpVersion: 'http2and3',
        ViewerCertificate: Match.objectLike({ MinimumProtocolVersion: 'TLSv1.2_2021', SslSupportMethod: 'sni-only' }),
        DefaultCacheBehavior: Match.objectLike({
          ViewerProtocolPolicy: 'redirect-to-https',
          AllowedMethods: ['GET', 'HEAD'],
          CachePolicyId: '658327ea-f89d-4fab-a63d-7e88639e58f6', // CACHING_OPTIMIZED
          FunctionAssociations: [Match.objectLike({ EventType: 'viewer-request' })],
        }),
        CustomErrorResponses: [
          Match.objectLike({ ErrorCode: 403, ResponseCode: 404, ResponsePagePath: '/404.html' }),
          Match.objectLike({ ErrorCode: 404, ResponseCode: 404, ResponsePagePath: '/404.html' }),
        ],
      }),
    });
  });

  test('the records are A and AAAA aliases at the apex, and the certificate is validated by DNS', () => {
    t.hasResourceProperties('AWS::Route53::RecordSet', { Type: 'A', Name: 'tierstore.dev.', AliasTarget: Match.anyValue() });
    t.hasResourceProperties('AWS::Route53::RecordSet', { Type: 'AAAA', Name: 'tierstore.dev.', AliasTarget: Match.anyValue() });
    t.hasResourceProperties('AWS::CertificateManager::Certificate', { DomainName: 'tierstore.dev', ValidationMethod: 'DNS' });
  });

  test('the response headers are the strict set: CSP, HSTS, nosniff, DENY, no-referrer, COOP, CORP, permissions', () => {
    t.hasResourceProperties('AWS::CloudFront::ResponseHeadersPolicy', {
      ResponseHeadersPolicyConfig: Match.objectLike({
        SecurityHeadersConfig: {
          ContentSecurityPolicy: { Override: true, ContentSecurityPolicy: CONTENT_SECURITY_POLICY },
          ContentTypeOptions: { Override: true },
          StrictTransportSecurity: { AccessControlMaxAgeSec: 63072000, IncludeSubdomains: true, Override: true },
          FrameOptions: { FrameOption: 'DENY', Override: true },
          ReferrerPolicy: { ReferrerPolicy: 'no-referrer', Override: true },
        },
        CustomHeadersConfig: {
          Items: [
            { Header: 'Cross-Origin-Opener-Policy', Value: 'same-origin', Override: true },
            { Header: 'Cross-Origin-Resource-Policy', Value: 'same-origin', Override: true },
            { Header: 'Permissions-Policy', Value: 'camera=(), microphone=(), geolocation=(), payment=(), usb=()', Override: true },
          ],
        },
      }),
    });
  });

  test('the Content-Security-Policy is the one in crates/tierstore/site/served-policy.ts', () => {
    const file = path.resolve(__dirname, '../../../crates/tierstore/site/served-policy.ts');
    const js = ts.transpileModule(fs.readFileSync(file, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS } }).outputText;
    const sandbox: { exports: Record<string, unknown> } = { exports: {} };
    vm.runInNewContext(js, sandbox);
    expect(sandbox.exports.CONTENT_SECURITY_POLICY).toBe(CONTENT_SECURITY_POLICY);
    expect(CONTENT_SECURITY_POLICY).not.toContain('connect-src');
    expect(CONTENT_SECURITY_POLICY).not.toContain('unsafe-inline');
  });
});

describe('router', () => {
  const run = (uri: string): string => {
    const sandbox: { event: any; out?: string } = { event: { request: { uri } } };
    vm.runInNewContext(`${ROUTER_SOURCE}\nout = handler(event).uri;`, sandbox);
    return sandbox.out!;
  };

  test('is the inline function of the distribution', () => {
    t.hasResourceProperties('AWS::CloudFront::Function', { FunctionCode: ROUTER_SOURCE, FunctionConfig: Match.objectLike({ Runtime: 'cloudfront-js-2.0' }) });
  });

  test('a path that ends in a slash gets index.html', () => {
    expect(run('/')).toBe('/index.html');
    expect(run('/docs/')).toBe('/docs/index.html');
  });

  test('a path with a file name is left alone', () => {
    expect(run('/assets/app.js')).toBe('/assets/app.js');
  });

  test('everything under /releases/ passes through untouched, a trailing slash included', () => {
    expect(run('/releases/1.2.3/tierstore_1.2.3_amd64.deb')).toBe('/releases/1.2.3/tierstore_1.2.3_amd64.deb');
    expect(run('/releases/latest.json')).toBe('/releases/latest.json');
    expect(run('/releases/1.2.3/')).toBe('/releases/1.2.3/');
  });
});

describe('site publish role', () => {
  const role = statements('tierstore-prod-publish');

  test('trusts this repository, the site environment and the site publish workflow on main, by condition keys', () => {
    const roleResource = Object.values(t.findResources('AWS::IAM::Role', { Properties: { RoleName: 'tierstore-prod-publish' } }))[0] as any;
    const stmt = roleResource.Properties.AssumeRolePolicyDocument.Statement[0];
    expect(stmt.Action).toBe('sts:AssumeRoleWithWebIdentity');
    expect(stmt.Condition).toEqual(c.trustConditions(CONFIG, c.SITE_TRUST));
    const eq = stmt.Condition.StringEquals;
    expect(eq['token.actions.githubusercontent.com:environment']).toBe('prod-site-tierstore');
    expect(eq['token.actions.githubusercontent.com:job_workflow_ref']).toBe('example-org/example-repo/.github/workflows/tierstore-site-publish.yml@refs/heads/main');
    expect(eq['token.actions.githubusercontent.com:repository_id']).toBe('222');
    expect(eq['token.actions.githubusercontent.com:repository_owner_id']).toBe('111');
  });

  test('grants exactly what tools/publish.ts runs: list, put, delete, invalidate', () => {
    expect(Object.keys(role).sort()).toEqual(['DeleteOutsideReleases', 'Invalidate', 'ListBucket', 'NeverTouchReleases', 'PutObjects']);
    expect(role.ListBucket.Action).toBe('s3:ListBucket');
    expect(role.PutObjects.Action).toBe('s3:PutObject');
    expect(role.DeleteOutsideReleases.Action).toBe('s3:DeleteObject');
    expect(role.Invalidate.Action).toBe('cloudfront:CreateInvalidation');
    expect(resourceText(role.Invalidate)).toContain('distribution/');
  });

  test('explicitly denies writing and deleting anything under releases/', () => {
    expect(role.NeverTouchReleases.Effect).toBe('Deny');
    expect(role.NeverTouchReleases.Action).toEqual(['s3:PutObject', 's3:DeleteObject', 's3:DeleteObjectVersion']);
    expect(resourceText(role.NeverTouchReleases)).toContain('/releases/*');
  });

  test('has no other action', () => {
    const actions = Object.values(role).flatMap((s: any) => (Array.isArray(s.Action) ? s.Action : [s.Action]));
    expect(new Set(actions)).toEqual(new Set(['s3:ListBucket', 's3:PutObject', 's3:DeleteObject', 's3:DeleteObjectVersion', 'cloudfront:CreateInvalidation']));
  });
});

describe('releases role', () => {
  const role = statements('tierstore-prod-releases');

  test('trusts this repository, the release environment and the release workflow on main, by condition keys', () => {
    const roleResource = Object.values(t.findResources('AWS::IAM::Role', { Properties: { RoleName: 'tierstore-prod-releases' } }))[0] as any;
    const stmt = roleResource.Properties.AssumeRolePolicyDocument.Statement[0];
    expect(stmt.Condition).toEqual(c.trustConditions(CONFIG, c.RELEASES_TRUST));
  });

  test('may put releases/<version>/<file> only with If-None-Match: *', () => {
    expect(role.PutReleaseFilesOnce.Action).toBe('s3:PutObject');
    expect(resourceText(role.PutReleaseFilesOnce)).toContain('/releases/*/*');
    expect(role.PutReleaseFilesOnce.Condition).toEqual({ StringEquals: { 's3:if-none-match': '*' } });
  });

  test('denies a put under releases/<version>/ that lacks If-None-Match: *, even if an allow is widened', () => {
    expect(role.ReleasesWriteOnce.Effect).toBe('Deny');
    expect(role.ReleasesWriteOnce.Action).toBe('s3:PutObject');
    expect(resourceText(role.ReleasesWriteOnce)).toContain('/releases/*/*');
    expect(role.ReleasesWriteOnce.Condition).toEqual({ StringNotEquals: { 's3:if-none-match': '*' } });
  });

  test('may replace releases/latest.json, which the write-once pattern releases/*/* does not match', () => {
    expect(role.ReplaceLatest.Action).toBe('s3:PutObject');
    expect(role.ReplaceLatest.Condition).toBeUndefined();
    expect(resourceText(role.ReplaceLatest)).toContain('/releases/latest.json');
    expect('releases/latest.json').not.toMatch(/^releases\/.*\/.*$/);
  });

  test('can delete nothing in the bucket', () => {
    expect(role.NeverDelete.Effect).toBe('Deny');
    expect(role.NeverDelete.Action).toEqual(['s3:DeleteObject', 's3:DeleteObjectVersion']);
    expect(JSON.stringify(role.NeverDelete.Resource)).toMatch(/\/\*"\]\]/);
  });

  test('can invalidate the distribution, and has no read, list or multipart grant', () => {
    expect(role.Invalidate.Action).toBe('cloudfront:CreateInvalidation');
    const allowed = Object.values(role).filter((s: any) => s.Effect !== 'Deny').flatMap((s: any) => (Array.isArray(s.Action) ? s.Action : [s.Action]));
    expect(new Set(allowed)).toEqual(new Set(['s3:PutObject', 'cloudfront:CreateInvalidation']));
  });
});

describe('outputs', () => {
  test('the values vars prints are exported', () => {
    expect(Object.keys(t.toJSON().Outputs).sort()).toEqual(['BucketName', 'DistributionDomainName', 'DistributionId', 'PublishRoleArn', 'ReleasesRoleArn']);
  });
});

describe('zone stack', () => {
  const z = Template.fromStack(new ZoneStack(new App(), { env }));

  test('is a retained public zone for tierstore.dev with its name servers exported', () => {
    z.hasResourceProperties('AWS::Route53::HostedZone', { Name: 'tierstore.dev.' });
    z.hasResource('AWS::Route53::HostedZone', { DeletionPolicy: 'Retain', UpdateReplacePolicy: 'Retain' });
    expect(Object.keys(z.toJSON().Outputs).sort()).toEqual(['NameServers', 'ZoneName']);
  });
});
