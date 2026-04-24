import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { createElement } from 'react';
import type { ReactNode } from 'react';
import { ThemeProvider } from '../../theme';
import System from './System';
import { getSystemInfo, getAdminModels } from '../../api';
import type { AdminModel, SystemInfo } from '../../types';

vi.mock('../../api', async () => {
  const actual = await vi.importActual<typeof import('../../api')>('../../api');
  return {
    ...actual,
    getSystemInfo: vi.fn(),
    getAdminModels: vi.fn(),
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
    ...overrides,
  };
}

async function renderSystem(models: AdminModel[]) {
  mockedGetSystemInfo.mockResolvedValue(makeSystem());
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
