import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, screen, fireEvent, waitFor, cleanup, act } from '@testing-library/react';
import { createElement } from 'react';
import type { ReactNode } from 'react';
import { ThemeProvider } from '../../theme';
import StartModelDialog from './StartModelDialog';
import { estimateVram, startContainer } from '../../api';
import type { AdminModel, VramEstimate } from '../../types';

vi.mock('../../api', () => ({
  estimateVram: vi.fn(),
  startContainer: vi.fn(),
}));

const mockedEstimateVram = vi.mocked(estimateVram);
const mockedStartContainer = vi.mocked(startContainer);

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

const fittingEstimate: VramEstimate = {
  model_weights_mb: 4000,
  kv_cache_mb: 512,
  overhead_mb: 200,
  total_mb: 4712,
  gpu_total_mb: 24576,
  gpu_used_mb: 2000,
  gpu_free_mb: 22576,
  fits: true,
};

const overflowEstimate: VramEstimate = {
  model_weights_mb: 20000,
  kv_cache_mb: 4000,
  overhead_mb: 1000,
  total_mb: 25000,
  gpu_total_mb: 24576,
  gpu_used_mb: 2000,
  gpu_free_mb: 22576,
  fits: false,
};

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers();

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

  mockedEstimateVram.mockResolvedValue(fittingEstimate);
  mockedStartContainer.mockResolvedValue({ container: 'c-1', url: 'http://localhost:8080' });
});

function wrapper({ children }: { children: ReactNode }) {
  return createElement(ThemeProvider, null, children);
}

function renderDialog(props: Partial<React.ComponentProps<typeof StartModelDialog>> = {}) {
  const defaults = {
    model: baseModel,
    availableGpuTypes: ['cuda', 'none'],
    onStarted: vi.fn(),
    onCancel: vi.fn(),
  };
  const merged = { ...defaults, ...props };
  const result = render(<StartModelDialog {...merged} />, { wrapper });
  return { ...result, ...merged };
}

// Helper to flush the debounce timer and let the estimate call resolve
async function flushEstimate() {
  await act(async () => {
    vi.advanceTimersByTime(350);
  });
}

describe('StartModelDialog', () => {
  it('calls showModal on mount', async () => {
    renderDialog();
    expect(HTMLDialogElement.prototype.showModal).toHaveBeenCalled();
    await flushEstimate();
  });

  it('renders model info', async () => {
    renderDialog();

    expect(screen.getByText('Start Model')).toBeTruthy();
    expect(screen.getByText(/TheBloke\/Llama-7B-GGUF/)).toBeTruthy();
    expect(screen.getByText(/llama-7b.Q4_K_M.gguf/)).toBeTruthy();

    await flushEstimate();
  });

  it('renders model size in GB', async () => {
    renderDialog();

    // 4,000,000,000 bytes = ~3814 MB = ~3.7 GB
    expect(screen.getByText(/3\.7 GB/)).toBeTruthy();

    await flushEstimate();
  });

  it('renders context size as read-only', async () => {
    renderDialog();

    expect(screen.getByText('Context Size')).toBeTruthy();
    expect(screen.getByText('8K tokens')).toBeTruthy();

    await flushEstimate();
  });

  it('renders parallel sequences select', async () => {
    renderDialog();

    const select = screen.getByLabelText('Parallel Sequences') as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.value).toBe('1');

    await flushEstimate();
  });

  it('renders GPU select with available options', async () => {
    renderDialog({ availableGpuTypes: ['cuda', 'vulkan', 'none'] });

    const select = screen.getByLabelText('GPU') as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.value).toBe('cuda');
    // Check that labels are applied
    expect(screen.getByText('Vulkan')).toBeTruthy();
    expect(screen.getByText('CPU')).toBeTruthy();

    await flushEstimate();
  });

  it('renders GPU layers input', async () => {
    renderDialog();

    const input = screen.getByLabelText('GPU Layers') as HTMLInputElement;
    expect(input).toBeTruthy();
    expect(input.value).toBe('99');

    await flushEstimate();
  });

  it('calls estimateVram with debounce on mount', async () => {
    renderDialog();

    expect(mockedEstimateVram).not.toHaveBeenCalled();

    await flushEstimate();

    expect(mockedEstimateVram).toHaveBeenCalledWith('model-1', 1);
  });

  it('shows VRAM estimation bar when estimate is available and has GPU', async () => {
    renderDialog();

    await flushEstimate();

    expect(screen.getByText('VRAM Estimate')).toBeTruthy();
    // Should show "This model" and "Other" and "Total" labels
    expect(screen.getByText(/This model/)).toBeTruthy();
    expect(screen.getByText(/Other/)).toBeTruthy();
    expect(screen.getByText(/Total/)).toBeTruthy();
  });

  it('shows KV cache breakdown when kv_cache_mb > 0', async () => {
    renderDialog();

    await flushEstimate();

    expect(screen.getByText(/Weights:/)).toBeTruthy();
    expect(screen.getByText(/KV cache:/)).toBeTruthy();
    expect(screen.getByText(/overhead:/)).toBeTruthy();
  });

  it('calls startContainer on start button click', async () => {
    const onStarted = vi.fn();
    renderDialog({ onStarted });

    await flushEstimate();

    // Switch to real timers for the start action
    vi.useRealTimers();

    fireEvent.click(screen.getByText('Start'));

    await waitFor(() => {
      expect(mockedStartContainer).toHaveBeenCalledWith({
        model_id: 'model-1',
        backend_type: 'llamacpp',
        gpu_type: 'cuda',
        gpu_layers: 99,
        parallel: 1,
      });
    });

    await waitFor(() => {
      expect(onStarted).toHaveBeenCalledOnce();
    });
  });

  it('disables start button when VRAM overflows', async () => {
    mockedEstimateVram.mockResolvedValue(overflowEstimate);

    renderDialog();

    await flushEstimate();

    const startBtn = screen.getByText('Start') as HTMLButtonElement;
    expect(startBtn.disabled).toBe(true);

    // Should show overflow warning
    expect(screen.getByText('Estimated VRAM exceeds available GPU memory')).toBeTruthy();
  });

  it('shows start error when startContainer fails', async () => {
    mockedStartContainer.mockRejectedValue(new Error('GPU busy'));

    renderDialog();

    await flushEstimate();

    vi.useRealTimers();

    fireEvent.click(screen.getByText('Start'));

    await waitFor(() => {
      expect(screen.getByText('GPU busy')).toBeTruthy();
    });
  });

  it('shows estimate error when estimateVram fails', async () => {
    mockedEstimateVram.mockRejectedValue(new Error('Model not found'));

    renderDialog();

    await flushEstimate();

    expect(screen.getByText(/Estimate error: Model not found/)).toBeTruthy();
  });

  it('calls onCancel when cancel button clicked', async () => {
    const onCancel = vi.fn();
    renderDialog({ onCancel });

    await flushEstimate();

    fireEvent.click(screen.getByText('Cancel'));
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('shows "no GPU memory info" when estimate has no GPU', async () => {
    mockedEstimateVram.mockResolvedValue({
      ...fittingEstimate,
      gpu_total_mb: 0,
      gpu_used_mb: 0,
      gpu_free_mb: 0,
    });

    renderDialog();

    await flushEstimate();

    expect(screen.getByText(/No GPU memory info available/)).toBeTruthy();
  });
});
