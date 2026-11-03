// The Content-Security-Policy tierstore.dev is served under. The header is set by the site's hosting, outside this
// repository; this is a copy of its CONTENT_SECURITY_POLICY, in the same form, which csp.test.ts checks the built pages
// against. A change to the policy there is made here in the same step.
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
