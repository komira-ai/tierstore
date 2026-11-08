# infra/site

The AWS side of tierstore.dev, as a CDK app, run once by an operator with their own credentials. Nothing in CI
deploys it: CI runs its tests (`.github/workflows/tierstore-site.yml`) and the workflows it serves only publish.

| Stack | What it holds |
|---|---|
| `tierstore-dns-zone` | the public Route 53 zone of `tierstore.dev` (retained: a new zone would have new name servers) |
| `tierstore-prod-site` | the private S3 bucket `tierstore-prod-site-<account>` (versioned, encrypted, TLS only, retained), the CloudFront distribution in front of it (origin access control, the ACM certificate for `tierstore.dev` and `www.tierstore.dev`, A and AAAA records for both names, the security headers, the router function) and two IAM roles |

Region is `us-east-1` for both (CloudFront certificates must live there). The site is under the bucket root; release
files are public downloads under `https://tierstore.dev/releases/<version>/<file>`, with `releases/latest.json` beside
them. The router passes `/releases/` through untouched and the headers policy is the site's.

**`www.tierstore.dev`** is a name of the same distribution (alternate domain name, certificate SAN, and A and AAAA alias
records in the Route 53 zone, not at the registrar). The router answers any request for that host, `/releases/` included,
with `301` to `https://tierstore.dev<path>?<query>` before it does anything else; the apex is routed as before. The
certificate's names are part of the stack, so adding `www` replaced the certificate: CloudFormation creates the new
certificate (validated by two DNS records in the zone), points the distribution at it, and only then deletes the old one,
so the apex keeps serving. The owner's step for the change is to run the `site` step again: the dry run, then `--apply`
(step 4 below). Nothing changes at the registrar.

## The two roles

Both are assumed with GitHub's OIDC token (no stored key), and each trusts the repository, one GitHub environment and one
workflow file on `main`.

| Role | Environment | Workflow | May |
|---|---|---|---|
| `tierstore-prod-publish` | `prod-site-tierstore` | `.github/workflows/tierstore-site-publish.yml` | list the bucket; put and delete objects outside `releases/` (what `crates/tierstore/site/tools/publish.ts` runs: `s3 sync --delete`, `s3 cp`); create an invalidation. Explicitly denied: put, delete and delete-version under `releases/` |
| `tierstore-prod-releases` | `prod-releases-tierstore` | `.github/workflows/release.yml` | put `releases/<version>/<file>` only with `If-None-Match: *`; put `releases/latest.json`; read `releases/*` (`GetObject`, public downloads anyway); list the bucket only with an `s3:prefix` under `releases/` (so a missing key answers 404, not 403); create an invalidation. Explicitly denied: put without `If-None-Match: *` under `releases/<version>/`, and delete or delete-version anywhere. These reads are what `cargo xtask channels upload` needs for a re-run and for `latest.json` |

**Single-part uploads.** A multipart upload sends its parts without the `If-None-Match` header, so the releases role
refuses them by design. The release job uploads each file with one `aws s3api put-object ... --if-none-match '*'`
(at most 5 GiB per file). A second upload of an existing version fails with `412 PreconditionFailed`: release files are
written once. `releases/latest.json` is the one object that is replaced; the job then invalidates `/releases/latest.json`.

**Trust, and the `sub` template.** The repository keeps GitHub's default immutable subject template
(`repo:<owner>@<owner id>/<name>@<repo id>:environment:<env>`); nothing here asks to change it. The trust names the four claims
AWS accepts as condition keys, each of which must hold whatever the template is (`environment`, `job_workflow_ref`,
`repository_id`, `repository_owner_id`), plus `aud`, and the `sub` in its two exact forms (with and without
`:job_workflow_ref:...`). The numeric ids are read by the CLI from `gh api repos/<owner>/<name>` and passed to CDK as
context; they are not in this repository. The CLI refuses a `--repo` whose GitHub name differs from the one given.

## The GitHub side

The environments already exist, each limited to the branch `main`: `prod-site-tierstore`, `prod-releases-tierstore`,
`release`, `apt-publish`, `apt-signing`. This app creates none; `github` and `vars` only check them.

## Steps (the owner runs them, in this order)

Every command below is a dry run unless it says `--apply`; there is no `--force`; every AWS step refuses unless the
profile's account is the `--account` given. Use your own admin profile for `<profile>` and its 12-digit account id for
`<account>`. Needs Node as in `crates/tierstore/site/.node-version`, the AWS CLI and `gh` (logged in).

```
cd infra/site
npm ci --ignore-scripts

# 0. optional: the environments exist and deploy from main only (read-only)
npm run held -- github --repo komira-ai/tierstore

# 1. the DNS zone: the dry run shows `cdk diff`; then create it
npm run held -- dns --account <account> --profile <profile>
npm run held -- dns --account <account> --profile <profile> --apply
```

2. `dns --apply` prints the four name servers. At the registrar (GoDaddy), set the domain's nameservers to those four,
   one per line, without trailing dots.
3. Wait until the delegation answers, and check it: `dig NS tierstore.dev +short` shows exactly those four. The site
   stack's certificate is validated through this zone, so the next step waits (up to about 30 minutes) until it does.

```
# 4. the site: bucket, distribution, certificate (apex and www), records, the router, the two roles; run it again after any change here
npm run held -- site --account <account> --profile <profile> --repo komira-ai/tierstore
npm run held -- site --account <account> --profile <profile> --repo komira-ai/tierstore --apply

# 5. the GitHub variables: checks the two environments, prints six `gh variable set` commands; run them
npm run held -- vars --account <account> --profile <profile> --repo komira-ai/tierstore

# 6. turn publishing on (the workflow's deploy job is skipped until this is `true`)
gh variable set TIERSTORE_SITE_PUBLISH --body true --repo komira-ai/tierstore
```

`site` refuses, and says so, when the account has no GitHub OIDC provider. Create it once with
`npm run held -- oidc-provider --account <account> --profile <profile>` (dry run), then with `--apply`; it creates the
provider only when absent and never touches an existing one (an account holds one per URL).

The variables `vars` prints are `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID` and
`TIERSTORE_SITE_PUBLISH_ROLE_ARN` in `prod-site-tierstore`, and `TIERSTORE_RELEASES_BUCKET`,
`TIERSTORE_RELEASES_ROLE_ARN` and `TIERSTORE_RELEASES_DISTRIBUTION_ID` in `prod-releases-tierstore`. There is no secret.

`release.yml` must run its upload job in the environment `prod-releases-tierstore` from `main`, and the repository's
workflow lint must allow `id-token: write` for that job; until then the releases role can be assumed by nobody.

## Undo

- Stop publishing at once: `gh variable set TIERSTORE_SITE_PUBLISH --body false --repo komira-ai/tierstore`
  (or delete the variable). The site stays as it is.
- Stop the roles being assumed: delete the six variables, or deploy the site stack again after removing a role here.
- Remove the site stack: `aws cloudformation delete-stack --stack-name tierstore-prod-site --region us-east-1 --profile <profile>`.
  The bucket is **retained** (it holds the releases, which are never deleted by CI); the distribution, certificate, records
  and roles go. To remove the bucket as well, an admin empties it, versions and delete markers included, in the S3 console
  or with `aws s3api delete-object --version-id`, then deletes it; nothing in this app does that.
- Remove the zone: delete the stack `tierstore-dns-zone` the same way; the zone is retained, so delete its records and the
  zone in Route 53 by hand, and point the registrar elsewhere first.

## Development

```
npm ci --ignore-scripts
npm run typecheck
npm test          # jest with coverage; fails below 99% of lines or branches
npx cdk synth -c account=<12 digits> -c githubRepo=<owner>/<name> -c githubRepoId=<id> -c githubOwnerId=<id>
```

`synth` looks the hosted zone up by name and so needs AWS credentials for the account; the tests do not. The
Content-Security-Policy in `lib/site-stack.ts` is held equal to `crates/tierstore/site/served-policy.ts` by
`test/site-stack.test.ts`; change both together. Dependencies are exact versions with a lockfile, installed with
`npm ci --ignore-scripts`.
