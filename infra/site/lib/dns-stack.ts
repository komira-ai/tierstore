import { CfnOutput, Fn, LegacyStackSynthesizer, RemovalPolicy, Stack, StackProps } from 'aws-cdk-lib';
import { PublicHostedZone } from 'aws-cdk-lib/aws-route53';
import { Construct } from 'constructs';
import { DOMAIN, ZONE_STACK_NAME } from './config';

/** The public hosted zone of tierstore.dev and nothing else. Retained: a new zone would have new name servers. */
export class ZoneStack extends Stack {
  readonly zone: PublicHostedZone;

  constructor(scope: Construct, props: StackProps) {
    // The legacy synthesizer needs no cdk bootstrap; the admin deploys with their own credentials.
    super(scope, ZONE_STACK_NAME, { ...props, synthesizer: new LegacyStackSynthesizer() });
    this.zone = new PublicHostedZone(this, 'Zone', { zoneName: DOMAIN, comment: `${DOMAIN} public zone` });
    this.zone.applyRemovalPolicy(RemovalPolicy.RETAIN);
    new CfnOutput(this, 'NameServers', {
      description: 'The four name servers of this zone, comma separated',
      value: Fn.join(',', this.zone.hostedZoneNameServers!),
    });
    new CfnOutput(this, 'ZoneName', { value: this.zone.zoneName });
  }
}
