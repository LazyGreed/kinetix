import React, { useEffect, useState } from 'react';
import { Settings, ShieldCheck, KeyRound, LogOut, Info, Globe2, Save } from 'lucide-react';
import { WobblyCard, SketchBadge, SketchButton } from '../HandDrawnElements';
import { Kinetix } from '../../lib/resources';

interface SettingsViewProps {
  onLogout?: () => void;
}

/**
 * Settings & Security. The dashboard password is stored server-side as a hash
 * (never in the browser); changing it invalidates every existing session,
 * including the one making the change, so the operator is signed out and must
 * log in again.
 */
export const SettingsView: React.FC<SettingsViewProps> = ({ onLogout }) => {
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirmPw, setConfirmPw] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const [publicBaseUrl, setPublicBaseUrl] = useState('');
  const [publicBaseSource, setPublicBaseSource] = useState<'dashboard' | 'environment'>('environment');
  const [environmentDefault, setEnvironmentDefault] = useState('');
  const [publicBaseBusy, setPublicBaseBusy] = useState(false);
  const [publicBaseError, setPublicBaseError] = useState<string | null>(null);
  const [publicBaseDone, setPublicBaseDone] = useState(false);

  useEffect(() => {
    let cancelled = false;
    Kinetix.publicBaseUrl()
      .then((result) => {
        if (cancelled) return;
        setPublicBaseUrl(result.public_base_url);
        setPublicBaseSource(result.source);
        setEnvironmentDefault(result.environment_default);
      })
      .catch((err) => {
        if (!cancelled) {
          setPublicBaseError(err instanceof Error ? err.message : String(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setDone(false);
    if (next.length < 8) {
      setError('New password must be at least 8 characters.');
      return;
    }
    if (next !== confirmPw) {
      setError('New password and confirmation do not match.');
      return;
    }
    setBusy(true);
    try {
      await Kinetix.changePassword(current, next);
      setDone(true);
      setCurrent('');
      setNext('');
      setConfirmPw('');
      // All sessions (including this one) are now invalid; return to login.
      setTimeout(() => onLogout?.(), 1500);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const savePublicBaseUrl = async (e: React.FormEvent) => {
    e.preventDefault();
    setPublicBaseError(null);
    setPublicBaseDone(false);
    setPublicBaseBusy(true);
    try {
      const result = await Kinetix.updatePublicBaseUrl(publicBaseUrl);
      setPublicBaseUrl(result.public_base_url);
      setPublicBaseSource(result.source);
      setPublicBaseDone(true);
    } catch (err) {
      setPublicBaseError(err instanceof Error ? err.message : String(err));
    } finally {
      setPublicBaseBusy(false);
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
          <Settings className="w-7 h-7 text-[var(--pen-blue)]" />
          <span>Settings &amp; Security</span>
          <SketchBadge variant="blue" rotation="1deg">
            Control Plane
          </SketchBadge>
        </h2>
        <p className="text-base font-body text-[var(--ink)]/80">
          Runtime network identity, session handling, and the administrator password. Changes to the public base URL
          take effect immediately for public-origin callbacks; desktop OAuth callbacks remain bound to the local listener.
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <WobblyCard decoration="tape" className="p-6">
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
            <Globe2 className="w-6 h-6 text-[var(--pen-blue)]" />
            Public Base URL
          </h3>

          <form onSubmit={savePublicBaseUrl} className="space-y-4">
            <div>
              <label className="block text-sm font-heading font-bold mb-1">Externally visible Kinetix URL</label>
              <input
                type="url"
                value={publicBaseUrl}
                onChange={(e) => setPublicBaseUrl(e.target.value)}
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: '12px 16px 12px 16px / 16px 12px 16px 12px' }}
                placeholder="https://kinetix.example.com"
                autoComplete="url"
              />
              <p className="mt-2 text-xs font-body text-[var(--ink)]/70">
                Used for externally visible links and web-style OAuth callbacks. Claude Code and Antigravity use
                loopback callbacks derived from <span className="font-mono">KINETIX_BIND</span> instead.
              </p>
            </div>

            <div className="text-xs font-mono text-[var(--ink)]/70">
              Active source: <strong>{publicBaseSource === 'dashboard' ? 'dashboard setting' : 'environment/default'}</strong>
              {environmentDefault && (
                <div className="mt-1 break-all">Startup default: {environmentDefault}</div>
              )}
            </div>

            {publicBaseError && (
              <div className="p-2 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] rounded text-sm font-mono text-[var(--danger-text)]">
                {publicBaseError}
              </div>
            )}
            {publicBaseDone && (
              <div className="p-2 bg-[var(--tint-green)] border-2 border-[var(--pen-green)] rounded text-sm font-mono text-[var(--success-text)]">
                Public base URL updated. No restart required.
              </div>
            )}

            <SketchButton type="submit" variant="primary" disabled={publicBaseBusy || !publicBaseUrl.trim()} className="gap-2">
              <Save className="w-4 h-4" />
              {publicBaseBusy ? 'Saving…' : 'Save Public URL'}
            </SketchButton>
          </form>
        </WobblyCard>

        <WobblyCard decoration="tack" className="p-6">
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
            <KeyRound className="w-6 h-6 text-[var(--marker-red)]" />
            Change Administrator Password
          </h3>

          <form onSubmit={submit} className="space-y-4">
            <div>
              <label className="block text-sm font-heading font-bold mb-1">Current Password</label>
              <input
                type="password"
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: '12px 16px 12px 16px / 16px 12px 16px 12px' }}
                autoComplete="current-password"
              />
            </div>
            <div>
              <label className="block text-sm font-heading font-bold mb-1">New Password (min 8 chars)</label>
              <input
                type="password"
                value={next}
                onChange={(e) => setNext(e.target.value)}
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: '12px 16px 12px 16px / 16px 12px 16px 12px' }}
                autoComplete="new-password"
              />
            </div>
            <div>
              <label className="block text-sm font-heading font-bold mb-1">Confirm New Password</label>
              <input
                type="password"
                value={confirmPw}
                onChange={(e) => setConfirmPw(e.target.value)}
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: '12px 16px 12px 16px / 16px 12px 16px 12px' }}
                autoComplete="new-password"
              />
            </div>

            {error && (
              <div className="p-2 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] rounded text-sm font-mono text-[var(--danger-text)]">
                {error}
              </div>
            )}
            {done && (
              <div className="p-2 bg-[var(--tint-green)] border-2 border-[var(--pen-green)] rounded text-sm font-mono text-[var(--success-text)]">
                Password changed. All sessions invalidated — signing you out…
              </div>
            )}

            <SketchButton type="submit" variant="primary" disabled={busy || !current || !next} className="gap-2">
              <ShieldCheck className="w-4 h-4" />
              {busy ? 'Updating…' : 'Update Password'}
            </SketchButton>
          </form>
        </WobblyCard>

        <div className="space-y-6">
          <WobblyCard variant="muted" className="p-5">
            <h4 className="text-lg font-heading font-bold mb-2 flex items-center gap-2">
              <Info className="w-5 h-5 text-[var(--pen-blue)]" /> Session Policy
            </h4>
            <ul className="list-disc list-inside space-y-1.5 text-sm font-body text-[var(--ink)]/85">
              <li>Sessions are held in server memory with a TTL (default 12 h).</li>
              <li>
                <strong>Restarting the server invalidates every session</strong>, so a browser must log in again — a
                stale cookie can never grant access.
              </li>
              <li>Changing the password invalidates all sessions immediately.</li>
              <li>The password is stored only as a hash, never in the browser.</li>
            </ul>
          </WobblyCard>

          <WobblyCard decoration="tape" className="p-5">
            <h4 className="text-lg font-heading font-bold mb-2">CLI Equivalent</h4>
            <p className="text-sm font-body text-[var(--ink)]/85 mb-2">
              The same change can be made without the dashboard (works while the server is stopped):
            </p>
            <pre className="bg-[var(--code-bg)] text-[var(--code-fg)] text-xs font-mono p-3 rounded overflow-x-auto">
{`kinetix password set 'a-new-strong-password'
kinetix password show`}
            </pre>
          </WobblyCard>

          {onLogout && (
            <SketchButton variant="danger" onClick={onLogout} className="gap-2">
              <LogOut className="w-4 h-4" /> Sign Out Now
            </SketchButton>
          )}
        </div>
      </div>
    </div>
  );
};
