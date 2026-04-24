import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { createElement } from 'react';
import type { ReactNode } from 'react';
import { ThemeProvider } from '../../theme';
import ModelMapping from './ModelMapping';
import { getCategories, getAdminModels } from '../../api';
import type { AdminModel } from '../../types';

vi.mock('../../api', async () => {
  const actual = await vi.importActual<typeof import('../../api')>('../../api');
  return {
    ...actual,
    getCategories: vi.fn(),
    getAdminModels: vi.fn(),
    createCategory: vi.fn(),
    updateCategory: vi.fn(),
    deleteCategory: vi.fn(),
    updateModel: vi.fn(),
  };
});

const mockedGetCategories = vi.mocked(getCategories);
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

async function renderMapping(models: AdminModel[]) {
  mockedGetCategories.mockResolvedValue([]);
  mockedGetAdminModels.mockResolvedValue(models);
  const result = render(<ModelMapping />, { wrapper });
  await waitFor(() => {
    expect(screen.getByText('Model Category Assignments')).toBeTruthy();
  });
  return result;
}

describe('ModelMapping page — Vision badge', () => {
  it('shows Vision badge for a model with mmproj_filename', async () => {
    await renderMapping([
      makeModel({ id: 'vision-1', hf_repo: 'org/vision-model', mmproj_filename: 'mmproj-foo-f16.gguf' }),
    ]);

    expect(screen.getByText('Vision')).toBeTruthy();
  });

  it('omits Vision badge for a text-only model', async () => {
    await renderMapping([
      makeModel({ id: 'text-1', hf_repo: 'org/text-model', mmproj_filename: null }),
    ]);

    expect(screen.queryByText('Vision')).toBeNull();
  });
});
