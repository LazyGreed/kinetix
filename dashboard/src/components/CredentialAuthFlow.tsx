import { useEffect, useRef, useState } from 'react';
import { LogIn } from 'lucide-react';
import { Kinetix } from '../lib/resources';
import { SketchButton, WobblyCard } from './HandDrawnElements';

export interface AuthEnrollmentStart {
  authorize_url: string;
  redirect_uri: string;
  state: string;
  expires_in_secs: number;
  manual_callback_supported: boolean;
}

interface AuthSession {
  authorizeUrl: string;
  callbackUrl: string;
  providerId: string;
  state: string;
  redirectUri: string;
  showFallback: boolean;
}

interface UseAuthEnrollmentOptions {
  onSuccess: (providerId: string) => void | Promise<void>;
  onError: (message: string) => void;
}

export function useAuthEnrollment({ onSuccess, onError }: UseAuthEnrollmentOptions) {
  const [session, setSession] = useState<AuthSession | null>(null);
  const [busy, setBusy] = useState(false);
  const onSuccessRef = useRef(onSuccess);
  const onErrorRef = useRef(onError);
  onSuccessRef.current = onSuccess;
  onErrorRef.current = onError;

  const begin = async (
    providerId: string,
    start: () => Promise<AuthEnrollmentStart>,
  ) => {
    setBusy(true);
    try {
      const started = await start();
      if (started.manual_callback_supported) {
        setSession({
          authorizeUrl: started.authorize_url,
          callbackUrl: '',
          providerId,
          state: started.state,
          redirectUri: started.redirect_uri,
          showFallback: false,
        });
        window.open(started.authorize_url, '_blank', 'noopener,noreferrer');
        setBusy(false);
      } else {
        window.location.assign(started.authorize_url);
      }
    } catch (error) {
      onErrorRef.current(error instanceof Error ? error.message : String(error));
      setBusy(false);
    }
  };

  useEffect(() => {
    if (!session) return;

    let cancelled = false;
    const poll = async () => {
      try {
        const status = await Kinetix.pluginAuthStatus(session.state);
        if (cancelled || status.result === 'pending') return;
        if (status.result === 'success') {
          const providerId = session.providerId;
          setSession(null);
          await onSuccessRef.current(providerId);
          return;
        }
        setSession(null);
        onErrorRef.current(
          status.result === 'cancelled'
            ? 'Account authorization was cancelled.'
            : status.result === 'binding_changed'
              ? 'Provider binding changed during authorization.'
              : 'Account authorization failed during token exchange.',
        );
      } catch {
        // The callback can briefly move between the one-time session and
        // completion ledger. Keep polling until it resolves.
      }
    };

    void poll();
    const timer = window.setInterval(() => void poll(), 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [session?.state, session?.providerId]);

  const completeManual = async () => {
    if (!session?.callbackUrl.trim()) return;
    setBusy(true);
    try {
      const result = await Kinetix.completePluginAuth(session.callbackUrl.trim());
      if (!result.ok || result.result !== 'success') {
        throw new Error(
          result.result === 'cancelled'
            ? 'Account authorization was cancelled.'
            : result.result === 'binding_changed'
              ? 'Provider binding changed during authorization.'
              : 'Account authorization failed during token exchange.',
        );
      }
      const providerId = session.providerId;
      setSession(null);
      await onSuccessRef.current(providerId);
    } catch (error) {
      onErrorRef.current(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  const modal = session ? (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/45 backdrop-blur-xs"
      role="dialog"
      aria-modal="true"
      aria-labelledby="credential-auth-waiting-title"
    >
      <div className="w-full max-w-2xl max-h-[90vh] overflow-y-auto">
        <WobblyCard decoration="tape" className="p-5 bg-[var(--paper)] relative">
          <button
            type="button"
            onClick={() => setSession(null)}
            disabled={busy}
            className="absolute top-3 right-3 w-8 h-8 flex items-center justify-center border-2 border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--tint-red)] font-heading font-bold cursor-pointer disabled:opacity-50"
            aria-label="Cancel account authorization"
            title="Cancel"
          >
            ✕
          </button>

          <h3 id="credential-auth-waiting-title" className="text-xl font-heading font-bold flex items-center gap-2 pr-10">
            <LogIn className="w-5 h-5 text-[var(--pen-blue)]" />
            Waiting for account authorization
          </h3>
          <p className="mt-2 text-sm font-body text-[var(--ink)]/80">
            Complete authorization in the tab that was opened. Kinetix checks the authorization status automatically.
          </p>

          <div className="mt-3 flex gap-2 flex-wrap">
            <SketchButton
              variant="secondary"
              onClick={() => window.open(session.authorizeUrl, '_blank', 'noopener,noreferrer')}
              disabled={busy}
            >
              Reopen authorization
            </SketchButton>
            <SketchButton
              variant="secondary"
              onClick={() =>
                setSession((current) =>
                  current ? { ...current, showFallback: !current.showFallback } : current,
                )
              }
              disabled={busy}
            >
              {session.showFallback ? 'Hide manual fallback' : 'Loopback did not load?'}
            </SketchButton>
            <SketchButton variant="secondary" onClick={() => setSession(null)} disabled={busy}>
              Cancel
            </SketchButton>
          </div>

          {session.showFallback && (
            <div className="mt-4 space-y-2">
              <p className="text-xs font-body text-[var(--ink)]/70">
                Paste the final callback URL from the browser address bar. It must match the redirect URI for this session.
              </p>
              <code className="block text-xs break-all bg-[var(--erased)] px-2 py-1">
                {session.redirectUri}
              </code>
              <textarea
                rows={3}
                value={session.callbackUrl}
                onChange={(event) =>
                  setSession((current) =>
                    current ? { ...current, callbackUrl: event.target.value } : current,
                  )
                }
                placeholder="Paste callback URL"
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-xs"
              />
              <SketchButton
                variant="primary"
                onClick={() => void completeManual()}
                disabled={busy || !session.callbackUrl.trim()}
              >
                {busy ? 'Completing…' : 'Complete authorization'}
              </SketchButton>
            </div>
          )}
        </WobblyCard>
      </div>
    </div>
  ) : null;

  return { begin, busy, modal };
}
