import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, screen, cleanup, waitFor, fireEvent } from '@testing-library/react';
import { createElement } from 'react';
import type { ReactNode } from 'react';
import { ThemeProvider } from '../../theme';
import System from './System';
import { getSystemInfo, getAdminModels, getCrashHistory } from '../../api';
import type {
  AdminModel,
  SystemInfo,
  SystemContainer,
  CrashHistoryRow,
} from '../../types';

vi.mock('../../api', async () => {
  const actual = await vi.importActual<typeof import('../../api')>('../../api');
  return {
    ...actual,
    getSystemInfo: vi.fn(),
    getAdminModels: vi.fn(),
    getCrashHistory: vi.fn(),
    stopContainer: vi.fn(),
    deleteModel: vi.fn(),
  };
});

vi.mock('../../hooks/useEventStream', () => ({
  useEventStream: () => ({
    snapshot: null,
    reservationRevision: 0,
    status: 'connected' as const,
  }),
}));

const mockedGetSystemInfo = vi.mocked(getSystemInfo);
const mockedGetAdminModels = vi.mocked(getAdminModels);
const mockedGetCrashHistory = vi.mocked(getCrashHistory);

afterEach(cleanup);

beforeEach(() => {
  vi.clearAllMocks();

  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    configurable: true,
    value: vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });

  HTMLDialogElement.prototype.showModal = vi.fn();
  HTMLDialogElement.prototype.close = vi.fn();
});

function wrapper({ children }: { children: ReactNode }) {
  return createElement(ThemeProvider, null, children);
}

function makeSystem(overrides: Partial<SystemInfo> = {}): SystemInfo {
  return {
    disk: {
      model_path: '/models',
      total_bytes: 1_000_000_000,
      used_bytes: 500_000_000,
      free_bytes: 500_000_000,
    },
    queues: {},
    gates: {},
    containers: [],
    gpu: [],
    gpu_memory: [],
    available_backends: ['llamacpp'],
    ...overrides,
  };
}

function makeModel(overrides: Partial<AdminModel> = {}): AdminModel {
  return {
    id: 'm1',
    hf_repo: 'org/model',
    filename: 'model-q4.gguf',
    mmproj_filename: null,
    size_bytes: 1_000_000_000,
    category_id: null,
    loaded: false,
    backend_port: null,
    backend_type: 'llamacpp',
    last_used_at: null,
    created_at: '2025-01-01T00:00:00Z',
    context_length: 4096,
    n_layers: null,
    n_heads: null,
    n_kv_heads: null,
    embedding_length: null,
    runtime_overrides: null,
    quarantined_at: null,
    quarantine_reason: null,
    ...overrides,
  };
}

function makeContainer(overrides: Partial<SystemContainer> = {}): SystemContainer {
  return {
    model_id: 'm1',
    backend_type: 'llamacpp',
    healthy: true,
    state: 'running',
    vram_used_mb: 4096,
    quarantined: false,
    quarantine_reason: null,
    last_crash: null,
    ...overrides,
  };
}

async function renderSystem(models: AdminModel[], containers: SystemContainer[] = []) {
  mockedGetSystemInfo.mockResolvedValue(makeSystem({ containers }));
  mockedGetAdminModels.mockResolvedValue(models);
  const result = render(<System />, { wrapper });
  // Wait for the initial load to complete (table renders).
  await waitFor(() => {
    expect(screen.getByText('Models')).toBeTruthy();
  });
  return result;
}

describe('System page — Vision badge', () => {
  it('shows Vision badge for a model with mmproj_filename', async () => {
    await renderSystem([
      makeModel({ id: 'vision-1', hf_repo: 'org/vision-model', mmproj_filename: 'mmproj-foo-f16.gguf' }),
    ]);

    expect(screen.getByText('Vision')).toBeTruthy();
  });

  it('omits Vision badge for a text-only model', async () => {
    await renderSystem([
      makeModel({ id: 'text-1', hf_repo: 'org/text-model', mmproj_filename: null }),
    ]);

    expect(screen.queryByText('Vision')).toBeNull();
  });

  it('Vision badge tooltip shows the mmproj filename', async () => {
    await renderSystem([
      makeModel({
        id: 'vision-1',
        hf_repo: 'org/vision-model',
        mmproj_filename: 'mmproj-foo-f16.gguf',
      }),
    ]);

    const badge = screen.getByText('Vision');
    expect(badge.getAttribute('title')).toBe('mmproj: mmproj-foo-f16.gguf');
  });

  it('renders exactly one Vision badge when mixing vision and text models', async () => {
    await renderSystem([
      makeModel({ id: 'vision-1', hf_repo: 'org/vision-model', mmproj_filename: 'mmproj-foo-f16.gguf' }),
      makeModel({ id: 'text-1', hf_repo: 'org/text-model', mmproj_filename: null }),
    ]);

    expect(screen.getAllByText('Vision')).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Phase 7: status-badge state matrix, crash history, quarantine UX
// ---------------------------------------------------------------------------

describe('System page — Phase 7 status badge state matrix', () => {
  const cases: Array<{
    name: string;
    container: Partial<SystemContainer>;
    label: string;
  }> = [
    { name: 'Starting → "Loading"', container: { fsm_state: 'Starting' }, label: 'Loading' },
    { name: 'Healthy → "Healthy"', container: { fsm_state: 'Healthy' }, label: 'Healthy' },
    { name: 'Suspect → "Unhealthy"', container: { fsm_state: 'Suspect' }, label: 'Unhealthy' },
    { name: 'Crashed → "Crashed"', container: { fsm_state: 'Crashed' }, label: 'Crashed' },
    {
      name: 'quarantined=true → "Quarantined"',
      container: { fsm_state: 'Crashed', quarantined: true, quarantine_reason: 'never worked' },
      label: 'Quarantined',
    },
  ];

  for (const c of cases) {
    it(c.name, async () => {
      const model = makeModel({ id: 'badge-1', loaded: true });
      const container = makeContainer({ model_id: 'badge-1', ...c.container });
      await renderSystem([model], [container]);
      expect(screen.getByText(c.label)).toBeTruthy();
    });
  }

  it('falls back to legacy Healthy/Unhealthy when fsm_state is missing', async () => {
    const model = makeModel({ id: 'legacy-1', loaded: true });
    const container = makeContainer({ model_id: 'legacy-1', healthy: true });
    delete (container as { fsm_state?: string }).fsm_state;
    await renderSystem([model], [container]);
    expect(screen.getByText('Healthy')).toBeTruthy();
  });

  it('shows Quarantined badge for unloaded models that have models.quarantined_at set', async () => {
    const model = makeModel({
      id: 'quar-1',
      loaded: false,
      quarantined_at: '2026-04-29T11:00:00Z',
      quarantine_reason: 'never worked',
    });
    await renderSystem([model], []);
    expect(screen.getByText('Quarantined')).toBeTruthy();
  });
});

describe('System page — Phase 7 Start/Restart button on Quarantined', () => {
  it('relabels Start as Restart when the model is quarantined', async () => {
    const model = makeModel({
      id: 'quar-1',
      loaded: false,
      quarantined_at: '2026-04-29T11:00:00Z',
      quarantine_reason: 'startup failed',
    });
    await renderSystem([model], []);
    expect(screen.getByTestId('restart-button')).toBeTruthy();
    expect(screen.getByText('Restart')).toBeTruthy();
  });

  it('shows the quarantine note explaining Start clears the flag', async () => {
    const model = makeModel({
      id: 'quar-1',
      loaded: false,
      quarantined_at: '2026-04-29T11:00:00Z',
      quarantine_reason: 'startup failed',
    });
    await renderSystem([model], []);
    expect(screen.getByTestId('quarantine-note').textContent).toMatch(
      /Quarantined.*Restart.*clear the quarantine flag.*launch/,
    );
  });

  it('keeps the Start label and hides the note when not quarantined', async () => {
    const model = makeModel({ id: 'fresh-1', loaded: false });
    await renderSystem([model], []);
    expect(screen.getByTestId('start-button')).toBeTruthy();
    expect(screen.getByText('Start')).toBeTruthy();
    expect(screen.queryByTestId('quarantine-note')).toBeNull();
  });
});

describe('System page — Phase 7 crash history panel', () => {
  function makeCrashRow(over: Partial<CrashHistoryRow> = {}): CrashHistoryRow {
    return {
      occurred_at: '2026-04-29T10:00:00Z',
      container_id: 'c1',
      exit_code: 1,
      oom_killed: 0,
      signal: null,
      log_path_present: true,
      ...over,
    };
  }

  it('fetches and renders 5 crash rows when toggle is clicked', async () => {
    const model = makeModel({ id: 'crashy', loaded: false });
    const rows: CrashHistoryRow[] = Array.from({ length: 5 }, (_, i) =>
      makeCrashRow({
        occurred_at: `2026-04-29T1${i}:00:00Z`,
        exit_code: i,
        oom_killed: i === 0 ? 1 : 0,
      }),
    );
    mockedGetCrashHistory.mockResolvedValueOnce(rows);

    await renderSystem([model], []);
    fireEvent.click(screen.getByTestId('crash-history-toggle'));

    await waitFor(() => {
      expect(screen.getByTestId('crash-history-list')).toBeTruthy();
    });

    const list = screen.getByTestId('crash-history-list');
    expect(list.querySelectorAll('li')).toHaveLength(5);
    expect(mockedGetCrashHistory).toHaveBeenCalledWith('crashy');
  });

  it('shows "log no longer available" when log_path_present is false', async () => {
    const model = makeModel({ id: 'crashy', loaded: false });
    mockedGetCrashHistory.mockResolvedValueOnce([
      makeCrashRow({ log_path_present: false }),
    ]);

    await renderSystem([model], []);
    fireEvent.click(screen.getByTestId('crash-history-toggle'));

    await waitFor(() => {
      expect(screen.getByText('log no longer available')).toBeTruthy();
    });
  });

  it('renders the view-log link as a new-tab anchor when log is present', async () => {
    const model = makeModel({ id: 'crashy', loaded: false });
    mockedGetCrashHistory.mockResolvedValueOnce([
      makeCrashRow({ log_path_present: true, occurred_at: '2026-04-29T11:00:00Z' }),
    ]);

    await renderSystem([model], []);
    fireEvent.click(screen.getByTestId('crash-history-toggle'));

    await waitFor(() => {
      expect(screen.getByText('view log')).toBeTruthy();
    });
    const link = screen.getByText('view log') as HTMLAnchorElement;
    expect(link.target).toBe('_blank');
    expect(link.href).toMatch(/\/api\/admin\/models\/crashy\/crash_log\//);
  });

  it('shows error message if crash history fetch fails', async () => {
    const model = makeModel({ id: 'crashy', loaded: false });
    mockedGetCrashHistory.mockRejectedValueOnce(new Error('network down'));

    await renderSystem([model], []);
    fireEvent.click(screen.getByTestId('crash-history-toggle'));

    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toMatch(/network down/);
    });
  });

  it('shows "No crash events recorded" when history is empty', async () => {
    const model = makeModel({ id: 'never-crashed', loaded: false });
    mockedGetCrashHistory.mockResolvedValueOnce([]);

    await renderSystem([model], []);
    fireEvent.click(screen.getByTestId('crash-history-toggle'));

    await waitFor(() => {
      expect(screen.getByText('No crash events recorded.')).toBeTruthy();
    });
  });
});
