/** Yield's native-app GitHub issue gateway. No user token is returned in a page or log. */

export interface SessionStore {
  get(key: string): Promise<string | null>;
  put(key: string, value: string, options: { expirationTtl: number }): Promise<void>;
  delete(key: string): Promise<void>;
}

export interface RateLimiter {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

export interface GatewayEnv {
  GITHUB_CLIENT_ID: string;
  GITHUB_CLIENT_SECRET: string;
  GITHUB_OWNER: string;
  GITHUB_REPO: string;
  GITHUB_OAUTH_SCOPE: 'public_repo' | 'repo';
  SESSIONS: SessionStore;
  EDGE_RATE_LIMIT: RateLimiter;
  ISSUE_SESSION_RATE_LIMIT: RateLimiter;
}

type Fetcher = typeof fetch;
const SESSION_SECONDS = 7 * 24 * 60 * 60;
const STATE_SECONDS = 10 * 60;
const POLL_SECONDS = 5 * 60;
const APPROVED_OWNER = 'PORTALSURFER';
const APPROVED_REPO = 'yield';
const MAX_TITLE_BYTES = 200;
const MAX_BODY_BYTES = 60_000;
// Allow the native JSON encoder to escape quotes, backslashes, tabs and newlines.
const MAX_REQUEST_BYTES = 128 * 1024;
const REQUEST_ID = /^[0-9a-f]{64}$/;
const SESSION_ID = /^[A-Za-z0-9_-]{32,128}$/;
const UTF8 = new TextEncoder();
const AUTH_PATHS = new Set(['/auth/start', '/auth/callback', '/auth/poll']);

function json(value: object, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' },
  });
}

function randomToken(bytes = 32): string {
  const raw = new Uint8Array(bytes);
  crypto.getRandomValues(raw);
  return btoa(String.fromCharCode(...raw))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/g, '');
}

function configured(env: GatewayEnv): boolean {
  return Boolean(
    env.GITHUB_CLIENT_ID &&
    env.GITHUB_CLIENT_SECRET &&
    env.GITHUB_OWNER === APPROVED_OWNER &&
    env.GITHUB_REPO === APPROVED_REPO &&
    env.GITHUB_OAUTH_SCOPE === 'repo' &&
    env.SESSIONS &&
    typeof env.EDGE_RATE_LIMIT?.limit === 'function' &&
    typeof env.ISSUE_SESSION_RATE_LIMIT?.limit === 'function',
  );
}

async function boundedText(response: Request | Response, maxBytes: number): Promise<string> {
  const reader = response.body?.getReader();
  if (!reader) return '';
  const chunks: Uint8Array[] = [];
  let size = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > maxBytes) {
      await reader.cancel();
      throw new Error('body too large');
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
}

function issuePayload(payload: unknown): { title: string; body: string } | null {
  if (!payload || typeof payload !== 'object' || Array.isArray(payload) || Object.keys(payload).length !== 2)
    return null;
  const fields = payload as Record<string, unknown>;
  if (typeof fields.title !== 'string' || typeof fields.body !== 'string') return null;
  const title = fields.title.trim();
  const body = fields.body.replace(/\r\n?/g, '\n');
  const titleBytes = UTF8.encode(title).byteLength;
  if (titleBytes < 3 || titleBytes > MAX_TITLE_BYTES || /[\x00-\x1f\x7f\u2028\u2029]/.test(title))
    return null;
  if (!body.trim() || UTF8.encode(body).byteLength > MAX_BODY_BYTES || /[\x00-\x08\x0b-\x1f\x7f]/.test(body))
    return null;
  return { title, body };
}

function bearer(request: Request): string | null {
  const match = /^Bearer ([A-Za-z0-9_-]{32,128})$/i.exec(request.headers.get('authorization') ?? '');
  return match?.[1] ?? null;
}

function edgeBucket(request: Request, path: string): string {
  // Cloudflare's edge header is the only IP input. Unknown values share a bucket.
  const ip = request.headers.get('cf-connecting-ip') ?? '';
  return `${path}:${/^[0-9a-f:.]{1,64}$/i.test(ip) ? ip : 'unknown'}`;
}

async function sha256Hex(value: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', UTF8.encode(value));
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

async function enforceRate(limiter: RateLimiter, key: string | Promise<string>): Promise<Response | null> {
  try {
    if ((await limiter.limit({ key: await key })).success) return null;
  } catch {
    return json({ error: 'Gateway unavailable' }, 503);
  }
  const response = json({ error: 'Too many feedback requests. Try again in a minute.' }, 429);
  response.headers.set('Retry-After', '60');
  return response;
}

export function createWorker(fetcher: Fetcher) {
  return {
    async fetch(request: Request, env: GatewayEnv): Promise<Response> {
      if (!configured(env)) return json({ error: 'Gateway unavailable' }, 503);
      const url = new URL(request.url);
      const issueRequest = request.method === 'POST' && url.pathname === '/issue';
      if (issueRequest || (request.method === 'GET' && AUTH_PATHS.has(url.pathname))) {
        const rejection = await enforceRate(env.EDGE_RATE_LIMIT, edgeBucket(request, url.pathname));
        if (rejection) return rejection;
      }

      if (request.method === 'GET' && url.pathname === '/auth/start') {
        const requestId = url.searchParams.get('requestId') ?? '';
        if (!REQUEST_ID.test(requestId)) return json({ error: 'Invalid request ID' }, 400);
        const state = randomToken();
        await env.SESSIONS.put(`state:${state}`, requestId, { expirationTtl: STATE_SECONDS });
        const authorize = new URL('https://github.com/login/oauth/authorize');
        authorize.searchParams.set('client_id', env.GITHUB_CLIENT_ID);
        authorize.searchParams.set('state', state);
        authorize.searchParams.set('scope', env.GITHUB_OAUTH_SCOPE);
        return new Response(null, {
          status: 302,
          headers: { location: authorize.toString(), 'cache-control': 'no-store' },
        });
      }

      if (request.method === 'GET' && url.pathname === '/auth/callback') {
        const code = url.searchParams.get('code') ?? '';
        const state = url.searchParams.get('state') ?? '';
        const denied = url.searchParams.has('error');
        if ((!code && !denied) || !SESSION_ID.test(state))
          return json({ error: 'Invalid authorization callback' }, 400);
        const requestId = await env.SESSIONS.get(`state:${state}`);
        if (!requestId || !REQUEST_ID.test(requestId)) return json({ error: 'Authorization expired' }, 400);
        await env.SESSIONS.delete(`state:${state}`);
        if (denied) {
          await env.SESSIONS.put(`poll:${requestId}`, 'denied', { expirationTtl: POLL_SECONDS });
          return new Response('GitHub connection canceled. Return to Yield; you can close this tab.', {
            headers: { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' },
          });
        }
        let githubToken: string;
        try {
          const response = await fetcher('https://github.com/login/oauth/access_token', {
            method: 'POST',
            headers: { accept: 'application/json', 'content-type': 'application/json' },
            body: JSON.stringify({
              client_id: env.GITHUB_CLIENT_ID,
              client_secret: env.GITHUB_CLIENT_SECRET,
              code,
            }),
          });
          const payload = JSON.parse(await boundedText(response, 16 * 1024));
          if (!response.ok || typeof payload.access_token !== 'string' || !payload.access_token)
            throw new Error('exchange rejected');
          githubToken = payload.access_token;
        } catch {
          return json({ error: 'GitHub authorization failed' }, 502);
        }
        const sessionId = randomToken();
        await env.SESSIONS.put(`sess:${sessionId}`, githubToken, { expirationTtl: SESSION_SECONDS });
        await env.SESSIONS.put(`poll:${requestId}`, sessionId, { expirationTtl: POLL_SECONDS });
        return new Response('GitHub connected. Return to Yield; you can close this tab.', {
          headers: { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' },
        });
      }

      if (request.method === 'GET' && url.pathname === '/auth/poll') {
        const requestId = url.searchParams.get('requestId') ?? '';
        if (!REQUEST_ID.test(requestId)) return json({ error: 'Invalid request ID' }, 400);
        const proof = bearer(request);
        if (!proof || !REQUEST_ID.test(proof) || await sha256Hex(proof) !== requestId)
          return json({ error: 'Feedback request proof is missing or invalid' }, 401);
        const sessionId = await env.SESSIONS.get(`poll:${requestId}`);
        if (!sessionId) return json({ ok: false }, 202);
        await env.SESSIONS.delete(`poll:${requestId}`);
        if (sessionId === 'denied') return json({ error: 'GitHub connection canceled' }, 403);
        if (!SESSION_ID.test(sessionId)) return json({ error: 'Gateway unavailable' }, 503);
        return json({ ok: true, sessionId });
      }

      if (issueRequest) {
        const sessionId = bearer(request);
        if (!sessionId) return json({ error: 'Connect GitHub first' }, 401);
        const githubToken = await env.SESSIONS.get(`sess:${sessionId}`);
        if (!githubToken) return json({ error: 'GitHub connection expired' }, 401);
        let payload: unknown;
        try {
          payload = JSON.parse(await boundedText(request, MAX_REQUEST_BYTES));
        } catch {
          return json({ error: 'Invalid or oversized issue request' }, 400);
        }
        const draft = issuePayload(payload);
        if (!draft) return json({ error: 'Invalid issue title or body' }, 400);
        const rejection = await enforceRate(env.ISSUE_SESSION_RATE_LIMIT, sha256Hex(sessionId));
        if (rejection) return rejection;
        const issueUrl = `https://api.github.com/repos/${APPROVED_OWNER}/${APPROVED_REPO}/issues`;
        let response: Response;
        try {
          // Deliberately one POST. A network/5xx outcome may have created the issue.
          response = await fetcher(issueUrl, {
            method: 'POST',
            headers: {
              accept: 'application/vnd.github+json',
              'content-type': 'application/json',
              authorization: `Bearer ${githubToken}`,
              'user-agent': 'yield-issue-gateway',
              'x-github-api-version': '2022-11-28',
            },
            body: JSON.stringify(draft),
          });
        } catch {
          return json({ error: 'Submission outcome unknown; check the repository before retrying', outcome: 'unknown' }, 502);
        }
        if (response.status === 401 || response.status === 403)
          return json({ error: 'GitHub authorization or permission failed' }, 403);
        if (response.status === 422)
          return json({ error: 'GitHub rejected the issue content' }, 422);
        if (!response.ok)
          return json({ error: 'Submission outcome unknown; check the repository before retrying', outcome: 'unknown' }, 502);
        try {
          const issue = JSON.parse(await boundedText(response, 32 * 1024));
          const expected = `https://github.com/${APPROVED_OWNER}/${APPROVED_REPO}/issues/`;
          if (!Number.isSafeInteger(issue.number) || issue.number <= 0 ||
              issue.html_url !== `${expected}${issue.number}`) throw new Error('invalid response');
          return json({ ok: true, issue_url: issue.html_url, number: issue.number });
        } catch {
          return json({ error: 'Submission outcome unknown; check the repository before retrying', outcome: 'unknown' }, 502);
        }
      }
      return json({ error: 'Not found' }, 404);
    },
  };
}

export default createWorker(fetch);
