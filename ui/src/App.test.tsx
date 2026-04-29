/**
 * Component tests for the break-glass login section in LoginPage.
 *
 * LoginPage is not exported, so we test it through the full WrappedApp and
 * mock both /auth/me (always 401 so we stay on the login page) and
 * /auth/providers to control `bootstrap_active`.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor, cleanup, act } from '@testing-library/react';
import WrappedApp from './App';

// ---- Globals ----

const mockFetch = vi.fn();
globalThis.fetch = mockFetch;

// LocationHref spy — we can't actually navigate in jsdom, so capture it.
let hrefAssigned: string | null = null;

// matchMedia stub (ThemeProvider uses it)
beforeEach(() => {
  hrefAssigned = null;

  // Set jsdom URL to match the BrowserRouter basename so routing works,
  // and spy on href assignment so we can verify redirects without actually navigating.
  const locationObj = {
    href: 'http://localhost/portal/',
    pathname: '/portal/',
  };
  Object.defineProperty(locationObj, 'href', {
    get() { return hrefAssigned ?? 'http://localhost/portal/'; },
    set(v: string) { hrefAssigned = v; },
    configurable: true,
  });
  Object.defineProperty(window, 'location', {
    writable: true,
    configurable: true,
    value: locationObj,
  });

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
  mockFetch.mockReset();
});

afterEach(cleanup);

// ---- Helpers ----

/** /auth/me returns 401 — keeps the app on the login screen. */
function meUnauthorized() {
  return {
    ok: false,
    status: 401,
    statusText: 'Unauthorized',
    json: () => Promise.resolve({ error: 'Unauthorized' }),
  };
}

function providersResponse(bootstrapActive: boolean) {
  return {
    ok: true,
    status: 200,
    json: () =>
      Promise.resolve({
        providers: [{ id: 'oidc-main', name: 'Acme SSO' }],
        bootstrap_active: bootstrapActive,
      }),
  };
}

function bootstrapSuccess() {
  return {
    ok: true,
    status: 204,
    json: () => Promise.reject(new Error('no body')),
  };
}

function bootstrapError(status: number) {
  return {
    ok: false,
    status,
    statusText: `Error ${status}`,
    json: () => Promise.resolve({ error: `HTTP ${status}` }),
  };
}

/**
 * Render WrappedApp and wait until the login card is visible
 * (i.e., after /auth/me and /auth/providers have both resolved).
 */
async function renderLoginPage(bootstrapActive: boolean) {
  mockFetch.mockImplementation((url: string) => {
    if (url === '/auth/me') return Promise.resolve(meUnauthorized());
    if (url === '/auth/providers') return Promise.resolve(providersResponse(bootstrapActive));
    return Promise.reject(new Error(`Unexpected fetch: ${url}`));
  });

  render(<WrappedApp />);

  // Wait until the OIDC provider button appears — that means providers loaded.
  await waitFor(() => {
    expect(screen.getByText('Sign in with Acme SSO')).toBeTruthy();
  });
}

// ---- Tests ----

describe('LoginPage — break-glass form visibility', () => {
  it('does NOT render the break-glass form when bootstrap_active is false', async () => {
    await renderLoginPage(false);

    expect(screen.queryByLabelText('Username')).toBeNull();
    expect(screen.queryByLabelText('Password')).toBeNull();
    expect(screen.queryByRole('button', { name: /break-glass sign in/i })).toBeNull();
    expect(screen.queryByText(/admin emergency login/i)).toBeNull();
  });

  it('renders the break-glass form when bootstrap_active is true', async () => {
    await renderLoginPage(true);

    expect(screen.getByLabelText('Username')).toBeTruthy();
    expect(screen.getByLabelText('Password')).toBeTruthy();
    expect(screen.getByRole('button', { name: /break-glass sign in/i })).toBeTruthy();
    expect(screen.getByText(/admin emergency login/i)).toBeTruthy();
  });

  it('password field has type="password"', async () => {
    await renderLoginPage(true);
    const pwInput = screen.getByLabelText('Password') as HTMLInputElement;
    expect(pwInput.type).toBe('password');
  });
});

describe('LoginPage — break-glass form submission', () => {
  async function fillAndSubmit(user: string, pass: string) {
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: user } });
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: pass } });
    fireEvent.click(screen.getByRole('button', { name: /break-glass sign in/i }));
  }

  it('redirects to /portal/ on 204 success', async () => {
    await renderLoginPage(true);

    mockFetch.mockImplementationOnce(() => Promise.resolve(bootstrapSuccess()));

    await act(async () => {
      await fillAndSubmit('admin', 'secret');
    });

    await waitFor(() => {
      expect(hrefAssigned).toBe('/portal/');
    });
  });

  it('shows "Invalid credentials" and clears password on 401', async () => {
    await renderLoginPage(true);

    mockFetch.mockImplementationOnce(() => Promise.resolve(bootstrapError(401)));

    await act(async () => {
      await fillAndSubmit('admin', 'wrongpass');
    });

    await waitFor(() => {
      expect(screen.getByText('Invalid credentials')).toBeTruthy();
    });

    const pwInput = screen.getByLabelText('Password') as HTMLInputElement;
    expect(pwInput.value).toBe('');
  });

  it('shows rate-limit message on 429', async () => {
    await renderLoginPage(true);

    mockFetch.mockImplementationOnce(() => Promise.resolve(bootstrapError(429)));

    await act(async () => {
      await fillAndSubmit('admin', 'pass');
    });

    await waitFor(() => {
      expect(
        screen.getByText(/too many attempts/i),
      ).toBeTruthy();
    });
  });

  it('hides the form on 404 (break-glass disabled mid-session)', async () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

    await renderLoginPage(true);

    mockFetch.mockImplementationOnce(() => Promise.resolve(bootstrapError(404)));

    await act(async () => {
      await fillAndSubmit('admin', 'pass');
    });

    await waitFor(() => {
      expect(screen.queryByRole('button', { name: /break-glass sign in/i })).toBeNull();
    });

    expect(warnSpy).toHaveBeenCalledWith(expect.stringContaining('bootstrap'));
    warnSpy.mockRestore();
  });

  it('shows generic error message on 500', async () => {
    await renderLoginPage(true);

    mockFetch.mockImplementationOnce(() => Promise.resolve(bootstrapError(500)));

    await act(async () => {
      await fillAndSubmit('admin', 'pass');
    });

    await waitFor(() => {
      expect(screen.getByText(/login failed/i)).toBeTruthy();
    });
  });
});
