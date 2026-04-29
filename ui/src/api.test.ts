import { vi, describe, it, expect, beforeEach } from 'vitest';
import {
  getMe,
  getProviders,
  bootstrapLogin,
  logout,
  getUserUsage,
  getUserTokens,
  mintToken,
  revokeToken,
  deleteToken,
  getUserModels,
  getDiskUsage,
  getHfRepoFiles,
  searchHfModels,
  startHfDownload,
  cancelHfDownload,
  getIdps,
  createIdp,
  updateIdp,
  deleteIdp,
  getCategories,
  createCategory,
  updateCategory,
  deleteCategory,
  getAdminModels,
  updateModel,
  deleteModel,
  getAdminUsers,
  updateUser,
  getSystemInfo,
  startContainer,
  estimateVram,
  stopContainer,
  getLoadedModels,
  createReservation,
  getUserReservations,
  cancelReservation,
  approveReservation,
  deleteReservation,
  setOnUnauthorized,
} from './api';

// ---- Global fetch mock ----

const mockFetch = vi.fn();
globalThis.fetch = mockFetch;

beforeEach(() => {
  mockFetch.mockReset();
});

// ---- Helpers ----

/** Build a successful Response-like object that `request()` will accept. */
function okResponse(body: unknown, status = 200) {
  return {
    ok: true,
    status,
    json: () => Promise.resolve(body),
  };
}

/** Build a 204 No Content response. */
function noContentResponse() {
  return {
    ok: true,
    status: 204,
    json: () => Promise.reject(new Error('no body')),
  };
}

/** Build a failed response with a JSON error body. */
function errorResponse(status: number, errorBody: unknown) {
  return {
    ok: false,
    status,
    statusText: `Error ${status}`,
    json: () => Promise.resolve(errorBody),
  };
}

/** Build a failed response whose body is not JSON. */
function errorResponseNonJson(status: number, statusText = 'Internal Server Error') {
  return {
    ok: false,
    status,
    statusText,
    json: () => Promise.reject(new Error('not JSON')),
  };
}

// ==========================================================================
// request() shared behaviour - tested via concrete API functions
// ==========================================================================

describe('request() shared behaviour', () => {
  it('always sends Content-Type application/json', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ user_id: 'u1', email: null, display_name: null, is_admin: false }));
    await getMe();

    expect(mockFetch).toHaveBeenCalledWith('/auth/me', expect.objectContaining({
      headers: expect.objectContaining({ 'Content-Type': 'application/json' }),
    }));
  });

  it('throws ApiError with string error from JSON body', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(400, { error: 'bad input' }));
    await expect(getMe()).rejects.toThrow('bad input');
  });

  it('throws ApiError with nested error.message from JSON body', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(422, { error: { message: 'validation failed', type: 'invalid', code: '422' } }));
    await expect(getMe()).rejects.toThrow('validation failed');
  });

  it('falls back to statusText when response body is not JSON', async () => {
    mockFetch.mockResolvedValueOnce(errorResponseNonJson(500, 'Gateway Timeout'));
    await expect(getMe()).rejects.toThrow('Gateway Timeout');
  });

  it('calls onUnauthorized handler and throws on 401', async () => {
    const handler = vi.fn();
    setOnUnauthorized(handler);

    mockFetch.mockResolvedValueOnce({
      ok: false,
      status: 401,
      statusText: 'Unauthorized',
      json: () => Promise.resolve({}),
    });

    await expect(getMe()).rejects.toThrow('Unauthorized');
    expect(handler).toHaveBeenCalledOnce();

    // Clean up: reset the handler so it doesn't leak into other tests
    setOnUnauthorized(() => {});
  });

  it('returns undefined for 204 No Content', async () => {
    mockFetch.mockResolvedValueOnce(noContentResponse());
    // revokeToken calls request and discards the result (await on void)
    // We test via logout which also returns void but the underlying request
    // returns { status: string }. Let's use deleteIdp instead.
    // Actually, the 204 case is in the shared request(). We can trigger it
    // by calling any void function where the server returns 204.
    // For a clean test, just call a simple GET that happens to return 204:
    mockFetch.mockReset();
    mockFetch.mockResolvedValueOnce(noContentResponse());
    const result = await getDiskUsage();
    expect(result).toBeUndefined();
  });
});

// ==========================================================================
// GET functions - simple wrappers
// ==========================================================================

describe('getMe()', () => {
  it('returns the auth user from /auth/me', async () => {
    const user = { user_id: 'u1', email: 'a@b.com', display_name: 'Alice', is_admin: true };
    mockFetch.mockResolvedValueOnce(okResponse(user));

    const result = await getMe();

    expect(result).toEqual(user);
    expect(mockFetch).toHaveBeenCalledWith('/auth/me', expect.objectContaining({
      headers: { 'Content-Type': 'application/json' },
    }));
  });
});

describe('getProviders()', () => {
  it('returns the full ProvidersResponse including bootstrap_active', async () => {
    const providers = [{ id: 'p1', name: 'Google' }];
    mockFetch.mockResolvedValueOnce(okResponse({ providers, bootstrap_active: true }));

    const result = await getProviders();

    expect(result).toEqual({ providers, bootstrap_active: true });
    expect(mockFetch).toHaveBeenCalledWith('/auth/providers', expect.anything());
  });
});

describe('bootstrapLogin()', () => {
  it('sends POST /auth/bootstrap-login with user and pass as JSON', async () => {
    mockFetch.mockResolvedValueOnce(noContentResponse());

    await bootstrapLogin('admin', 'secret');

    expect(mockFetch).toHaveBeenCalledWith(
      '/auth/bootstrap-login',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({ user: 'admin', pass: 'secret' }),
      }),
    );
  });

  it('throws ApiError with status 401 on wrong credentials', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(401, { error: 'Unauthorized' }));

    await expect(bootstrapLogin('admin', 'wrong')).rejects.toMatchObject({ status: 401 });
  });

  it('throws ApiError with status 429 on rate limit', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(429, { error: 'Too Many Requests' }));

    await expect(bootstrapLogin('admin', 'pass')).rejects.toMatchObject({ status: 429 });
  });
});

describe('getUserTokens()', () => {
  it('unwraps the tokens array', async () => {
    const tokens = [{ id: 't1', name: 'dev', category_id: null, category_name: null, specific_model_id: null, expires_at: null, revoked: false, created_at: '2025-01-01' }];
    mockFetch.mockResolvedValueOnce(okResponse({ tokens }));

    const result = await getUserTokens();

    expect(result).toEqual(tokens);
    expect(mockFetch).toHaveBeenCalledWith('/api/user/tokens', expect.anything());
  });
});

describe('getUserModels()', () => {
  it('unwraps the models array from /api/user/models', async () => {
    const models = [{ id: 'm1', hf_repo: 'repo', filename: null, size_bytes: 100, category_id: null, loaded: false, backend_port: null, backend_type: 'vllm', last_used_at: null, created_at: '2025-01-01', context_length: null, n_layers: null, n_heads: null, n_kv_heads: null, embedding_length: null, runtime_overrides: null }];
    mockFetch.mockResolvedValueOnce(okResponse({ models }));

    const result = await getUserModels();

    expect(result).toEqual(models);
  });
});

describe('getDiskUsage()', () => {
  it('returns disk usage directly (no unwrapping)', async () => {
    const disk = { total_bytes: 1000, used_bytes: 500, free_bytes: 500 };
    mockFetch.mockResolvedValueOnce(okResponse(disk));

    const result = await getDiskUsage();

    expect(result).toEqual(disk);
    expect(mockFetch).toHaveBeenCalledWith('/api/user/disk', expect.anything());
  });
});

describe('getLoadedModels()', () => {
  it('unwraps the data array from OpenAI-compatible response', async () => {
    const models = [{ id: 'llama', object: 'model', owned_by: 'local' }];
    mockFetch.mockResolvedValueOnce(okResponse({ object: 'list', data: models }));

    const result = await getLoadedModels();

    expect(result).toEqual(models);
    expect(mockFetch).toHaveBeenCalledWith('/v1/models', expect.anything());
  });
});

describe('getIdps()', () => {
  it('unwraps the idps array', async () => {
    const idps = [{ id: 'i1', name: 'Okta', issuer: 'https://okta.example.com', client_id: 'cid', scopes: 'openid', enabled: true, created_at: '2025-01-01' }];
    mockFetch.mockResolvedValueOnce(okResponse({ idps }));

    const result = await getIdps();

    expect(result).toEqual(idps);
  });
});

describe('getCategories()', () => {
  it('unwraps the categories array', async () => {
    const categories = [{ id: 'c1', name: 'Code', description: 'Coding models', preferred_model_id: null, created_at: '2025-01-01' }];
    mockFetch.mockResolvedValueOnce(okResponse({ categories }));

    const result = await getCategories();

    expect(result).toEqual(categories);
  });
});

describe('getSystemInfo()', () => {
  it('returns system info directly', async () => {
    const info = { disk: { model_path: '/models', total_bytes: 1000, used_bytes: 500, free_bytes: 500 }, queues: {}, gates: {}, containers: [], gpu: [], gpu_memory: [], available_backends: ['vllm'] };
    mockFetch.mockResolvedValueOnce(okResponse(info));

    const result = await getSystemInfo();

    expect(result).toEqual(info);
    expect(mockFetch).toHaveBeenCalledWith('/api/admin/system', expect.anything());
  });
});

// ==========================================================================
// GET with query parameters
// ==========================================================================

describe('getHfRepoFiles()', () => {
  it('encodes the repo parameter and unwraps files', async () => {
    const files = [{ path: 'model.bin', size: 4000 }];
    mockFetch.mockResolvedValueOnce(okResponse({ files }));

    const result = await getHfRepoFiles('org/my-model');

    expect(result).toEqual(files);
    expect(mockFetch).toHaveBeenCalledWith(
      '/api/user/hf/files?repo=org%2Fmy-model',
      expect.anything(),
    );
  });
});

describe('searchHfModels()', () => {
  it('builds URL params with all optional fields', async () => {
    const body = { models: [{ id: 'hf/llama', downloads: 100, likes: 50, pipeline_tag: 'text-generation', tags: [] }], has_more: false };
    mockFetch.mockResolvedValueOnce(okResponse(body));

    const result = await searchHfModels('llama', 'text-generation', 'gguf', 10, 25);

    expect(result).toEqual(body);
    const calledUrl = mockFetch.mock.calls[0][0] as string;
    expect(calledUrl).toContain('/api/user/hf/search?');
    expect(calledUrl).toContain('q=llama');
    expect(calledUrl).toContain('task=text-generation');
    expect(calledUrl).toContain('tags=gguf');
    expect(calledUrl).toContain('offset=10');
    expect(calledUrl).toContain('limit=25');
  });

  it('omits optional params when not provided', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ models: [], has_more: false }));

    await searchHfModels('test');

    const calledUrl = mockFetch.mock.calls[0][0] as string;
    expect(calledUrl).toContain('q=test');
    expect(calledUrl).not.toContain('task=');
    expect(calledUrl).not.toContain('tags=');
    expect(calledUrl).not.toContain('offset=');
    expect(calledUrl).not.toContain('limit=');
  });

  it('omits task param when set to "any"', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ models: [], has_more: false }));

    await searchHfModels('gemma', 'any');

    const calledUrl = mockFetch.mock.calls[0][0] as string;
    expect(calledUrl).toContain('q=gemma');
    expect(calledUrl).not.toContain('task=');
  });
});

// ==========================================================================
// getUserUsage() - Promise.all pattern
// ==========================================================================

describe('getUserUsage()', () => {
  it('merges stats and timeline from two parallel requests', async () => {
    const summary = { total_requests: 10, total_input_tokens: 100, total_output_tokens: 50, period: 'day' };
    const by_model = [{ model_id: 'm1', category_name: 'Code', requests: 5, input_tokens: 50, output_tokens: 25 }];
    const by_token = [{ token_name: 'dev', requests: 5, input_tokens: 50, output_tokens: 25 }];
    const timeline = [{ timestamp: '2025-01-01T00:00:00Z', model: 'm1', requests: 5, input_tokens: 50, output_tokens: 25 }];
    const timeline_by_token = [{ timestamp: '2025-01-01T00:00:00Z', token_name: 'dev', requests: 5, input_tokens: 50, output_tokens: 25 }];

    // Two parallel fetches: stats then timeline
    mockFetch
      .mockResolvedValueOnce(okResponse({ summary, by_model, by_token }))
      .mockResolvedValueOnce(okResponse({ timeline, timeline_by_token }));

    const result = await getUserUsage('week');

    expect(result).toEqual({ summary, by_model, by_token, timeline, timeline_by_token });
    expect(mockFetch).toHaveBeenCalledTimes(2);

    const urls = mockFetch.mock.calls.map((c: unknown[]) => c[0]);
    expect(urls).toContain('/api/user/usage?period=week');
    expect(urls).toContain('/api/user/usage/timeline?period=week');
  });

  it('defaults period to "day"', async () => {
    mockFetch
      .mockResolvedValueOnce(okResponse({ summary: {}, by_model: [], by_token: [] }))
      .mockResolvedValueOnce(okResponse({ timeline: [], timeline_by_token: [] }));

    await getUserUsage();

    const urls = mockFetch.mock.calls.map((c: unknown[]) => c[0]);
    expect(urls).toContain('/api/user/usage?period=day');
    expect(urls).toContain('/api/user/usage/timeline?period=day');
  });
});

// ==========================================================================
// POST functions
// ==========================================================================

describe('logout()', () => {
  it('sends POST to /auth/logout', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await logout();

    expect(mockFetch).toHaveBeenCalledWith('/auth/logout', expect.objectContaining({
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
    }));
  });
});

describe('mintToken()', () => {
  it('sends POST with request body and returns minted token', async () => {
    const req = { name: 'dev', category_id: null, specific_model_id: null, expires_in_days: 90 };
    const minted = { token: 'sk-abc', name: 'dev', warning: 'save this' };
    mockFetch.mockResolvedValueOnce(okResponse(minted));

    const result = await mintToken(req);

    expect(result).toEqual(minted);
    expect(mockFetch).toHaveBeenCalledWith('/api/user/tokens', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify(req),
    }));
  });
});

describe('revokeToken()', () => {
  it('sends POST with encoded ID in URL', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await revokeToken('tok/123');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/user/tokens/tok%2F123/revoke',
      expect.objectContaining({ method: 'POST' }),
    );
  });
});

describe('createIdp()', () => {
  it('sends POST to /api/admin/idps and returns id+name', async () => {
    const req = { name: 'Google', issuer: 'https://accounts.google.com', client_id: 'cid', client_secret: 'secret', scopes: 'openid email' };
    const created = { id: 'i1', name: 'Google' };
    mockFetch.mockResolvedValueOnce(okResponse(created));

    const result = await createIdp(req);

    expect(result).toEqual(created);
    expect(mockFetch).toHaveBeenCalledWith('/api/admin/idps', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify(req),
    }));
  });
});

describe('startContainer()', () => {
  it('sends POST with container start request', async () => {
    const req = { model_id: 'm1', backend_type: 'vllm', context_size: 4096 };
    const response = { container: 'c1', url: 'http://localhost:8000' };
    mockFetch.mockResolvedValueOnce(okResponse(response));

    const result = await startContainer(req);

    expect(result).toEqual(response);
    expect(mockFetch).toHaveBeenCalledWith('/api/admin/containers/start', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify(req),
    }));
  });
});

describe('estimateVram()', () => {
  it('sends POST with model_id and parallel', async () => {
    const estimate = { model_weights_mb: 4000, kv_cache_mb: 500, overhead_mb: 200, total_mb: 4700, gpu_total_mb: 24000, gpu_used_mb: 0, gpu_free_mb: 24000, fits: true };
    mockFetch.mockResolvedValueOnce(okResponse(estimate));

    const result = await estimateVram('m1', 4);

    expect(result).toEqual(estimate);
    expect(mockFetch).toHaveBeenCalledWith('/api/admin/containers/estimate', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify({ model_id: 'm1', parallel: 4 }),
    }));
  });
});

describe('stopContainer()', () => {
  it('sends POST with model_id in body', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await stopContainer('m1');

    expect(mockFetch).toHaveBeenCalledWith('/api/admin/containers/stop', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify({ model_id: 'm1' }),
    }));
  });
});

describe('createReservation()', () => {
  it('sends POST and returns id+status', async () => {
    const req = { start_time: '2025-06-01T10:00:00Z', end_time: '2025-06-01T12:00:00Z', reason: 'training' };
    const created = { id: 'r1', status: 'pending' };
    mockFetch.mockResolvedValueOnce(okResponse(created));

    const result = await createReservation(req);

    expect(result).toEqual(created);
    expect(mockFetch).toHaveBeenCalledWith('/api/user/reservations', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify(req),
    }));
  });
});

describe('startHfDownload()', () => {
  it('sends POST and returns download_id', async () => {
    const req = { hf_repo: 'org/model', files: ['model.bin'], category_id: 'c1' };
    mockFetch.mockResolvedValueOnce(okResponse({ download_id: 'd1' }));

    const result = await startHfDownload(req);

    expect(result).toEqual({ download_id: 'd1' });
    expect(mockFetch).toHaveBeenCalledWith('/api/user/hf/download', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify(req),
    }));
  });
});

describe('approveReservation()', () => {
  it('sends POST with optional note', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await approveReservation('r1', 'looks good');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/reservations/r1/approve',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({ note: 'looks good' }),
      }),
    );
  });
});

// ==========================================================================
// PUT functions
// ==========================================================================

describe('updateIdp()', () => {
  it('sends PUT with partial update body and encoded ID', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await updateIdp('idp/1', { name: 'Updated', enabled: false });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/idps/idp%2F1',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ name: 'Updated', enabled: false }),
      }),
    );
  });
});

describe('updateCategory()', () => {
  it('sends PUT to /api/admin/categories/:id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await updateCategory('c1', { description: 'new desc' });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/categories/c1',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ description: 'new desc' }),
      }),
    );
  });
});

describe('updateModel()', () => {
  it('sends PUT with category_id only when no overrides are provided', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await updateModel('m1', { category_id: 'c1' });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/models/m1',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ category_id: 'c1' }),
      }),
    );
  });

  it('sends PUT with runtime_overrides payload when provided', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    const overrides = {
      cache_ram_mib: 0,
      swa_full: true,
      ctx_checkpoints: 32,
      cache_reuse: 256,
      extra: ['--threads', '8'],
    };
    await updateModel('m1', { runtime_overrides: overrides });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/models/m1',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ runtime_overrides: overrides }),
      }),
    );
  });

  it('encodes the model id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await updateModel('org/model:latest', { runtime_overrides: {} });

    const url = mockFetch.mock.calls[0][0] as string;
    expect(url).toBe('/api/admin/models/org%2Fmodel%3Alatest');
  });
});

describe('updateUser()', () => {
  it('sends PUT with is_admin flag', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await updateUser('u1', { is_admin: true });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/users/u1',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ is_admin: true }),
      }),
    );
  });
});

// ==========================================================================
// DELETE functions
// ==========================================================================

describe('deleteIdp()', () => {
  it('sends DELETE to /api/admin/idps/:id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await deleteIdp('i1');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/idps/i1',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });
});

describe('deleteCategory()', () => {
  it('sends DELETE to /api/admin/categories/:id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await deleteCategory('c1');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/categories/c1',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });
});

describe('deleteModel()', () => {
  it('sends DELETE with encoded model ID (no override by default)', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'deleted', revoked_tokens: 0 }));

    await deleteModel('org/model:latest');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/models/org%2Fmodel%3Alatest',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });

  it('appends ?override=true when the caller opts in', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'deleted', revoked_tokens: 2 }));

    const result = await deleteModel('m1', { override: true });

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/models/m1?override=true',
      expect.objectContaining({ method: 'DELETE' }),
    );
    expect(result.revoked_tokens).toBe(2);
  });

  it('throws ApiError with blocking_tokens on 409', async () => {
    mockFetch.mockResolvedValueOnce(
      errorResponse(409, {
        error: 'Model is in use by 1 active token(s).',
        blocking_tokens: [
          { id: 't1', name: 'dev-token', user_email: 'dev@example.com' },
        ],
      }),
    );

    await expect(deleteModel('m1')).rejects.toMatchObject({
      status: 409,
      message: 'Model is in use by 1 active token(s).',
      data: {
        blocking_tokens: [
          { id: 't1', name: 'dev-token', user_email: 'dev@example.com' },
        ],
      },
    });
  });
});

describe('cancelHfDownload()', () => {
  it('sends DELETE to /api/user/hf/downloads/:id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await cancelHfDownload('d1');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/user/hf/downloads/d1',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });
});

describe('deleteReservation()', () => {
  it('sends DELETE to /api/admin/reservations/:id', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await deleteReservation('r1');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/admin/reservations/r1',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });
});

describe('deleteToken()', () => {
  it('sends DELETE to /api/user/tokens/:id with encoded ID', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await deleteToken('tok/123');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/user/tokens/tok%2F123',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });
});

// ==========================================================================
// POST-as-action functions (cancel, revoke etc)
// ==========================================================================

describe('cancelReservation()', () => {
  it('sends POST to /api/user/reservations/:id/cancel', async () => {
    mockFetch.mockResolvedValueOnce(okResponse({ status: 'ok' }));

    await cancelReservation('r1');

    expect(mockFetch).toHaveBeenCalledWith(
      '/api/user/reservations/r1/cancel',
      expect.objectContaining({ method: 'POST' }),
    );
  });
});

// ==========================================================================
// GET functions that unwrap nested arrays
// ==========================================================================

describe('getAdminModels()', () => {
  it('unwraps models from /api/admin/models', async () => {
    const models = [{ id: 'm1', hf_repo: 'r', filename: null, size_bytes: 0, category_id: null, loaded: false, backend_port: null, backend_type: 'vllm', last_used_at: null, created_at: '', context_length: null, n_layers: null, n_heads: null, n_kv_heads: null, embedding_length: null, runtime_overrides: { cache_ram_mib: 0, swa_full: true } }];
    mockFetch.mockResolvedValueOnce(okResponse({ models }));

    const result = await getAdminModels();

    expect(result).toEqual(models);
  });
});

describe('getAdminUsers()', () => {
  it('unwraps users from /api/admin/users', async () => {
    const users = [{ id: 'u1', idp_id: 'i1', email: 'a@b.com', display_name: 'Alice', is_admin: false, created_at: '', usage_summary: { total_requests: 0, total_tokens: 0 } }];
    mockFetch.mockResolvedValueOnce(okResponse({ users }));

    const result = await getAdminUsers();

    expect(result).toEqual(users);
  });
});

describe('getUserReservations()', () => {
  it('unwraps reservations from /api/user/reservations', async () => {
    const reservations = [{ id: 'r1', user_id: 'u1', status: 'pending', start_time: '', end_time: '', reason: '', admin_note: '', approved_by: null, created_at: '', updated_at: '' }];
    mockFetch.mockResolvedValueOnce(okResponse({ reservations }));

    const result = await getUserReservations();

    expect(result).toEqual(reservations);
  });
});

// ==========================================================================
// Error paths on different HTTP methods
// ==========================================================================

describe('error handling across methods', () => {
  it('POST: throws on server error', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(500, { error: 'container failed' }));

    await expect(startContainer({ model_id: 'm1' })).rejects.toThrow('container failed');
  });

  it('PUT: throws on server error', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(404, { error: 'not found' }));

    await expect(updateUser('u1', { is_admin: true })).rejects.toThrow('not found');
  });

  it('DELETE: throws on server error', async () => {
    mockFetch.mockResolvedValueOnce(errorResponseNonJson(403, 'Forbidden'));

    await expect(deleteIdp('i1')).rejects.toThrow('Forbidden');
  });

  it('createCategory: throws on 409 conflict', async () => {
    mockFetch.mockResolvedValueOnce(errorResponse(409, { error: 'name already exists' }));

    await expect(createCategory({ name: 'Dup', description: '', preferred_model_id: null })).rejects.toThrow('name already exists');
  });
});
