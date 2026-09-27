import { describe, expect, it } from 'vitest';
import { createWorker, type GatewayEnv, type SessionStore } from '../src/index';

class MemoryStore implements SessionStore {
  values = new Map<string, string>();
  expirations = new Map<string, number>();
  now = 0;
  advance(seconds: number) { this.now += seconds; }
  async get(key: string) {
    if ((this.expirations.get(key) ?? Infinity) <= this.now) {
      await this.delete(key);
      return null;
    }
    return this.values.get(key) ?? null;
  }
  async put(key: string, value: string, options: { expirationTtl: number }) {
    this.values.set(key, value);
    this.expirations.set(key, this.now + options.expirationTtl);
  }
  async delete(key: string) {
    this.values.delete(key);
    this.expirations.delete(key);
  }
}

function fixture(fetcher: typeof fetch = async () => { throw new Error('unexpected network'); }) {
  const sessions = new MemoryStore();
  const env: GatewayEnv = {
    GITHUB_CLIENT_ID: 'public-client',
    GITHUB_CLIENT_SECRET: 'private-secret-never-show',
    GITHUB_OWNER: 'PORTALSURFER',
    GITHUB_REPO: 'yield',
    GITHUB_OAUTH_SCOPE: 'repo',
    SESSIONS: sessions,
  };
  const worker = createWorker(fetcher);
  const call = (path: string, init?: RequestInit) => worker.fetch(new Request(`https://gateway.test${path}`, init), env);
  return { sessions, env, call };
}

const requestId = 'a'.repeat(43);
const sessionId = 'b'.repeat(43);

async function connect(call: ReturnType<typeof fixture>['call'], sessions: MemoryStore) {
  const start = await call(`/auth/start?requestId=${requestId}`);
  expect(start.status).toBe(302);
  const authorize = new URL(start.headers.get('location')!);
  expect(authorize.origin).toBe('https://github.com');
  expect(authorize.searchParams.get('scope')).toBe('repo');
  const state = authorize.searchParams.get('state')!;
  expect(sessions.values.get(`state:${state}`)).toBe(requestId);
  return state;
}

describe('Yield issue gateway', () => {
  it('fails closed when deployment config is incomplete', async () => {
    const { call, env } = fixture();
    env.GITHUB_REPO = '';
    const response = await call(`/auth/start?requestId=${requestId}`);
    expect(response.status).toBe(503);
    expect(await response.text()).not.toContain('private-secret-never-show');
  });

  it('requires an unguessable request ID and emits no token in the auth page', async () => {
    let exchanges = 0;
    const { call, sessions } = fixture(async () => {
      exchanges += 1;
      return Response.json({ access_token: 'github-private-token' });
    });
    expect((await call('/auth/start?requestId=short')).status).toBe(400);
    const state = await connect(call, sessions);
    const callback = await call(`/auth/callback?code=one-use-code&state=${state}`);
    expect(callback.status).toBe(200);
    const page = await callback.text();
    expect(page).not.toContain('github-private-token');
    expect(page).not.toContain('private-secret-never-show');
    expect(page).not.toContain('sess:');
    expect((await call(`/auth/callback?code=one-use-code&state=${state}`)).status).toBe(400);
    expect(exchanges).toBe(1);
    const poll = await call(`/auth/poll?requestId=${requestId}`);
    const result = await poll.json() as { ok: boolean; sessionId: string };
    expect(result.ok).toBe(true);
    expect(result.sessionId).toMatch(/^[A-Za-z0-9_-]{32,128}$/);
    expect((await call(`/auth/poll?requestId=${requestId}`)).status).toBe(202);
  });

  it('reports auth exchange failure without reflecting secrets', async () => {
    const { call, sessions } = fixture(async () => Response.json({ error: 'private-secret-never-show' }, { status: 401 }));
    const state = await connect(call, sessions);
    const response = await call(`/auth/callback?code=bad&state=${state}`);
    expect(response.status).toBe(502);
    expect(await response.text()).not.toContain('private-secret-never-show');
    expect((await call(`/auth/poll?requestId=${requestId}`)).status).toBe(202);
  });

  it('reports OAuth denial to the poller and consumes the state once', async () => {
    let exchanges = 0;
    const { call, sessions } = fixture(async () => {
      exchanges += 1;
      throw new Error('no exchange on denial');
    });
    const state = await connect(call, sessions);
    const callback = await call(`/auth/callback?error=access_denied&state=${state}`);
    expect(callback.status).toBe(200);
    expect(await callback.text()).toContain('canceled');
    const poll = await call(`/auth/poll?requestId=${requestId}`);
    expect(poll.status).toBe(403);
    expect(await poll.json()).toEqual({ error: 'GitHub connection canceled' });
    expect((await call(`/auth/poll?requestId=${requestId}`)).status).toBe(202);
    expect((await call(`/auth/callback?error=access_denied&state=${state}`)).status).toBe(400);
    expect(exchanges).toBe(0);
  });

  it('expires OAuth state, the poll handoff, and gateway sessions', async () => {
    const { call, sessions } = fixture(async (input) =>
      String(input).endsWith('/login/oauth/access_token')
        ? Response.json({ access_token: 'github-private-token' })
        : Response.json({ number: 7, html_url: 'https://github.com/PORTALSURFER/yield/issues/7' }));
    const expiredState = await connect(call, sessions);
    sessions.advance(601);
    expect((await call(`/auth/callback?code=late&state=${expiredState}`)).status).toBe(400);

    const state = await connect(call, sessions);
    expect((await call(`/auth/callback?code=valid&state=${state}`)).status).toBe(200);
    const sessionId = sessions.values.get(`poll:${requestId}`)!;
    sessions.advance(301);
    expect((await call(`/auth/poll?requestId=${requestId}`)).status).toBe(202);
    expect((await call('/issue', {
      method: 'POST',
      headers: { authorization: `Bearer ${sessionId}` },
      body: JSON.stringify({ title: 'Valid report' }),
    })).status).toBe(200);
    sessions.advance(7 * 24 * 60 * 60);
    expect((await call('/issue', {
      method: 'POST',
      headers: { authorization: `Bearer ${sessionId}` },
      body: JSON.stringify({ title: 'Valid report' }),
    })).status).toBe(401);
  });

  it('rejects missing, expired, malformed, or oversized issue requests before GitHub', async () => {
    let calls = 0;
    const { call, sessions } = fixture(async () => { calls += 1; throw new Error('unexpected'); });
    expect((await call('/issue', { method: 'POST' })).status).toBe(401);
    expect((await call('/issue', { method: 'POST', headers: { authorization: `Bearer ${sessionId}` } })).status).toBe(401);
    sessions.values.set(`sess:${sessionId}`, 'github-private-token');
    const headers = { authorization: `Bearer ${sessionId}` };
    expect((await call('/issue', { method: 'POST', headers, body: '{' })).status).toBe(400);
    expect((await call('/issue', { method: 'POST', headers, body: JSON.stringify({ title: 'x' }) })).status).toBe(400);
    expect((await call('/issue', { method: 'POST', headers, body: JSON.stringify({ title: 'Valid', body: 'x'.repeat(70_000) }) })).status).toBe(400);
    expect(calls).toBe(0);
  });

  it('submits one issue to the configured repo and validates its result URL', async () => {
    let calls = 0;
    const { call, sessions } = fixture(async (input, init) => {
      calls += 1;
      expect(String(input)).toBe('https://api.github.com/repos/PORTALSURFER/yield/issues');
      expect(init?.method).toBe('POST');
      expect(new Headers(init?.headers).get('authorization')).toBe('Bearer github-private-token');
      return Response.json({ number: 42, html_url: 'https://github.com/PORTALSURFER/yield/issues/42' });
    });
    sessions.values.set(`sess:${sessionId}`, 'github-private-token');
    const response = await call('/issue', {
      method: 'POST',
      headers: { authorization: `Bearer ${sessionId}` },
      body: JSON.stringify({ title: '  Bug: sample  ', body: 'Details' }),
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ok: true, issue_url: 'https://github.com/PORTALSURFER/yield/issues/42', number: 42 });
    expect(calls).toBe(1);
  });

  it('does not retry an uncertain GitHub POST or expose token and server text', async () => {
    let calls = 0;
    const { call, sessions } = fixture(async () => {
      calls += 1;
      throw new Error('Bearer github-private-token private-secret-never-show');
    });
    sessions.values.set(`sess:${sessionId}`, 'github-private-token');
    const response = await call('/issue', {
      method: 'POST',
      headers: { authorization: `Bearer ${sessionId}` },
      body: JSON.stringify({ title: 'Bug: uncertain' }),
    });
    expect(response.status).toBe(502);
    const text = await response.text();
    expect(text).toContain('outcome unknown');
    expect(text).not.toContain('github-private-token');
    expect(text).not.toContain('private-secret-never-show');
    expect(calls).toBe(1);
  });

  it('uses deterministic errors for GitHub authorization and input rejection', async () => {
    const { call, sessions } = fixture(async () => Response.json({ error: 'github-private-token' }, { status: 422 }));
    sessions.values.set(`sess:${sessionId}`, 'github-private-token');
    const response = await call('/issue', {
      method: 'POST',
      headers: { authorization: `Bearer ${sessionId}` },
      body: JSON.stringify({ title: 'Bug: rejected' }),
    });
    expect(response.status).toBe(422);
    expect(await response.text()).not.toContain('github-private-token');
  });
});
