# Yield issue gateway (staged)

This Worker is the proposed backend for the Yield feedback prompt. It has not been deployed. Its example config intentionally contains invalid placeholders, so a deployment cannot accidentally reuse the legacy Wavecrate KV namespace, OAuth app, or GitHub repository.

## Contract

1. The app generates a random request ID and opens `/auth/start?requestId=...` in the user's browser.
2. The Worker binds it to a short-lived GitHub OAuth state. Its callback exchanges the code server-side, stores the GitHub token in a new Yield KV namespace, and shows a page with no token.
3. The app polls `/auth/poll?requestId=...`, receives a high-entropy gateway session ID, and stores that ID in the OS keyring under Yield identifiers. Missing or unavailable keyring storage is a visible error; there is no plaintext fallback.
4. The app sends one `POST /issue` with the gateway session. The Worker sends one GitHub issue creation POST. Transport errors and GitHub 5xx responses have an **unknown** outcome; the app must ask the user to inspect the target repository before retrying. No automatic issue POST retry is allowed.

If the user denies GitHub authorization, the callback records a short-lived denial and the next poll returns an explicit 403. If the app closes the connection flow before GitHub redirects, it stops polling; the unused OAuth state expires after ten minutes. Successful poll handoffs expire after five minutes, and gateway sessions after seven days.

The Worker emits generic errors without OAuth codes, GitHub tokens, gateway session IDs, issue bodies, or upstream response bodies. The browser callback never shows a token. Request/response bodies are bounded.

## Deployment prerequisites

- The approved feedback destination is the existing private `PORTALSURFER/yield` repository. GitHub's OAuth `repo` scope grants access to all private repositories the authorizing user can access, not just Yield. Review that broad grant in the GitHub consent screen before connecting; the Worker only creates issues in `PORTALSURFER/yield`.
- Create a separate GitHub OAuth app with callback `https://<new-yield-worker-host>/auth/callback`. Store its client secret using Wrangler secrets, never in this repo.
- Create a new Yield KV namespace for `SESSIONS`. Do not reuse the legacy namespace or its sessions. Copy `wrangler.example.jsonc` to `wrangler.jsonc` and fill the new namespace ID; keep the approved repository and scope. The client ID is a deployment variable; the client secret is a secret.
- Review the Worker target and run its tests/typecheck. Run an OAuth connect, expiry, rejection, and confirmed issue-creation session against the approved target before pointing a release build at the Worker. Deployment and live issue creation require explicit approval.
- Set the app's `YIELD_ISSUE_GATEWAY_URL` to the approved HTTPS Worker base URL. Without it, the Rust client reports that feedback is unconfigured. The GPUI prompt integration remains OPT-1896.

## Local checks

From this directory: `pnpm install --frozen-lockfile`, `pnpm test`, and `pnpm run typecheck`. The Worker tests use an in-memory KV and mock GitHub requests; they create no real issues or credentials. Rust client checks: `cargo test --locked -p yield-issue-gateway --lib` from the repository root.

Cloudflare KV is eventually consistent, so it is used for expiring auth/session lookups, not as a lock for issue-creation idempotency. The single-POST/unknown-outcome rule is deliberate.
