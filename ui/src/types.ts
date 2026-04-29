// ---- Auth ----

export interface AuthUser {
  user_id: string;
  email: string | null;
  display_name: string | null;
  is_admin: boolean;
  chat_url: string;
}

export interface AuthProvider {
  id: string;
  name: string;
}

export interface ProvidersResponse {
  providers: AuthProvider[];
  bootstrap_active: boolean;
}

// ---- User Tokens ----

export interface UserToken {
  id: string;
  name: string;
  category_id: string | null;
  category_name: string | null;
  specific_model_id: string | null;
  expires_at: string | null;
  revoked: boolean;
  created_at: string;
}

export interface MintedToken {
  token: string;
  name: string;
  warning: string;
}

export interface MintTokenRequest {
  name: string;
  category_id: string | null;
  specific_model_id: string | null;
  expires_in_days: number | null;
}

// ---- Usage ----

export interface UsageSummary {
  total_requests: number;
  total_input_tokens: number;
  total_output_tokens: number;
  period: string;
}

export interface UsageByModel {
  model_id: string;
  category_name: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface UsageTimelinePoint {
  timestamp: string;
  model: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface UsageByToken {
  token_name: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface UsageTokenTimelinePoint {
  timestamp: string;
  token_name: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface UsageResponse {
  summary: UsageSummary;
  by_model: UsageByModel[];
  by_token: UsageByToken[];
  timeline: UsageTimelinePoint[];
  timeline_by_token: UsageTokenTimelinePoint[];
}

// ---- Admin: Usage Analytics ----

export interface AdminUsageByUser {
  user_label: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface AdminUsageTimelinePoint {
  timestamp: string;
  user_label: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
}

export interface AdminUsageResponse {
  summary: UsageSummary;
  by_user: AdminUsageByUser[];
  timeline: AdminUsageTimelinePoint[];
}

// ---- Admin: IdPs ----

export interface IdP {
  id: string;
  name: string;
  issuer: string;
  client_id: string;
  scopes: string;
  enabled: boolean;
  created_at: string;
}

export interface IdPCreateRequest {
  name: string;
  issuer: string;
  client_id: string;
  client_secret: string;
  scopes: string;
}

// ---- Admin: Categories ----

export interface Category {
  id: string;
  name: string;
  description: string;
  preferred_model_id: string | null;
  created_at: string;
}

export interface CategoryCreateRequest {
  name: string;
  description: string;
  preferred_model_id: string | null;
}

// ---- Admin: Models ----

/**
 * Per-model overrides for the llama.cpp CLI args used at container start.
 *
 * All fields are optional. An omitted field means "use llama.cpp default".
 * An empty object `{}` is also valid and means "use defaults for everything".
 */
export interface RuntimeOverrides {
  /** `--cache-ram <MiB>`. Set to 0 to disable the prompt LRU cache. */
  cache_ram_mib?: number;
  /** `--swa-full`. Allocate sliding-window attention cache at full size. */
  swa_full?: boolean;
  /** `--ctx-checkpoints <N>`. Per-slot KV snapshots. */
  ctx_checkpoints?: number;
  /** `--cache-reuse <N>`. Min token chunk for KV-shifting cross-request reuse. */
  cache_reuse?: number;
  /** Raw additional flags. Each element is one CLI token. */
  extra?: string[];
}

export interface AdminModel {
  id: string;
  hf_repo: string;
  filename: string | null;
  /**
   * Filename of the multimodal projector (mmproj) GGUF that pairs with this
   * model, if any. Non-null means the model was downloaded with a vision
   * projector and is launched with `--mmproj` by the llama.cpp backend.
   */
  mmproj_filename: string | null;
  size_bytes: number;
  category_id: string | null;
  loaded: boolean;
  backend_port: number | null;
  backend_type: string;
  last_used_at: string | null;
  created_at: string;
  context_length: number | null;
  n_layers: number | null;
  n_heads: number | null;
  n_kv_heads: number | null;
  embedding_length: number | null;
  runtime_overrides: RuntimeOverrides | null;
  /**
   * Phase 6/7 quarantine: ISO-8601 timestamp set when the supervisor flagged
   * this model as no-auto-restart. `null` means not quarantined. Even when
   * the container is gone (loaded=0), this column persists so the admin UI
   * can still show the badge and prompt the operator to manually restart.
   */
  quarantined_at?: string | null;
  quarantine_reason?: string | null;
}

// ---- Admin: Users ----

export interface AdminUser {
  id: string;
  idp_id: string;
  email: string | null;
  display_name: string | null;
  is_admin: boolean;
  created_at: string;
  usage_summary: {
    total_requests: number;
    total_tokens: number;
  };
}

// ---- Reservations ----

export type ReservationStatus = 'pending' | 'approved' | 'active' | 'completed' | 'rejected' | 'cancelled';

export interface Reservation {
  id: string;
  user_id: string;
  status: ReservationStatus;
  start_time: string;
  end_time: string;
  reason: string;
  admin_note: string;
  approved_by: string | null;
  created_at: string;
  updated_at: string;
}

export interface ReservationWithUser extends Reservation {
  user_email: string | null;
  user_display_name: string | null;
}

export interface CreateReservationRequest {
  start_time: string;
  end_time: string;
  reason?: string;
}

export interface ActiveReservationInfo {
  reservation_id: string;
  user_id: string;
  user_display_name: string | null;
  end_time: string;
}

// ---- SSE Metrics Snapshot ----

export interface GateSnapshot {
  max_slots: number;
  in_flight: number;
}

export interface MetricsSnapshot {
  gpu_memory: GpuMemory[];
  cpu: CpuInfo | null;
  containers: SystemContainer[];
  queues: Record<string, { depth: number; avg_wait_ms: number }>;
  gates: Record<string, GateSnapshot>;
  disk: { total_bytes: number; used_bytes: number; free_bytes: number } | null;
  active_reservation: ActiveReservationInfo | null;
  timestamp: string;
}

export interface CpuInfo {
  utilization_percent: number;
  num_cores: number;
}

// ---- Admin: System ----

export interface GpuMemory {
  gpu_type: string;
  device_index: number;
  total_mb: number;
  used_mb: number;
  free_mb: number;
  utilization_percent: number | null;
}

export interface VramEstimate {
  model_weights_mb: number;
  kv_cache_mb: number;
  overhead_mb: number;
  total_mb: number;
  gpu_total_mb: number;
  gpu_used_mb: number;
  gpu_free_mb: number;
  fits: boolean;
}

export interface SystemInfo {
  disk: {
    model_path: string;
    total_bytes: number;
    used_bytes: number;
    free_bytes: number;
  };
  queues: Record<string, { depth: number; avg_wait_ms: number }>;
  gates: Record<string, GateSnapshot>;
  containers: SystemContainer[];
  gpu: string[];
  gpu_memory: GpuMemory[];
  available_backends: string[];
}

export interface SystemContainer {
  model_id: string;
  backend_type: string;
  healthy: boolean;
  state: string;
  vram_used_mb: number | null;
  // Phase 7: supervisor + crash diagnostics surfaced by the backend.
  fsm_state?: 'Starting' | 'Healthy' | 'Suspect' | 'Crashed' | 'Quarantined';
  quarantined?: boolean;
  quarantine_reason?: string | null;
  last_crash?: LastCrashSummary | null;
}

export interface LastCrashSummary {
  occurred_at: string;
  exit_code: number | null;
  oom_killed: boolean;
  log_path_present: boolean;
}

export interface CrashHistoryRow {
  occurred_at: string;
  container_id: string | null;
  exit_code: number | null;
  oom_killed: number; // 0 or 1, raw from sqlite
  signal: string | null;
  log_path_present: boolean;
}

// ---- Admin: Containers ----

export interface Container {
  id: string;
  names: string[];
  model_id: string;
  state: string;
  status: string;
  port: number;
}

export interface ContainerStartRequest {
  model_id: string;
  backend_type?: string;
  gpu_type?: string;
  gpu_layers?: number;
  parallel?: number;
}

// ---- OpenAI-compatible Models ----

export interface OpenAIModel {
  id: string;
  object: string;
  owned_by: string;
}

// ---- HuggingFace ----

export interface HfSearchResult {
  id: string;
  downloads: number;
  likes: number;
  pipeline_tag: string | null;
  tags: string[];
}

export interface HfDownload {
  id: string;
  hf_repo: string;
  progress_bytes: number;
  total_bytes: number;
  status: 'downloading' | 'complete' | 'failed' | 'cancelled';
  error: string | null;
}

export interface HfDownloadRequest {
  hf_repo: string;
  files?: string[];
  category_id?: string;
  backend_type?: string;
}

export interface HfRepoFile {
  path: string;
  size: number;
}

// ---- Disk Usage ----

export interface DiskUsage {
  total_bytes: number;
  used_bytes: number;
  free_bytes: number;
}

// ---- API Error ----

export interface ApiError {
  error: string | { message: string; type: string; code: string };
}
