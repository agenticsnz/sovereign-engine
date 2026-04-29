import type {
  AuthUser,
  ProvidersResponse,
  UsageResponse,
  UserToken,
  MintedToken,
  MintTokenRequest,
  AdminUsageResponse,
  IdP,
  IdPCreateRequest,
  Category,
  CategoryCreateRequest,
  AdminModel,
  RuntimeOverrides,
  AdminUser,
  SystemInfo,
  OpenAIModel,
  ContainerStartRequest,
  HfSearchResult,
  HfDownload,
  HfDownloadRequest,
  HfRepoFile,
  DiskUsage,
  VramEstimate,
  Reservation,
  ReservationWithUser,
  CreateReservationRequest,
  CrashHistoryRow,
} from './types';

export class ApiError extends Error {
  /**
   * Parsed JSON body of the error response, if it was valid JSON. Callers
   * that care about structured error payloads (e.g. the `blocking_tokens`
   * list returned from `DELETE /api/admin/models/:id` on 409) can read it
   * here instead of re-fetching or re-parsing.
   */
  public data: unknown;

  constructor(public status: number, message: string, data?: unknown) {
    super(message);
    this.name = 'ApiError';
    this.data = data;
  }
}

let onUnauthorized: (() => void) | null = null;

export function setOnUnauthorized(handler: () => void): void {
  onUnauthorized = handler;
}

async function request<T>(url: string, options?: RequestInit): Promise<T> {
  const res = await fetch(url, {
    ...options,
    headers: {
      'Content-Type': 'application/json',
      ...options?.headers,
    },
  });

  if (res.status === 401) {
    if (onUnauthorized) onUnauthorized();
    throw new ApiError(401, 'Unauthorized');
  }

  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    let data: unknown = undefined;
    try {
      data = await res.json();
      const body = data as { error?: string | { message?: string } };
      if (typeof body.error === 'string') {
        message = body.error;
      } else if (body.error && typeof body.error === 'object' && body.error.message) {
        message = body.error.message;
      }
    } catch {
      // body wasn't JSON, use status text
      message = res.statusText || message;
    }
    throw new ApiError(res.status, message, data);
  }

  // Handle 204 No Content
  if (res.status === 204) {
    return undefined as T;
  }

  return res.json() as Promise<T>;
}

// ---- Auth ----

export async function getMe(): Promise<AuthUser> {
  return request<AuthUser>('/auth/me');
}

export async function getProviders(): Promise<ProvidersResponse> {
  return request<ProvidersResponse>('/auth/providers');
}

export async function bootstrapLogin(user: string, pass: string): Promise<void> {
  await request<void>('/auth/bootstrap-login', {
    method: 'POST',
    body: JSON.stringify({ user, pass }),
  });
}

export async function logout(): Promise<void> {
  await request<{ status: string }>('/auth/logout', { method: 'POST' });
}

// ---- User: Usage ----

export async function getUserUsage(period: string = 'day'): Promise<UsageResponse> {
  const param = encodeURIComponent(period);
  const [stats, timeline] = await Promise.all([
    request<{ summary: UsageResponse['summary']; by_model: UsageResponse['by_model']; by_token: UsageResponse['by_token'] }>(
      `/api/user/usage?period=${param}`,
    ),
    request<{ timeline: UsageResponse['timeline']; timeline_by_token: UsageResponse['timeline_by_token'] }>(
      `/api/user/usage/timeline?period=${param}`,
    ),
  ]);
  return { ...stats, timeline: timeline.timeline, timeline_by_token: timeline.timeline_by_token };
}

// ---- User: Tokens ----

export async function getUserTokens(): Promise<UserToken[]> {
  const data = await request<{ tokens: UserToken[] }>('/api/user/tokens');
  return data.tokens;
}

export async function mintToken(req: MintTokenRequest): Promise<MintedToken> {
  return request<MintedToken>('/api/user/tokens', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function revokeToken(id: string): Promise<void> {
  await request<{ status: string }>(`/api/user/tokens/${encodeURIComponent(id)}/revoke`, {
    method: 'POST',
  });
}

export async function deleteToken(id: string): Promise<void> {
  await request<{ status: string }>(`/api/user/tokens/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---- User: Models ----

export async function getUserModels(): Promise<AdminModel[]> {
  const data = await request<{ models: AdminModel[] }>('/api/user/models');
  return data.models;
}

// ---- User: Disk ----

export async function getDiskUsage(): Promise<DiskUsage> {
  return request<DiskUsage>('/api/user/disk');
}

// ---- User: HuggingFace ----

export async function getHfRepoFiles(repo: string): Promise<HfRepoFile[]> {
  const data = await request<{ files: HfRepoFile[] }>(`/api/user/hf/files?repo=${encodeURIComponent(repo)}`);
  return data.files;
}

export async function searchHfModels(
  query: string,
  task?: string,
  tags?: string,
  offset?: number,
  limit?: number,
): Promise<{ models: HfSearchResult[]; has_more: boolean }> {
  const params = new URLSearchParams({ q: query });
  if (task && task !== 'any') params.set('task', task);
  if (tags) params.set('tags', tags);
  if (offset) params.set('offset', String(offset));
  if (limit) params.set('limit', String(limit));
  return request<{ models: HfSearchResult[]; has_more: boolean }>(`/api/user/hf/search?${params}`);
}

export async function startHfDownload(req: HfDownloadRequest): Promise<{ download_id: string }> {
  return request<{ download_id: string }>('/api/user/hf/download', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function getHfDownloads(): Promise<HfDownload[]> {
  const data = await request<{ downloads: HfDownload[] }>('/api/user/hf/downloads');
  return data.downloads;
}

export async function cancelHfDownload(id: string): Promise<void> {
  await request<{ status: string }>(`/api/user/hf/downloads/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---- Admin: Usage Analytics ----

export async function getAdminUsage(period: string = 'day'): Promise<AdminUsageResponse> {
  const param = encodeURIComponent(period);
  const [stats, timeline] = await Promise.all([
    request<{ summary: AdminUsageResponse['summary']; by_user: AdminUsageResponse['by_user'] }>(
      `/api/admin/usage?period=${param}`,
    ),
    request<{ timeline: AdminUsageResponse['timeline'] }>(
      `/api/admin/usage/timeline?period=${param}`,
    ),
  ]);
  return { ...stats, timeline: timeline.timeline };
}

// ---- Admin: IdPs ----

export async function getIdps(): Promise<IdP[]> {
  const data = await request<{ idps: IdP[] }>('/api/admin/idps');
  return data.idps;
}

export async function createIdp(req: IdPCreateRequest): Promise<{ id: string; name: string }> {
  return request<{ id: string; name: string }>('/api/admin/idps', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function updateIdp(id: string, req: Partial<IdPCreateRequest> & { enabled?: boolean }): Promise<void> {
  await request<{ status: string }>(`/api/admin/idps/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(req),
  });
}

export async function deleteIdp(id: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/idps/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---- Admin: Categories ----

export async function getCategories(): Promise<Category[]> {
  const data = await request<{ categories: Category[] }>('/api/admin/categories');
  return data.categories;
}

export async function createCategory(req: CategoryCreateRequest): Promise<{ id: string; name: string }> {
  return request<{ id: string; name: string }>('/api/admin/categories', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function updateCategory(id: string, req: Partial<CategoryCreateRequest>): Promise<void> {
  await request<{ status: string }>(`/api/admin/categories/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(req),
  });
}

export async function deleteCategory(id: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/categories/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---- Admin: Models ----

export async function getAdminModels(): Promise<AdminModel[]> {
  const data = await request<{ models: AdminModel[] }>('/api/admin/models');
  return data.models;
}

export async function updateModel(
  id: string,
  req: {
    category_id?: string | null;
    backend_type?: string;
    runtime_overrides?: RuntimeOverrides;
  },
): Promise<void> {
  await request<{ status: string }>(`/api/admin/models/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(req),
  });
}

/** One row of the `blocking_tokens` array returned with a 409. */
export interface BlockingToken {
  id: string;
  name: string;
  user_email: string | null;
}

export interface DeleteModelResult {
  status: string;
  revoked_tokens: number;
}

/**
 * Delete a model. When `override` is true, any active tokens pinned to this
 * model are force-revoked (soft-deleted) as part of the delete. Without
 * override, the server returns 409 and this call throws `ApiError` with
 * `data.blocking_tokens` populated — callers should catch that case and
 * prompt the admin before retrying with `{ override: true }`.
 */
export async function deleteModel(
  id: string,
  opts: { override?: boolean } = {},
): Promise<DeleteModelResult> {
  const qs = opts.override ? '?override=true' : '';
  return request<DeleteModelResult>(
    `/api/admin/models/${encodeURIComponent(id)}${qs}`,
    { method: 'DELETE' },
  );
}

// ---- Admin: Users ----

export async function getAdminUsers(): Promise<AdminUser[]> {
  const data = await request<{ users: AdminUser[] }>('/api/admin/users');
  return data.users;
}

export async function updateUser(id: string, req: { is_admin: boolean }): Promise<void> {
  await request<{ status: string }>(`/api/admin/users/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify(req),
  });
}

// ---- Admin: System ----

export async function getSystemInfo(): Promise<SystemInfo> {
  return request<SystemInfo>('/api/admin/system');
}

export async function startContainer(req: ContainerStartRequest): Promise<{ container: string; url: string }> {
  return request<{ container: string; url: string }>('/api/admin/containers/start', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function estimateVram(modelId: string, parallel: number): Promise<VramEstimate> {
  return request<VramEstimate>('/api/admin/containers/estimate', {
    method: 'POST',
    body: JSON.stringify({ model_id: modelId, parallel }),
  });
}

export async function stopContainer(modelId: string): Promise<void> {
  await request<{ status: string }>('/api/admin/containers/stop', {
    method: 'POST',
    body: JSON.stringify({ model_id: modelId }),
  });
}

// ---- Admin: backend crash diagnostics (Phase 7) ----

export async function getCrashHistory(modelId: string): Promise<CrashHistoryRow[]> {
  const data = await request<{ history: CrashHistoryRow[] }>(
    `/api/admin/models/${encodeURIComponent(modelId)}/crash_history`,
  );
  return data.history;
}

/**
 * URL for the captured crash-log tail. Use as an `<a target="_blank">` href —
 * the response is `text/plain`.
 */
export function crashLogUrl(modelId: string, occurredAt: string): string {
  return `/api/admin/models/${encodeURIComponent(modelId)}/crash_log/${encodeURIComponent(occurredAt)}`;
}

// ---- User: Reservations ----

export async function createReservation(req: CreateReservationRequest): Promise<{ id: string; status: string }> {
  return request<{ id: string; status: string }>('/api/user/reservations', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function getUserReservations(): Promise<Reservation[]> {
  const data = await request<{ reservations: Reservation[] }>('/api/user/reservations');
  return data.reservations;
}

export async function cancelReservation(id: string): Promise<void> {
  await request<{ status: string }>(`/api/user/reservations/${encodeURIComponent(id)}/cancel`, {
    method: 'POST',
  });
}

export async function getActiveReservation(): Promise<{ active: boolean; reservation_id?: string; user_id?: string; user_display_name?: string | null; end_time?: string }> {
  return request('/api/user/reservations/active');
}

export async function getCalendarReservations(): Promise<ReservationWithUser[]> {
  const data = await request<{ reservations: ReservationWithUser[] }>('/api/user/reservations/calendar');
  return data.reservations;
}

export async function reservationStartContainer(req: ContainerStartRequest): Promise<{ container: string; url: string }> {
  return request<{ container: string; url: string }>('/api/user/reservations/containers/start', {
    method: 'POST',
    body: JSON.stringify(req),
  });
}

export async function reservationStopContainer(modelId: string): Promise<void> {
  await request<{ status: string }>('/api/user/reservations/containers/stop', {
    method: 'POST',
    body: JSON.stringify({ model_id: modelId }),
  });
}

// ---- Admin: Reservations ----

export async function getAdminReservations(): Promise<ReservationWithUser[]> {
  const data = await request<{ reservations: ReservationWithUser[] }>('/api/admin/reservations');
  return data.reservations;
}

export async function approveReservation(id: string, note?: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/reservations/${encodeURIComponent(id)}/approve`, {
    method: 'POST',
    body: JSON.stringify({ note }),
  });
}

export async function rejectReservation(id: string, note?: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/reservations/${encodeURIComponent(id)}/reject`, {
    method: 'POST',
    body: JSON.stringify({ note }),
  });
}

export async function activateReservation(id: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/reservations/${encodeURIComponent(id)}/activate`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

export async function deactivateReservation(id: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/reservations/${encodeURIComponent(id)}/deactivate`, {
    method: 'POST',
    body: JSON.stringify({}),
  });
}

export async function deleteReservation(id: string): Promise<void> {
  await request<{ status: string }>(`/api/admin/reservations/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ---- OpenAI-compatible: Models ----

export async function getLoadedModels(): Promise<OpenAIModel[]> {
  const data = await request<{ object: string; data: OpenAIModel[] }>('/v1/models');
  return data.data;
}
