import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, screen, fireEvent, waitFor, cleanup } from '@testing-library/react';
import { createElement } from 'react';
import type { ReactNode } from 'react';
import { ThemeProvider } from '../../theme';
import RuntimeOverridesEditor from './RuntimeOverridesEditor';
import { parseDraft, buildCliPreview, overridesToDraft } from './runtimeOverrides';
import { updateModel } from '../../api';
import type { AdminModel } from '../../types';

vi.mock('../../api', () => ({
  updateModel: vi.fn(),
}));

const mockedUpdateModel = vi.mocked(updateModel);

const baseModel: AdminModel = {
  id: 'model-1',
  hf_repo: 'TheBloke/Llama-7B-GGUF',
  filename: 'llama-7b.Q4_K_M.gguf',
  mmproj_filename: null,
  size_bytes: 4_000_000_000,
  category_id: null,
  loaded: false,
  backend_port: null,
  backend_type: 'llamacpp',
  last_used_at: null,
  created_at: '2026-01-01T00:00:00Z',
  context_length: 8192,
  n_layers: 32,
  n_heads: 32,
  n_kv_heads: 8,
  embedding_length: 4096,
  runtime_overrides: null,
};

afterEach(() => {
  cleanup();
});

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

  mockedUpdateModel.mockResolvedValue(undefined);
});

function wrapper({ children }: { children: ReactNode }) {
  return createElement(ThemeProvider, null, children);
}

function renderEditor(props: Partial<React.ComponentProps<typeof RuntimeOverridesEditor>> = {}) {
  const defaults = {
    model: baseModel,
    onSaved: vi.fn(),
    onCancel: vi.fn(),
  };
  const merged = { ...defaults, ...props };
  const result = render(<RuntimeOverridesEditor {...merged} />, { wrapper });
  return { ...result, ...merged };
}

// =============================================================================
// Pure helper unit tests
// =============================================================================

describe('overridesToDraft()', () => {
  it('returns blank draft for null', () => {
    expect(overridesToDraft(null)).toEqual({
      cache_ram_mib: '',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
  });

  it('preserves explicit 0 as "0", not blank', () => {
    const d = overridesToDraft({ cache_ram_mib: 0 });
    expect(d.cache_ram_mib).toBe('0');
  });

  it('round-trips swa_full true/false', () => {
    expect(overridesToDraft({ swa_full: true }).swa_full).toBe('true');
    expect(overridesToDraft({ swa_full: false }).swa_full).toBe('false');
  });

  it('joins extra args with newlines', () => {
    expect(overridesToDraft({ extra: ['--threads', '8'] }).extra).toBe('--threads\n8');
  });
});

describe('parseDraft()', () => {
  it('omits blank numeric fields from payload', () => {
    const { payload, errors } = parseDraft({
      cache_ram_mib: '',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
    expect(payload).toEqual({});
    expect(errors).toEqual({});
  });

  it('sends explicit 0 as cache_ram_mib: 0', () => {
    const { payload } = parseDraft({
      cache_ram_mib: '0',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
    expect(payload).toEqual({ cache_ram_mib: 0 });
  });

  it('rejects non-integer numeric fields', () => {
    const { payload, errors } = parseDraft({
      cache_ram_mib: 'abc',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
    expect(payload.cache_ram_mib).toBeUndefined();
    expect(errors.cache_ram_mib).toBeDefined();
  });

  it('rejects out-of-range numbers', () => {
    const { errors: e1 } = parseDraft({
      cache_ram_mib: '-1',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
    expect(e1.cache_ram_mib).toMatch(/between 0 and 16384/);

    const { errors: e2 } = parseDraft({
      cache_ram_mib: '99999',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '',
    });
    expect(e2.cache_ram_mib).toMatch(/between 0 and 16384/);
  });

  it('tri-state swa_full: default omits, true sends true, false sends false', () => {
    const blank = { cache_ram_mib: '', ctx_checkpoints: '', cache_reuse: '', extra: '' };
    expect(parseDraft({ ...blank, swa_full: 'default' }).payload.swa_full).toBeUndefined();
    expect(parseDraft({ ...blank, swa_full: 'true' }).payload.swa_full).toBe(true);
    expect(parseDraft({ ...blank, swa_full: 'false' }).payload.swa_full).toBe(false);
  });

  it('trims and drops blank lines from extra args', () => {
    const { payload } = parseDraft({
      cache_ram_mib: '',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '  --threads  \n\n   \n8\n',
    });
    expect(payload.extra).toEqual(['--threads', '8']);
  });

  it('omits extra when all lines are blank', () => {
    const { payload } = parseDraft({
      cache_ram_mib: '',
      swa_full: 'default',
      ctx_checkpoints: '',
      cache_reuse: '',
      extra: '   \n\n  ',
    });
    expect(payload.extra).toBeUndefined();
  });
});

describe('buildCliPreview()', () => {
  it('returns empty string for empty payload', () => {
    expect(buildCliPreview({})).toBe('');
  });

  it('renders all flags in canonical order', () => {
    expect(
      buildCliPreview({
        cache_ram_mib: 0,
        swa_full: true,
        ctx_checkpoints: 32,
        cache_reuse: 256,
        extra: ['--threads', '8'],
      }),
    ).toBe('--cache-ram 0 --swa-full --ctx-checkpoints 32 --cache-reuse 256 --threads 8');
  });

  it('uses --no-swa-full for explicit false', () => {
    expect(buildCliPreview({ swa_full: false })).toBe('--no-swa-full');
  });

  it('omits swa flag entirely when undefined', () => {
    expect(buildCliPreview({ cache_ram_mib: 0 })).toBe('--cache-ram 0');
  });
});

// =============================================================================
// Component tests
// =============================================================================

describe('RuntimeOverridesEditor component', () => {
  it('opens the modal dialog on mount', () => {
    renderEditor();
    expect(HTMLDialogElement.prototype.showModal).toHaveBeenCalled();
  });

  it('renders all the form fields', () => {
    renderEditor();
    expect(screen.getByLabelText('Prompt cache size (MiB)')).toBeTruthy();
    expect(screen.getByLabelText('Full SWA cache')).toBeTruthy();
    expect(screen.getByLabelText('Context checkpoints')).toBeTruthy();
    expect(screen.getByLabelText('Cache reuse min chunk')).toBeTruthy();
    expect(screen.getByLabelText('Extra args (advanced)')).toBeTruthy();
  });

  it('disables Save when form is unchanged', () => {
    renderEditor();
    const save = screen.getByText('Save').closest('button') as HTMLButtonElement;
    expect(save.disabled).toBe(true);
  });

  it('CLI preview reflects inputs in real time', () => {
    renderEditor();

    const cacheRam = screen.getByLabelText('Prompt cache size (MiB)') as HTMLInputElement;
    fireEvent.change(cacheRam, { target: { value: '0' } });

    const preview = screen.getByTestId('cli-preview');
    expect(preview.textContent).toContain('--cache-ram 0');

    const swa = screen.getByLabelText('Full SWA cache') as HTMLSelectElement;
    fireEvent.change(swa, { target: { value: 'true' } });
    expect(preview.textContent).toContain('--swa-full');

    const extra = screen.getByLabelText('Extra args (advanced)') as HTMLTextAreaElement;
    fireEvent.change(extra, { target: { value: '--threads\n8' } });
    expect(preview.textContent).toContain('--threads 8');
  });

  it('blank numeric field stays out of payload, "0" goes in', async () => {
    const onSaved = vi.fn();
    renderEditor({ onSaved });

    const ctx = screen.getByLabelText('Context checkpoints') as HTMLInputElement;
    fireEvent.change(ctx, { target: { value: '0' } });

    const save = screen.getByText('Save').closest('button') as HTMLButtonElement;
    fireEvent.click(save);

    await waitFor(() => {
      expect(mockedUpdateModel).toHaveBeenCalledWith('model-1', {
        runtime_overrides: { ctx_checkpoints: 0 },
      });
    });
    await waitFor(() => {
      expect(onSaved).toHaveBeenCalledWith({ ctx_checkpoints: 0 });
    });
  });

  it('tri-state default vs Yes vs No produces correct payloads', async () => {
    const onSaved = vi.fn();
    renderEditor({ onSaved });

    const swa = screen.getByLabelText('Full SWA cache') as HTMLSelectElement;
    fireEvent.change(swa, { target: { value: 'false' } });

    fireEvent.click(screen.getByText('Save').closest('button') as HTMLButtonElement);

    await waitFor(() => {
      expect(mockedUpdateModel).toHaveBeenCalledWith('model-1', {
        runtime_overrides: { swa_full: false },
      });
    });
  });

  it('shows a per-field validation error and disables Save', () => {
    renderEditor();
    const cacheRam = screen.getByLabelText('Prompt cache size (MiB)') as HTMLInputElement;
    fireEvent.change(cacheRam, { target: { value: '999999' } });

    expect(screen.getByText(/between 0 and 16384/)).toBeTruthy();

    const save = screen.getByText('Save').closest('button') as HTMLButtonElement;
    expect(save.disabled).toBe(true);
  });

  it('calls onCancel when Cancel clicked', () => {
    const onCancel = vi.fn();
    renderEditor({ onCancel });
    fireEvent.click(screen.getByText('Cancel').closest('button') as HTMLButtonElement);
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('shows API error when save fails and keeps form open', async () => {
    mockedUpdateModel.mockRejectedValue(new Error('backend exploded'));

    const onSaved = vi.fn();
    renderEditor({ onSaved });

    fireEvent.change(screen.getByLabelText('Prompt cache size (MiB)'), { target: { value: '0' } });
    fireEvent.click(screen.getByText('Save').closest('button') as HTMLButtonElement);

    await waitFor(() => {
      expect(screen.getByText('backend exploded')).toBeTruthy();
    });
    expect(onSaved).not.toHaveBeenCalled();
  });

  it('pre-populates from existing runtime_overrides', () => {
    renderEditor({
      model: {
        ...baseModel,
        runtime_overrides: { cache_ram_mib: 0, swa_full: true, extra: ['--foo'] },
      },
    });

    expect((screen.getByLabelText('Prompt cache size (MiB)') as HTMLInputElement).value).toBe('0');
    expect((screen.getByLabelText('Full SWA cache') as HTMLSelectElement).value).toBe('true');
    expect((screen.getByLabelText('Extra args (advanced)') as HTMLTextAreaElement).value).toBe('--foo');

    // Save disabled because nothing was changed
    expect((screen.getByText('Save').closest('button') as HTMLButtonElement).disabled).toBe(true);
  });
});
