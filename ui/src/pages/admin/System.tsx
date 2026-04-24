import { useState, useEffect, useCallback, useRef } from 'react';
import { getSystemInfo, getAdminModels, stopContainer, deleteModel, ApiError, type BlockingToken } from '../../api';
import type { SystemInfo, AdminModel, SystemContainer, GpuMemory, CpuInfo, GateSnapshot } from '../../types';
import { useTheme } from '../../theme';
import { useEventStream, type ConnectionStatus } from '../../hooks/useEventStream';
import LoadingSpinner from '../../components/common/LoadingSpinner';
import ErrorAlert from '../../components/common/ErrorAlert';
import ConfirmDialog from '../../components/common/ConfirmDialog';
import StartModelDialog from '../../components/admin/StartModelDialog';

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(1)} ${units[i]}`;
}

function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function LiveIndicator({ status }: Readonly<{ status: ConnectionStatus }>) {
  const { colors } = useTheme();
  const fallbackColor = status === 'connecting' ? colors.warningText : colors.dangerText;
  const color = status === 'connected' ? colors.successText : fallbackColor;
  const fallbackLabel = status === 'connecting' ? 'Connecting...' : 'Disconnected';
  const label = status === 'connected' ? 'Live' : fallbackLabel;

  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: '0.75rem', color }}>
      <span
        style={{
          width: 8,
          height: 8,
          borderRadius: '50%',
          background: color,
          display: 'inline-block',
          animation: status === 'connected' ? 'pulse 2s infinite' : undefined,
        }}
      />
      {label}
      <style>{`@keyframes pulse { 0%, 100% { opacity: 1; } 50% { opacity: 0.4; } }`}</style>
    </span>
  );
}

function percentColor(percent: number, colors: { dangerText: string; warningText: string; successText: string }): string {
  if (percent > 90) return colors.dangerText;
  if (percent > 70) return colors.warningText;
  return colors.successText;
}

export default function System() {
  const { colors } = useTheme();
  const [system, setSystem] = useState<SystemInfo | null>(null);
  const [models, setModels] = useState<AdminModel[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState<string | null>(null);
  const [confirmStop, setConfirmStop] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<AdminModel | null>(null);
  const [overrideDelete, setOverrideDelete] = useState<{
    model: AdminModel;
    blockingTokens: BlockingToken[];
  } | null>(null);
  const [startModel, setStartModel] = useState<AdminModel | null>(null);

  // SSE live metrics
  const { snapshot, status: sseStatus } = useEventStream();

  const cardStyle: React.CSSProperties = {
    background: colors.cardBg,
    border: `1px solid ${colors.cardBorder}`,
    borderRadius: 8,
    padding: '1.25rem',
  };

  const fetchData = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [sysInfo, modelList] = await Promise.all([getSystemInfo(), getAdminModels()]);
      setSystem(sysInfo);
      setModels(modelList);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load system info');
    } finally {
      setLoading(false);
    }
  }, []);

  const refreshModels = useCallback(async () => {
    setError(null);
    try {
      setModels(await getAdminModels());
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load models');
    }
  }, []);

  useEffect(() => {
    fetchData();
  }, [fetchData]);

  const handleStarted = async () => {
    setStartModel(null);
    await refreshModels();
  };

  const handleStop = async (modelId: string) => {
    setConfirmStop(null);
    setActionLoading(modelId);
    try {
      await stopContainer(modelId);
      await refreshModels();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to stop container');
    } finally {
      setActionLoading(null);
    }
  };

  const handleDelete = async (model: AdminModel, override = false) => {
    setConfirmDelete(null);
    setOverrideDelete(null);
    setActionLoading(model.id);
    try {
      await deleteModel(model.id, { override });
      await refreshModels();
    } catch (err) {
      // 409 — model is in use by active tokens. Surface the blockers so the
      // admin can choose to force-delete (which soft-deletes those tokens).
      if (
        err instanceof ApiError &&
        err.status === 409 &&
        Array.isArray((err.data as { blocking_tokens?: unknown })?.blocking_tokens)
      ) {
        const blockingTokens = (err.data as { blocking_tokens: BlockingToken[] }).blocking_tokens;
        setOverrideDelete({ model, blockingTokens });
      } else {
        setError(err instanceof Error ? err.message : 'Failed to delete model');
      }
    } finally {
      setActionLoading(null);
    }
  };

  if (loading) return <LoadingSpinner message="Loading system info..." />;
  if (error && !system) return <ErrorAlert message={error} onRetry={fetchData} />;
  if (!system) return null;

  // Merge SSE data over REST baseline where available
  const gpuMemory: GpuMemory[] = snapshot?.gpu_memory ?? system.gpu_memory ?? [];
  const cpu: CpuInfo | null = snapshot?.cpu ?? null;
  const containers: SystemContainer[] = snapshot?.containers ?? system.containers;
  const queues = snapshot?.queues ?? system.queues;
  const gates: Record<string, GateSnapshot> = snapshot?.gates ?? system.gates ?? {};
  const disk = snapshot?.disk ?? system.disk;

  const diskPercent = disk.total_bytes > 0
    ? (disk.used_bytes / disk.total_bytes) * 100
    : 0;

  // Derive available GPU types for the start dialog
  const availableGpuTypes: string[] = (() => {
    const types = new Set<string>();
    for (const g of system.gpu) {
      types.add(g);
    }
    types.add('none'); // CPU is always available
    return Array.from(types);
  })();

  // Build a lookup from model_id -> container info
  const containerMap = new Map(
    containers.map((c) => [c.model_id, c]),
  );

  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '1.5rem' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
          <h1 style={{ margin: 0 }}>System</h1>
          <LiveIndicator status={sseStatus} />
        </div>
        <button
          onClick={refreshModels}
          style={{
            padding: '0.5rem 1rem',
            background: colors.buttonPrimary,
            color: '#fff',
            border: 'none',
            borderRadius: 4,
            cursor: 'pointer',
          }}
        >
          Refresh Models
        </button>
      </div>

      {error && <ErrorAlert message={error} />}

      {/* Disk usage */}
      <div style={{ ...cardStyle, marginBottom: '1.5rem' }}>
        <h3 style={{ margin: '0 0 0.75rem' }}>Disk Usage — {system.disk.model_path}</h3>
        <div style={{ background: colors.progressBarBg, borderRadius: 8, height: 24, overflow: 'hidden', marginBottom: '0.5rem' }}>
          <div
            style={{
              height: '100%',
              width: `${diskPercent}%`,
              background: percentColor(diskPercent, colors),
              borderRadius: 8,
              transition: 'width 0.3s ease',
            }}
          />
        </div>
        <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '0.85rem', color: colors.textMuted }}>
          <span>{formatBytes(disk.used_bytes)} used</span>
          <span>{formatBytes(disk.free_bytes)} free</span>
          <span>{formatBytes(disk.total_bytes)} total ({diskPercent.toFixed(1)}%)</span>
        </div>
      </div>

      {/* CPU utilization */}
      {cpu && (
        <div style={{ ...cardStyle, marginBottom: '1.5rem' }}>
          <h3 style={{ margin: '0 0 0.75rem' }}>CPU — {cpu.num_cores} cores</h3>
          <div style={{ background: colors.progressBarBg, borderRadius: 8, height: 20, overflow: 'hidden', marginBottom: '0.35rem' }}>
            <div
              style={{
                height: '100%',
                width: `${cpu.utilization_percent}%`,
                background: percentColor(cpu.utilization_percent, colors),
                borderRadius: 8,
                transition: 'width 0.3s ease',
              }}
            />
          </div>
          <div style={{ fontSize: '0.85rem', color: colors.textMuted }}>
            {cpu.utilization_percent.toFixed(1)}% utilization
          </div>
        </div>
      )}

      {/* GPU & Backend info */}
      {(system.gpu.length > 0 || system.available_backends.length > 0) && (
        <div style={{ ...cardStyle, marginBottom: '1.5rem' }}>
          <h3 style={{ margin: '0 0 0.5rem' }}>GPU &amp; Backends</h3>
          <div style={{ fontSize: '0.9rem', color: colors.textPrimary }}>
            <div style={{ marginBottom: '0.25rem' }}>
              <strong>Detected GPUs:</strong>{' '}
              {system.gpu.length > 0 ? system.gpu.map(g => g === 'vulkan' ? 'Vulkan' : g).join(', ') : 'None'}
            </div>
            <div>
              <strong>Available backends:</strong>{' '}
              {system.available_backends.map(b => b === 'llamacpp' ? 'llama.cpp' : b).join(', ')}
            </div>
          </div>
          {gpuMemory.map((gm) => {
            const vramPercent = gm.total_mb > 0 ? (gm.used_mb / gm.total_mb) * 100 : 0;
            const gpuLabel = gm.gpu_type === 'nvidia' ? `NVIDIA GPU ${gm.device_index}` : `AMD GPU ${gm.device_index}`;
            return (
              <div key={`${gm.gpu_type}-${gm.device_index}`} style={{ marginTop: '0.75rem' }}>
                <div style={{ fontSize: '0.85rem', fontWeight: 600, color: colors.textSecondary, marginBottom: '0.35rem' }}>{gpuLabel}</div>
                {gm.utilization_percent != null && (
                  <div style={{ marginBottom: '0.75rem' }}>
                    <div style={{ background: colors.progressBarBg, borderRadius: 8, height: 14, overflow: 'hidden', marginBottom: '0.25rem' }}>
                      <div
                        style={{
                          height: '100%',
                          width: `${gm.utilization_percent}%`,
                          background: percentColor(gm.utilization_percent, colors),
                          borderRadius: 8,
                          transition: 'width 0.3s ease',
                        }}
                      />
                    </div>
                    <div style={{ fontSize: '0.8rem', color: colors.textMuted }}>{gm.utilization_percent}% compute</div>
                  </div>
                )}
                <div style={{ background: colors.progressBarBg, borderRadius: 8, height: 20, overflow: 'hidden', marginBottom: '0.35rem' }}>
                  <div
                    style={{
                      height: '100%',
                      width: `${vramPercent}%`,
                      background: percentColor(vramPercent, colors),
                      borderRadius: 8,
                      transition: 'width 0.3s ease',
                    }}
                  />
                </div>
                <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: '0.8rem', color: colors.textMuted }}>
                  <span>{formatBytes(gm.used_mb * 1024 * 1024)} used</span>
                  <span>{formatBytes(gm.free_mb * 1024 * 1024)} free</span>
                  <span>{formatBytes(gm.total_mb * 1024 * 1024)} total ({vramPercent.toFixed(1)}%)</span>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {/* Models table */}
      <h2 style={{ marginBottom: '0.75rem' }}>Models</h2>
      {models.length === 0 ? (
        <p style={{ color: colors.textMuted }}>No models registered.</p>
      ) : (
        <div style={{ overflowX: 'auto' }}>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '0.85rem' }}>
            <thead>
              <tr style={{ borderBottom: `2px solid ${colors.cardBorder}`, textAlign: 'left' }}>
                <th style={{ padding: '0.5rem' }}>Repository</th>
                <th style={{ padding: '0.5rem', textAlign: 'right' }}>Size</th>
                <th style={{ padding: '0.5rem', textAlign: 'right' }}>Context</th>
                <th style={{ padding: '0.5rem' }}>Backend</th>
                <th style={{ padding: '0.5rem' }}>Health</th>
                <th style={{ padding: '0.5rem' }}>Slots</th>
                <th style={{ padding: '0.5rem', textAlign: 'right' }}>VRAM</th>
                <th style={{ padding: '0.5rem', textAlign: 'right' }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {models.map((model) => {
                const container = containerMap.get(model.id);
                const isLoaded = !!container;
                const busy = actionLoading === model.id;
                const gate = gates[model.id];
                const queue = queues[model.id];

                return (
                  <tr key={model.id} style={{ borderBottom: `1px solid ${colors.tableRowBorder}` }}>
                    <td style={{ padding: '0.5rem' }}>
                      <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', wordBreak: 'break-all' }}>
                        <span>{model.hf_repo}</span>
                        {model.mmproj_filename && (
                          <span
                            title={`mmproj: ${model.mmproj_filename}`}
                            style={{
                              display: 'inline-block',
                              padding: '0.15rem 0.5rem',
                              borderRadius: 12,
                              fontSize: '0.7rem',
                              fontWeight: 600,
                              background: colors.badgePurpleBg,
                              color: colors.badgePurpleText,
                              whiteSpace: 'nowrap',
                            }}
                          >
                            Vision
                          </span>
                        )}
                      </div>
                    </td>
                    <td style={{ padding: '0.5rem', textAlign: 'right', whiteSpace: 'nowrap' }}>
                      {formatBytes(model.size_bytes)}
                    </td>
                    <td style={{ padding: '0.5rem', textAlign: 'right', whiteSpace: 'nowrap' }}>
                      {model.context_length ? formatNumber(model.context_length) : '-'}
                    </td>
                    <td style={{ padding: '0.5rem', whiteSpace: 'nowrap' }}>
                      {isLoaded ? (
                        <span
                          style={{
                            display: 'inline-block',
                            padding: '0.15rem 0.5rem',
                            borderRadius: 12,
                            fontSize: '0.7rem',
                            fontWeight: 600,
                            background: colors.badgeWarningBg,
                            color: colors.badgeWarningText,
                          }}
                        >
                          llama.cpp
                        </span>
                      ) : (
                        <span style={{ color: colors.textMuted }}>llama.cpp</span>
                      )}
                    </td>
                    <td style={{ padding: '0.5rem' }}>
                      {isLoaded ? (
                        <span
                          style={{
                            display: 'inline-block',
                            padding: '0.15rem 0.5rem',
                            borderRadius: 12,
                            fontSize: '0.75rem',
                            fontWeight: 600,
                            background: container.healthy ? colors.badgeSuccessBg : colors.badgeDangerBg,
                            color: container.healthy ? colors.badgeSuccessText : colors.badgeDangerText,
                          }}
                        >
                          {container.healthy ? 'Healthy' : container.state || 'Unhealthy'}
                        </span>
                      ) : (
                        <span style={{ color: colors.textMuted }}>-</span>
                      )}
                    </td>
                    <td style={{ padding: '0.5rem', whiteSpace: 'nowrap' }}>
                      {gate ? (
                        <span>
                          <span style={{
                            fontWeight: 600,
                            color: gate.in_flight > 0 ? colors.warningText : colors.textMuted,
                          }}>
                            {gate.in_flight}/{gate.max_slots}
                          </span>
                          {(queue?.depth ?? 0) > 0 && (
                            <span style={{ color: colors.dangerText, marginLeft: '0.4rem', fontSize: '0.8rem' }}>
                              {queue.depth} queued
                            </span>
                          )}
                        </span>
                      ) : (
                        <span style={{ color: colors.textMuted }}>-</span>
                      )}
                    </td>
                    <td style={{ padding: '0.5rem', textAlign: 'right', whiteSpace: 'nowrap' }}>
                      {isLoaded && container.vram_used_mb != null
                        ? formatBytes(container.vram_used_mb * 1024 * 1024)
                        : <span style={{ color: colors.textMuted }}>-</span>
                      }
                    </td>
                    <td style={{ padding: '0.5rem', textAlign: 'right', whiteSpace: 'nowrap' }}>
                      <div style={{ display: 'flex', gap: '0.35rem', justifyContent: 'flex-end' }}>
                        {isLoaded ? (
                          <button
                            onClick={() => setConfirmStop(model.id)}
                            disabled={busy}
                            style={{
                              padding: '0.25rem 0.6rem',
                              background: colors.buttonDanger,
                              color: '#fff',
                              border: 'none',
                              borderRadius: 6,
                              cursor: busy ? 'default' : 'pointer',
                              fontSize: '0.8rem',
                              opacity: busy ? 0.5 : 1,
                            }}
                          >
                            {busy ? 'Stopping...' : 'Stop'}
                          </button>
                        ) : (
                          <button
                            onClick={() => setStartModel(model)}
                            disabled={busy}
                            style={{
                              padding: '0.25rem 0.6rem',
                              background: colors.successText,
                              color: '#fff',
                              border: 'none',
                              borderRadius: 6,
                              cursor: busy ? 'default' : 'pointer',
                              fontSize: '0.8rem',
                              opacity: busy ? 0.5 : 1,
                            }}
                          >
                            {busy ? 'Starting...' : 'Start'}
                          </button>
                        )}
                        <button
                          onClick={() => setConfirmDelete(model)}
                          disabled={busy}
                          style={{
                            padding: '0.25rem 0.6rem',
                            background: colors.buttonDanger,
                            color: '#fff',
                            border: 'none',
                            borderRadius: 6,
                            cursor: busy ? 'default' : 'pointer',
                            fontSize: '0.8rem',
                            opacity: busy ? 0.5 : 1,
                          }}
                        >
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {startModel && (
        <StartModelDialog
          model={startModel}
          availableGpuTypes={availableGpuTypes}
          onStarted={handleStarted}
          onCancel={() => setStartModel(null)}
        />
      )}

      {confirmStop && (
        <ConfirmDialog
          title="Stop Container"
          message={`Stop the container for model "${confirmStop}"? Any in-flight requests will be terminated.`}
          confirmLabel="Stop"
          destructive
          onConfirm={() => handleStop(confirmStop)}
          onCancel={() => setConfirmStop(null)}
        />
      )}

      {confirmDelete && (
        <ConfirmDialog
          title="Delete Model"
          message={`Delete ${confirmDelete.hf_repo}? This will stop any running container and remove all downloaded files.`}
          confirmLabel="Delete"
          destructive
          onConfirm={() => handleDelete(confirmDelete)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}

      {overrideDelete && (
        <OverrideDeleteDialog
          model={overrideDelete.model}
          blockingTokens={overrideDelete.blockingTokens}
          onConfirm={() => handleDelete(overrideDelete.model, true)}
          onCancel={() => setOverrideDelete(null)}
        />
      )}
    </div>
  );
}

function OverrideDeleteDialog({
  model,
  blockingTokens,
  onConfirm,
  onCancel,
}: Readonly<{
  model: AdminModel;
  blockingTokens: BlockingToken[];
  onConfirm: () => void;
  onCancel: () => void;
}>) {
  const { colors } = useTheme();
  const dialogRef = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    dialogRef.current?.showModal();
  }, []);

  const count = blockingTokens.length;
  const label = count === 1 ? 'token' : 'tokens';

  return (
    <>
      <style>{`.confirm-dialog::backdrop { background: ${colors.overlayBg}; }`}</style>
      <dialog
        ref={dialogRef}
        className="confirm-dialog"
        style={{
          border: 'none',
          borderRadius: 8,
          padding: '1.5rem',
          maxWidth: 520,
          width: '90%',
          boxShadow: colors.dialogShadow,
          background: colors.dialogBg,
          color: 'inherit',
        }}
        onClose={onCancel}
        onClick={(e) => {
          if (e.target === e.currentTarget) onCancel();
        }}
      >
        <h3 style={{ margin: '0 0 0.75rem', color: colors.textPrimary }}>Model in use</h3>
        <p style={{ margin: '0 0 0.75rem', color: colors.textMuted, lineHeight: 1.5 }}>
          <strong>{model.hf_repo}</strong> is pinned by {count} active {label}. Deleting
          the model will revoke {count === 1 ? 'this token' : 'these tokens'} — the owner
          will need a new token before they can use the service again.
        </p>
        <ul
          style={{
            margin: '0 0 1rem',
            padding: '0.5rem 0.75rem',
            listStyle: 'none',
            maxHeight: 180,
            overflowY: 'auto',
            border: `1px solid ${colors.buttonDisabled}`,
            borderRadius: 4,
            background: colors.cardBg,
          }}
        >
          {blockingTokens.map((t) => (
            <li
              key={t.id}
              style={{ padding: '0.25rem 0', color: colors.textPrimary, fontSize: '0.9rem' }}
            >
              <strong>{t.name}</strong>
              {t.user_email && (
                <span style={{ color: colors.textMuted }}> — {t.user_email}</span>
              )}
            </li>
          ))}
        </ul>
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '0.75rem' }}>
          <button
            onClick={onCancel}
            style={{
              padding: '0.5rem 1rem',
              background: colors.buttonDisabled,
              color: colors.textSecondary,
              border: 'none',
              borderRadius: 4,
              cursor: 'pointer',
            }}
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            style={{
              padding: '0.5rem 1rem',
              background: colors.buttonDanger,
              color: '#fff',
              border: 'none',
              borderRadius: 4,
              cursor: 'pointer',
            }}
          >
            Revoke {count} {label} and delete
          </button>
        </div>
      </dialog>
    </>
  );
}
