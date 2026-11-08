// The Content-Security-Policy tierstore.dev is served under. The header is set by the site's hosting
// (infra/site/lib/site-stack.ts, CONTENT_SECURITY_POLICY); this is a copy of it, in the same form, which csp.test.ts
// checks the built pages against, and infra/site/test/site-stack.test.ts fails when the two differ. A change to the
// policy is made in both in the same pull request.
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
