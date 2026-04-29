import { useState, useEffect, useCallback, useRef } from 'react';
import { BrowserRouter, Routes, Route, Link, useLocation } from 'react-router-dom';
import { getMe, getProviders, bootstrapLogin, logout, setOnUnauthorized } from './api';
import type { AuthUser, AuthProvider } from './types';
import { ThemeProvider, useTheme } from './theme';
import { EventStreamProvider } from './hooks/EventStreamProvider';
import { useEventStream } from './hooks/useEventStream';
import Dashboard from './pages/user/Dashboard';
import TokenManage from './pages/user/TokenManage';
import Models from './pages/user/Models';
import UserReservations from './pages/user/Reservations';
import IdpConfig from './pages/admin/IdpConfig';
import ModelMapping from './pages/admin/ModelMapping';
import Users from './pages/admin/Users';
import System from './pages/admin/System';
import AdminReservations from './pages/admin/Reservations';
import UsageDashboard from './pages/admin/UsageDashboard';
import UserGuide from './pages/user/UserGuide';
import LoadingSpinner from './components/common/LoadingSpinner';
import ErrorAlert from './components/common/ErrorAlert';
import ThemeToggle from './components/common/ThemeToggle';

// ---- Login Page ----

function LoginPage() {
  const { colors } = useTheme();
  const [providers, setProviders] = useState<AuthProvider[]>([]);
  const [bootstrapActive, setBootstrapActive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Break-glass login state
  const [bgUser, setBgUser] = useState('');
  const [bgPass, setBgPass] = useState('');
  const [bgLoading, setBgLoading] = useState(false);
  const [bgError, setBgError] = useState<string | null>(null);
  const [bgDisabled, setBgDisabled] = useState(false);

  useEffect(() => {
    (async () => {
      try {
        const resp = await getProviders();
        setProviders(resp.providers);
        setBootstrapActive(resp.bootstrap_active);
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load login providers');
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const handleBreakGlassLogin = async (e: React.FormEvent) => {
    e.preventDefault();
    setBgLoading(true);
    setBgError(null);
    try {
      await bootstrapLogin(bgUser, bgPass);
      // 204 success — session cookie is now set, navigate to portal root
      window.location.href = '/portal/';
    } catch (err) {
      const status = (err as { status?: number }).status;
      if (status === 401) {
        setBgError('Invalid credentials');
        setBgPass('');
      } else if (status === 404) {
        // Break-glass was disabled between page load and submit
        console.warn('Break-glass login endpoint returned 404 — bootstrap mode was disabled');
        setBootstrapActive(false);
      } else if (status === 429) {
        setBgError('Too many attempts. Please wait a minute and try again.');
        setBgDisabled(true);
        setTimeout(() => setBgDisabled(false), 60_000);
      } else {
        setBgError('Login failed. Please try again.');
      }
    } finally {
      setBgLoading(false);
    }
  };

  return (
    <div style={{
      minHeight: '100vh',
      display: 'flex',
      justifyContent: 'center',
      alignItems: 'center',
      fontFamily: 'system-ui, sans-serif',
      background: colors.pageBg,
    }}>
      <div style={{
        background: colors.cardBg,
        borderRadius: 12,
        padding: '2.5rem',
        boxShadow: colors.dialogShadow,
        maxWidth: 380,
        width: '90%',
        textAlign: 'center',
      }}>
        <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: '0.5rem' }}>
          <ThemeToggle />
        </div>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', gap: '0.5rem', marginBottom: '0.5rem' }}>
          <a href="https://agentics.org.nz" target="_blank" rel="noopener noreferrer"><img src="/portal/agentics-logo_sml.webp" alt="Agentics NZ" /></a>
          <a href="https://agentics.org.nz" target="_blank" rel="noopener noreferrer" style={{ fontWeight: 400, fontSize: '18px', color: colors.textMuted, textDecoration: 'none' }}>Agentics NZ</a>
        </div>
        <h1 style={{ margin: '0 0 0.5rem', fontSize: '1.5rem', color: colors.textPrimary }}>Sovereign Engine</h1>
        <p style={{ margin: '0 0 1.5rem', color: colors.textMuted, fontSize: '0.9rem' }}>Sign in to continue</p>

        {error && <ErrorAlert message={error} />}

        {loading ? (
          <LoadingSpinner message="Loading providers..." />
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '0.75rem' }}>
            {providers.map((provider) => (
              <a
                key={provider.id}
                href={`/auth/login?idp=${encodeURIComponent(provider.id)}`}
                style={{
                  display: 'block',
                  padding: '0.75rem',
                  background: colors.buttonPrimary,
                  color: '#fff',
                  borderRadius: 6,
                  textDecoration: 'none',
                  fontWeight: 600,
                  fontSize: '0.95rem',
                }}
              >
                Sign in with {provider.name}
              </a>
            ))}

            {providers.length === 0 && !error && (
              <p style={{ color: colors.textMuted, fontSize: '0.85rem', margin: 0 }}>
                No identity providers configured.
              </p>
            )}
          </div>
        )}

        {bootstrapActive && (
          <div style={{
            marginTop: '1.5rem',
            borderTop: `1px solid ${colors.cardBorder}`,
            paddingTop: '1rem',
          }}>
            <div style={{
              background: colors.badgeWarningBg,
              border: `1px solid ${colors.warningBannerBorder}`,
              borderRadius: 6,
              padding: '0.5rem 0.75rem',
              marginBottom: '0.75rem',
              fontSize: '0.8rem',
              color: colors.warningBannerText,
              textAlign: 'left',
            }}>
              <strong>Admin emergency login</strong> — for break-glass access only. Not the normal login path.
            </div>
            <form onSubmit={handleBreakGlassLogin} style={{ textAlign: 'left' }} autoComplete="off">
              {bgError && <ErrorAlert message={bgError} />}
              <div style={{ marginBottom: '0.75rem' }}>
                <label htmlFor="bg-username" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.85rem', fontWeight: 600, color: colors.textSecondary }}>Username</label>
                <input
                  id="bg-username"
                  type="text"
                  autoComplete="off"
                  value={bgUser}
                  onChange={(e) => setBgUser(e.target.value)}
                  style={{
                    width: '100%',
                    padding: '0.5rem',
                    border: `1px solid ${colors.inputBorder}`,
                    borderRadius: 4,
                    fontSize: '0.9rem',
                    boxSizing: 'border-box',
                    background: colors.inputBg,
                    color: colors.textPrimary,
                  }}
                  required
                />
              </div>
              <div style={{ marginBottom: '0.75rem' }}>
                <label htmlFor="bg-password" style={{ display: 'block', marginBottom: '0.25rem', fontSize: '0.85rem', fontWeight: 600, color: colors.textSecondary }}>Password</label>
                <input
                  id="bg-password"
                  type="password"
                  autoComplete="off"
                  value={bgPass}
                  onChange={(e) => setBgPass(e.target.value)}
                  style={{
                    width: '100%',
                    padding: '0.5rem',
                    border: `1px solid ${colors.inputBorder}`,
                    borderRadius: 4,
                    fontSize: '0.9rem',
                    boxSizing: 'border-box',
                    background: colors.inputBg,
                    color: colors.textPrimary,
                  }}
                  required
                />
              </div>
              <button
                type="submit"
                disabled={bgLoading || bgDisabled}
                style={{
                  width: '100%',
                  padding: '0.6rem',
                  background: (bgLoading || bgDisabled) ? colors.buttonPrimaryDisabled : colors.buttonPrimary,
                  color: '#fff',
                  border: 'none',
                  borderRadius: 4,
                  cursor: (bgLoading || bgDisabled) ? 'default' : 'pointer',
                  fontSize: '0.9rem',
                }}
              >
                {bgLoading ? 'Signing in...' : 'Break-glass Sign In'}
              </button>
            </form>
          </div>
        )}
      </div>
    </div>
  );
}

// ---- Nav Link that highlights active route ----

function NavLink({ to, children }: Readonly<{ to: string; children: React.ReactNode }>) {
  const location = useLocation();
  const { colors } = useTheme();
  const isActive = location.pathname === to;
  return (
    <Link
      to={to}
      style={{
        color: isActive ? colors.navTextActive : colors.navTextInactive,
        textDecoration: 'none',
        fontSize: '0.9rem',
        borderBottom: isActive ? `2px solid ${colors.navTextActive}` : '2px solid transparent',
        paddingBottom: '0.2rem',
      }}
    >
      {children}
    </Link>
  );
}

// ---- Authenticated App Shell ----

function ReservationBanner({ userId }: Readonly<{ userId: string }>) {
  const { colors } = useTheme();
  const { snapshot } = useEventStream();
  const active = snapshot?.active_reservation;

  if (!active || active.user_id === userId) return null;

  return (
    <div style={{
      background: colors.warningBannerBg,
      border: `1px solid ${colors.warningBannerBorder}`,
      color: colors.warningBannerText,
      padding: '0.5rem 2rem',
      fontSize: '0.85rem',
      textAlign: 'center',
    }}>
      System is currently reserved for exclusive use by {active.user_display_name || 'another user'}
      {active.end_time && <span> (until {new Date(active.end_time + 'Z').toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })})</span>}
    </div>
  );
}

function GpuStatusBar() {
  const { colors } = useTheme();
  const { snapshot } = useEventStream();
  const gpus = snapshot?.gpu_memory;

  if (!gpus?.length) return null;

  return (
    <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
      {gpus.map((gpu) => {
        const usedGb = (gpu.used_mb / 1024).toFixed(1);
        const totalGb = (gpu.total_mb / 1024).toFixed(1);
        const pct = gpu.total_mb > 0 ? Math.round((gpu.used_mb / gpu.total_mb) * 100) : 0;
        let barColor = colors.successText;
        if (pct > 90) barColor = '#ef4444';
        else if (pct > 70) barColor = '#f59e0b';
        const utilSuffix = gpu.utilization_percent == null ? '' : `, ${gpu.utilization_percent}% util`;
        const gpuTitle = `${gpu.gpu_type} #${gpu.device_index} — ${usedGb}/${totalGb} GB VRAM${utilSuffix}`;
        return (
          <div
            key={`${gpu.gpu_type}-${gpu.device_index}`}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: '0.35rem',
              fontSize: '0.75rem',
              color: colors.navTextInactive,
            }}
            title={gpuTitle}
          >
            <span style={{ fontWeight: 600, color: colors.navTextInactive }}>
              {gpus.length > 1 ? `GPU${gpu.device_index}` : 'VRAM'}
            </span>
            <div style={{
              width: 48,
              height: 6,
              background: colors.navSeparator,
              borderRadius: 3,
              overflow: 'hidden',
            }}>
              <div style={{
                width: `${pct}%`,
                height: '100%',
                background: barColor,
                borderRadius: 3,
                transition: 'width 0.5s ease',
              }} />
            </div>
            <span>{usedGb}/{totalGb}G</span>
            {gpu.utilization_percent != null && (
              <span style={{ color: colors.navTextInactive, opacity: 0.7 }}>
                {gpu.utilization_percent}%
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}

const THEME_OPTIONS: { mode: import('./theme').ThemeMode; icon: string; label: string }[] = [
  { mode: 'system', icon: '\u{1F5A5}', label: 'System' },
  { mode: 'light', icon: '\u2600', label: 'Light' },
  { mode: 'dark', icon: '\u{1F319}', label: 'Dark' },
];

function ThemeSelector() {
  const { colors, mode, setMode } = useTheme();
  return (
    <div style={{ borderBottom: `1px solid ${colors.cardBorder}` }}>
      {THEME_OPTIONS.map((opt) => {
        const active = mode === opt.mode;
        return (
          <button
            key={opt.mode}
            onClick={() => setMode(opt.mode)}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: '0.4rem',
              width: '100%',
              padding: '0.4rem 0.75rem',
              background: active ? colors.buttonPrimary : 'transparent',
              color: active ? '#fff' : colors.textSecondary,
              border: 'none',
              cursor: 'pointer',
              fontSize: '0.8rem',
              fontWeight: active ? 600 : 400,
              textAlign: 'left',
            }}
          >
            <span>{opt.icon}</span>
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

function UserMenu({ user, onLogout }: Readonly<{ user: AuthUser; onLogout: () => void }>) {
  const { colors } = useTheme();
  const [open, setOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [open]);

  return (
    <div ref={menuRef} style={{ position: 'relative' }}>
      <button
        onClick={() => setOpen((v) => !v)}
        style={{
          padding: '0.3rem 0.7rem',
          background: 'transparent',
          color: colors.navTextInactive,
          border: `1px solid ${colors.navSeparator}`,
          borderRadius: 4,
          cursor: 'pointer',
          fontSize: '0.8rem',
          display: 'flex',
          alignItems: 'center',
          gap: '0.35rem',
        }}
      >
        {user.display_name || user.email || 'User'}
        <span style={{ fontSize: '0.6rem', lineHeight: 1 }}>{open ? '\u25B2' : '\u25BC'}</span>
      </button>
      {open && (
        <div
          style={{
            position: 'absolute',
            right: 0,
            top: 'calc(100% + 4px)',
            background: colors.cardBg,
            border: `1px solid ${colors.cardBorder}`,
            borderRadius: 6,
            boxShadow: colors.dialogShadow,
            minWidth: 160,
            zIndex: 100,
            overflow: 'hidden',
          }}
        >
          <div style={{ padding: '0.5rem 0.75rem', borderBottom: `1px solid ${colors.cardBorder}`, fontSize: '0.8rem', color: colors.textMuted }}>
            {user.email || user.display_name || 'User'}
            {user.is_admin && <span style={{ marginLeft: '0.35rem', fontSize: '0.7rem', color: colors.warningText }}>(admin)</span>}
          </div>
          <ThemeSelector />
          <button
            onClick={() => { setOpen(false); onLogout(); }}
            style={{
              display: 'block',
              width: '100%',
              padding: '0.5rem 0.75rem',
              background: 'none',
              border: 'none',
              textAlign: 'left',
              cursor: 'pointer',
              fontSize: '0.8rem',
              color: colors.buttonDanger,
            }}
          >
            Sign Out
          </button>
        </div>
      )}
    </div>
  );
}

function AuthenticatedApp({ user, onLogout }: Readonly<{ user: AuthUser; onLogout: () => void }>) {
  const { colors } = useTheme();

  const handleLogout = async () => {
    try {
      await logout();
    } catch {
      // Even if the API call fails, clear local state
    }
    onLogout();
  };

  return (
    <EventStreamProvider>
    <div style={{ fontFamily: 'system-ui, sans-serif', margin: 0, padding: 0, minHeight: '100vh', background: colors.pageBg, color: colors.textPrimary }}>
      <nav style={{
        background: colors.navBg,
        color: colors.navText,
        padding: '0.75rem 2rem',
        display: 'flex',
        gap: '0.5rem 1.5rem',
        alignItems: 'center',
        flexWrap: 'wrap',
      }}>
        <a href="https://agentics.org.nz" target="_blank" rel="noopener noreferrer" style={{ display: 'flex', alignItems: 'center', marginRight: '-1rem' }}>
          <img src="/portal/agentics-logo_sml.webp" alt="Agentics NZ" style={{ height: 24 }} />
        </a>
        <strong style={{ fontSize: '1.1rem', marginRight: '0.5rem' }}>Sovereign Engine</strong>
        <NavLink to="/">Dashboard</NavLink>
        <NavLink to="/tokens">Tokens</NavLink>
        <NavLink to="/models">Models</NavLink>
        <NavLink to="/reservations">Reservations</NavLink>
        <NavLink to="/guide">Guide</NavLink>
        <a
          href={user.chat_url}
          style={{
            color: colors.navTextInactive,
            textDecoration: 'none',
            fontSize: '0.9rem',
            borderBottom: '2px solid transparent',
            paddingBottom: '0.2rem',
          }}
        >Chat</a>

        <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
          <GpuStatusBar />
          <UserMenu user={user} onLogout={handleLogout} />
        </div>

        {user.is_admin && (
          <>
            <div style={{ flexBasis: '100%', height: 0, borderTop: `1px solid ${colors.navSeparator}` }} />
            <div style={{ width: 24, marginRight: '-1rem', visibility: 'hidden' }} aria-hidden="true" />
            <strong style={{ fontSize: '1.1rem', marginRight: '0.5rem', visibility: 'hidden' }} aria-hidden="true">Sovereign Engine</strong>
            <NavLink to="/admin/usage">Usage Analytics</NavLink>
            <NavLink to="/admin/idp">IdP Config</NavLink>
            <NavLink to="/admin/models">Model Mapping</NavLink>
            <NavLink to="/admin/users">Users</NavLink>
            <NavLink to="/admin/system">System</NavLink>
            <NavLink to="/admin/reservations">Manage Reservations</NavLink>
          </>
        )}
      </nav>
      <ReservationBanner userId={user.user_id} />
      <main style={{ padding: '2rem', maxWidth: 1200, margin: '0 auto' }}>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/tokens" element={<TokenManage />} />
          <Route path="/models" element={<Models />} />
          <Route path="/reservations" element={<UserReservations userId={user.user_id} />} />
          <Route path="/guide" element={<UserGuide />} />
          {user.is_admin && (
            <>
              <Route path="/admin/usage" element={<UsageDashboard />} />
              <Route path="/admin/idp" element={<IdpConfig />} />
              <Route path="/admin/models" element={<ModelMapping />} />
              <Route path="/admin/users" element={<Users />} />
              <Route path="/admin/system" element={<System />} />
              <Route path="/admin/reservations" element={<AdminReservations userId={user.user_id} />} />
            </>
          )}
        </Routes>
      </main>
    </div>
    </EventStreamProvider>
  );
}

// ---- Root App ----

function App() {
  const [user, setUser] = useState<AuthUser | null>(null);
  const [loading, setLoading] = useState(true);
  const [authError, setAuthError] = useState<string | null>(null);

  const checkAuth = useCallback(async () => {
    setLoading(true);
    setAuthError(null);
    try {
      const me = await getMe();
      setUser(me);
    } catch (err) {
      // 401 is expected when not logged in
      if (err instanceof Error && err.message === 'Unauthorized') {
        setUser(null);
      } else {
        setAuthError(err instanceof Error ? err.message : 'Failed to check authentication');
      }
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    setOnUnauthorized(() => {
      setUser(null);
    });
    checkAuth();
  }, [checkAuth]);

  if (loading) {
    return (
      <div style={{ fontFamily: 'system-ui, sans-serif' }}>
        <LoadingSpinner message="Checking authentication..." />
      </div>
    );
  }

  if (authError) {
    return (
      <div style={{ fontFamily: 'system-ui, sans-serif', padding: '2rem' }}>
        <ErrorAlert message={authError} onRetry={checkAuth} />
      </div>
    );
  }

  return (
    <BrowserRouter basename="/portal">
      {user ? (
        <AuthenticatedApp user={user} onLogout={() => setUser(null)} />
      ) : (
        <LoginPage />
      )}
    </BrowserRouter>
  );
}

export default function WrappedApp() {
  return (
    <ThemeProvider>
      <App />
    </ThemeProvider>
  );
}
