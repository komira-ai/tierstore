import { Duration, LegacyStackSynthesizer, RemovalPolicy, Stack, StackProps, CfnOutput, Token } from 'aws-cdk-lib';
import * as acm from 'aws-cdk-lib/aws-certificatemanager';
import * as cloudfront from 'aws-cdk-lib/aws-cloudfront';
import * as origins from 'aws-cdk-lib/aws-cloudfront-origins';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as route53 from 'aws-cdk-lib/aws-route53';
import * as targets from 'aws-cdk-lib/aws-route53-targets';
import * as s3 from 'aws-cdk-lib/aws-s3';
import { Construct } from 'constructs';
import * as c from './config';

export interface SiteStackProps extends Omit<StackProps, 'synthesizer'> {
  config: c.SiteConfig;
  /** ARN of the account's EXISTING GitHub OIDC provider. This stack never creates one. */
  oidcProviderArn: string;
  /** Dummy-synth only (tests): skips the hosted zone lookup. Real deploys look the zone up by name. */
  hostedZoneId?: string;
}

/**
 * Strict CSP. The site fetches nothing, so there is no connect-src (default-src 'none' covers it). Pages must use
 * external scripts and styles from the same origin: inline <style>/<script> are blocked. This is a copy of
 * crates/tierstore/site/served-policy.ts, which the site's test (csp.test.ts) checks the built pages against;
 * test/site-stack.test.ts fails when the two differ, so a change is made in both in the same pull request.
 */
export const CONTENT_SECURITY_POLICY = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
  "object-src 'none'",
].join('; ');

export const PERMISSIONS_POLICY = 'camera=(), microphone=(), geolocation=(), payment=(), usb=()';
export const NONCURRENT_VERSION_DAYS = 365;
export const ERROR_TTL = Duration.seconds(10);

/**
 * Viewer-request function (cloudfront-js-2.0). First: a request for www.tierstore.dev (whatever the path, /releases/
 * included) is answered with a 301 to https://tierstore.dev with the same path and query string, and goes no further.
 * Then: `/x/` -> `/x/index.html`. Everything under /releases/ is a download and is passed through untouched (the router
 * never rewrites it, including a path that ends in a slash).
 */
export const ROUTER_SOURCE = `function handler(event) {
  var r = event.request;
  var uri = r.uri;
  var host = r.headers && r.headers.host ? r.headers.host.value.toLowerCase() : '';
  if (host === '${c.WWW_DOMAIN}') {
    var pairs = [];
    var qs = r.querystring || {};
    Object.keys(qs).forEach(function (k) {
      var q = qs[k];
      var values = q.multiValue ? q.multiValue.map(function (m) { return m.value; }) : [q.value];
      values.forEach(function (v) { pairs.push(v === '' ? k : k + '=' + v); });
    });
    return {
      statusCode: 301,
      statusDescription: 'Moved Permanently',
      headers: { location: { value: 'https://${c.DOMAIN}' + uri + (pairs.length ? '?' + pairs.join('&') : '') } }
    };
  }
  if (uri.indexOf('/releases/') === 0) return r;
  if (uri.charAt(uri.length - 1) === '/') r.uri = uri + 'index.html';
  return r;
}
`;

/**
 * Private S3 bucket + CloudFront (OAC) for tierstore.dev, plus the two roles the repository's workflows assume: the
 * site publish role (pages) and the releases role (release files under releases/).
 */
export class SiteStack extends Stack {
  readonly bucket: s3.Bucket;
  readonly distribution: cloudfront.Distribution;
  readonly publishRole: iam.Role;
  readonly releasesRole: iam.Role;

  constructor(scope: Construct, props: SiteStackProps) {
    // No assets, so no cdk bootstrap is needed; the admin deploys with their own credentials.
    super(scope, c.SITE_STACK_NAME, { ...props, synthesizer: new LegacyStackSynthesizer() });
    if (!Token.isUnresolved(this.region) && this.region !== c.REGION) {
      throw new Error(`must be deployed to ${c.REGION}; got ${this.region}`);
    }
    if (!/^arn:aws:iam::\d{12}:oidc-provider\/token\.actions\.githubusercontent\.com$/.test(props.oidcProviderArn)) {
      throw new Error('oidcProviderArn must be the ARN of the account GitHub OIDC provider');
    }

    this.bucket = new s3.Bucket(this, 'Bucket', {
      bucketName: c.bucketName(this.account),
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      encryption: s3.BucketEncryption.S3_MANAGED,
      versioned: true,
      enforceSSL: true,
      objectOwnership: s3.ObjectOwnership.BUCKET_OWNER_ENFORCED,
      removalPolicy: RemovalPolicy.RETAIN,
      lifecycleRules: [
        {
          // Only superseded versions expire (365 days, so a replaced latest.json or page stays recoverable for a year).
          // Current objects, releases included, are kept forever. An upload that never completed is dropped after 7 days.
          id: 'expire-noncurrent-versions',
          noncurrentVersionExpiration: Duration.days(NONCURRENT_VERSION_DAYS),
          abortIncompleteMultipartUploadAfter: Duration.days(7),
        },
      ],
    });

    const zone = props.hostedZoneId
      ? route53.HostedZone.fromHostedZoneAttributes(this, 'Zone', { hostedZoneId: props.hostedZoneId, zoneName: c.DOMAIN })
      : route53.HostedZone.fromLookup(this, 'Zone', { domainName: c.DOMAIN });
    const cert = new acm.Certificate(this, 'Cert', {
      domainName: c.DOMAIN,
      subjectAlternativeNames: [c.WWW_DOMAIN],
      validation: acm.CertificateValidation.fromDns(zone),
    });

    const headers = new cloudfront.ResponseHeadersPolicy(this, 'Headers', {
      responseHeadersPolicyName: c.HEADERS_POLICY_NAME,
      securityHeadersBehavior: {
        contentSecurityPolicy: { contentSecurityPolicy: CONTENT_SECURITY_POLICY, override: true },
        contentTypeOptions: { override: true },
        strictTransportSecurity: { accessControlMaxAge: Duration.days(730), includeSubdomains: true, override: true },
        frameOptions: { frameOption: cloudfront.HeadersFrameOption.DENY, override: true },
        referrerPolicy: { referrerPolicy: cloudfront.HeadersReferrerPolicy.NO_REFERRER, override: true },
      },
      customHeadersBehavior: {
        customHeaders: [
          { header: 'Cross-Origin-Opener-Policy', value: 'same-origin', override: true },
          { header: 'Cross-Origin-Resource-Policy', value: 'same-origin', override: true },
          { header: 'Permissions-Policy', value: PERMISSIONS_POLICY, override: true },
        ],
      },
    });

    const router = new cloudfront.Function(this, 'Router', {
      functionName: c.ROUTER_NAME,
      comment: `${c.SITE_ID} site: ${c.WWW_DOMAIN} -> 301 ${c.DOMAIN}; /x/ -> /x/index.html; /releases/* untouched`,
      runtime: cloudfront.FunctionRuntime.JS_2_0,
      code: cloudfront.FunctionCode.fromInline(ROUTER_SOURCE),
    });

    // OAC has no ListBucket, so a missing key answers 403; both get the 404 page with status 404.
    const notFound = (httpStatus: number): cloudfront.ErrorResponse => ({
      httpStatus,
      responseHttpStatus: 404,
      responsePagePath: '/404.html',
      ttl: ERROR_TTL,
    });

    this.distribution = new cloudfront.Distribution(this, 'Distribution', {
      comment: `${c.DOMAIN} site and releases`,
      domainNames: [c.DOMAIN, c.WWW_DOMAIN],
      certificate: cert,
      defaultRootObject: 'index.html',
      minimumProtocolVersion: cloudfront.SecurityPolicyProtocol.TLS_V1_2_2021,
      httpVersion: cloudfront.HttpVersion.HTTP2_AND_3,
      errorResponses: [notFound(403), notFound(404)],
      defaultBehavior: {
        origin: origins.S3BucketOrigin.withOriginAccessControl(this.bucket),
        viewerProtocolPolicy: cloudfront.ViewerProtocolPolicy.REDIRECT_TO_HTTPS,
        allowedMethods: cloudfront.AllowedMethods.ALLOW_GET_HEAD,
        cachePolicy: cloudfront.CachePolicy.CACHING_OPTIMIZED,
        responseHeadersPolicy: headers,
        functionAssociations: [{ function: router, eventType: cloudfront.FunctionEventType.VIEWER_REQUEST }],
      },
    });

    const target = route53.RecordTarget.fromAlias(new targets.CloudFrontTarget(this.distribution));
    new route53.ARecord(this, 'ARecord', { zone, recordName: c.DOMAIN, target });
    new route53.AaaaRecord(this, 'AaaaRecord', { zone, recordName: c.DOMAIN, target });
    new route53.ARecord(this, 'WwwARecord', { zone, recordName: c.WWW_DOMAIN, target });
    new route53.AaaaRecord(this, 'WwwAaaaRecord', { zone, recordName: c.WWW_DOMAIN, target });

    const distributionArn = `arn:aws:cloudfront::${this.account}:distribution/${this.distribution.distributionId}`;
    const releaseObjects = this.bucket.arnForObjects(`${c.RELEASES_PREFIX}*`);
    const federated = (trust: c.Trust) =>
      new iam.FederatedPrincipal(props.oidcProviderArn, c.trustConditions(props.config, trust), 'sts:AssumeRoleWithWebIdentity');

    // The site publish role: assumed by tools/publish.ts in the site publish workflow only. No CDK, no IAM, nothing
    // under releases/ (an explicit deny holds even if a later edit widens an allow).
    this.publishRole = new iam.Role(this, 'PublishRole', {
      roleName: c.SITE_ROLE_NAME,
      description: `${c.SITE_ID} site-publish workflow: uploads to the ${c.DOMAIN} bucket and invalidates its distribution`,
      maxSessionDuration: Duration.hours(1),
      assumedBy: federated(c.SITE_TRUST),
    });
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({ sid: 'ListBucket', actions: ['s3:ListBucket'], resources: [this.bucket.bucketArn] }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({ sid: 'PutObjects', actions: ['s3:PutObject'], resources: [this.bucket.arnForObjects('*')] }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({ sid: 'DeleteOutsideReleases', actions: ['s3:DeleteObject'], resources: [this.bucket.arnForObjects('*')] }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'NeverTouchReleases',
        effect: iam.Effect.DENY,
        actions: ['s3:PutObject', 's3:DeleteObject', 's3:DeleteObjectVersion'],
        resources: [releaseObjects],
      }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({ sid: 'Invalidate', actions: ['cloudfront:CreateInvalidation'], resources: [distributionArn] }),
    );

    // The releases role: assumed by the release workflow's upload job only. It writes releases/<version>/<file> once and
    // releases/latest.json (the one object that is replaced), reads and lists nothing, and deletes nothing anywhere.
    // Uploads are single-part (aws s3api put-object, at most 5 GiB): a multipart upload sends its parts without the
    // If-None-Match header, which the policy below refuses, so a multipart upload to releases/<version>/ fails by design.
    this.releasesRole = new iam.Role(this, 'ReleasesRole', {
      roleName: c.RELEASES_ROLE_NAME,
      description: `${c.SITE_ID} release workflow: uploads release files to releases/ of the ${c.DOMAIN} bucket`,
      maxSessionDuration: Duration.hours(1),
      assumedBy: federated(c.RELEASES_TRUST),
    });
    this.releasesRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'PutReleaseFilesOnce',
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects(`${c.RELEASES_PREFIX}*/*`)],
        conditions: { StringEquals: { 's3:if-none-match': '*' } },
      }),
    );
    // Written once: a PutObject under releases/<version>/ must carry If-None-Match: * (S3 conditional write), so a
    // re-publish of an existing version fails instead of overwriting. The deny states it again, so a later edit that
    // widens the allow above still cannot write without the header. releases/latest.json sits directly under releases/,
    // so the pattern releases/*/* does not match it.
    this.releasesRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'ReleasesWriteOnce',
        effect: iam.Effect.DENY,
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects(`${c.RELEASES_PREFIX}*/*`)],
        conditions: { StringNotEquals: { 's3:if-none-match': '*' } },
      }),
    );
    this.releasesRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'ReplaceLatest',
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects(`${c.RELEASES_PREFIX}latest.json`)],
      }),
    );
    this.releasesRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'NeverDelete',
        effect: iam.Effect.DENY,
        actions: ['s3:DeleteObject', 's3:DeleteObjectVersion'],
        resources: [this.bucket.arnForObjects('*')],
      }),
    );
    // latest.json is cached by CloudFront; the release job invalidates it after replacing it.
    this.releasesRole.addToPolicy(
      new iam.PolicyStatement({ sid: 'Invalidate', actions: ['cloudfront:CreateInvalidation'], resources: [distributionArn] }),
    );

    new CfnOutput(this, 'BucketName', { value: this.bucket.bucketName });
    new CfnOutput(this, 'DistributionId', { value: this.distribution.distributionId });
    new CfnOutput(this, 'DistributionDomainName', { value: this.distribution.distributionDomainName });
    new CfnOutput(this, 'PublishRoleArn', { value: this.publishRole.roleArn });
    new CfnOutput(this, 'ReleasesRoleArn', { value: this.releasesRole.roleArn });
  }
}
