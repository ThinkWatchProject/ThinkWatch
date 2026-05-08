import { useCallback, useEffect, useRef, useState } from 'react';
import { apiDelete, apiGet } from '@/lib/api';
import type {
  PersistedWizardState,
  SharedPending,
  WizardState,
} from './types';

const STORAGE_KEY_PREFIX = 'mcp:wizard:';
const RESUME_FRAGMENT = 'wizard_resume=';

function genSessionId(): string {
  if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
    return crypto.randomUUID();
  }
  // Fallback used by older browsers in the test harness — random
  // enough for state-blob keying, the OAuth state HMAC catches
  // tampering anyway.
  return `wizard-${Math.random().toString(36).slice(2)}-${Date.now()}`;
}

function defaultState(sessionId: string): WizardState {
  return {
    wizard_session_id: sessionId,
    endpoint_url: '',
    transport_type: 'streamable_http',
    probe: null,
    oauth_probe: null,
    auth_shape: 'anonymous',
    oauth: {
      issuer: '',
      authorization_endpoint: '',
      token_endpoint: '',
      revocation_endpoint: '',
      userinfo_endpoint: '',
      client_id: '',
      client_secret: '',
      scopes: '',
    },
    static_token_help_url: '',
    auth_header_name: 'Authorization',
    auth_value_template: 'Bearer {{token}}',
    credential_owner: 'per_user',
    shared_pending: null,
    name: '',
    namespace_prefix: '',
    display_label: '',
    description: '',
    custom_headers: [],
    cache_ttl_secs: '',
    step: 1,
  };
}

function readStorage(sessionId: string): WizardState | null {
  if (typeof window === 'undefined') return null;
  const raw = window.sessionStorage.getItem(STORAGE_KEY_PREFIX + sessionId);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as PersistedWizardState;
    return { ...parsed, shared_pending: parsed.shared_pending };
  } catch {
    return null;
  }
}

function writeStorage(state: WizardState) {
  if (typeof window === 'undefined') return;
  // Scrub the in-memory plaintext token before persisting — losing
  // the tab kills the token, but disk never sees it.
  const persisted: PersistedWizardState = {
    ...state,
    shared_pending:
      state.shared_pending?.kind === 'oauth_done'
        ? state.shared_pending
        : null,
  };
  window.sessionStorage.setItem(
    STORAGE_KEY_PREFIX + state.wizard_session_id,
    JSON.stringify(persisted),
  );
}

function clearStorage(sessionId: string) {
  if (typeof window === 'undefined') return;
  window.sessionStorage.removeItem(STORAGE_KEY_PREFIX + sessionId);
}

function readResumeIdFromHash(): string | null {
  if (typeof window === 'undefined') return null;
  const hash = window.location.hash.replace(/^#/, '');
  if (!hash.startsWith(RESUME_FRAGMENT)) return null;
  const id = decodeURIComponent(hash.slice(RESUME_FRAGMENT.length));
  return id || null;
}

interface WizardController {
  state: WizardState;
  /** Patch one or more top-level fields of the wizard state. */
  patch: (partial: Partial<WizardState>) => void;
  /** Like `patch`, but for nested OAuth fields. */
  patchOAuth: (partial: Partial<WizardState['oauth']>) => void;
  /** Move to the given step. */
  goToStep: (step: 1 | 2 | 3 | 4) => void;
  /** Wipe sessionStorage + any pending Redis credential. */
  reset: () => Promise<void>;
  /** Whether this wizard came back from an OAuth callback. */
  resumed: boolean;
  /** True while we're awaiting the OAuth-resume credential probe. */
  resumeChecking: boolean;
}

/**
 * Hook owning the registration wizard's state machine. State is
 * mirrored to `sessionStorage` so the OAuth admin_shared redirect
 * cycle (leave page → upstream auth → callback → land back on
 * `/mcp/servers/new#wizard_resume=…`) doesn't lose what the admin
 * filled in.
 *
 * Resume contract:
 *   1. URL hash carries `#wizard_resume={session_id}` after the
 *      callback redirect.
 *   2. The hook reads sessionStorage for that session_id and rehydrates.
 *   3. If `auth_shape='oauth'` and `credential_owner='admin_shared'`,
 *      we GET `/api/admin/mcp/wizards/{id}/credential-status` to
 *      confirm the Redis blob landed; on 200 we set
 *      `shared_pending = { kind: 'oauth_done', ... }`.
 *   4. The wizard renders Step 3 with a green confirmation card and
 *      lets the admin click forward to Step 4.
 */
export function useWizardState(): WizardController {
  // Source the session_id once: from URL hash (resume), from
  // sessionStorage's most-recent (rare, e.g. browser back), or fresh.
  const sessionIdRef = useRef<string>('');
  const resumedRef = useRef<boolean>(false);
  if (!sessionIdRef.current) {
    const fromHash = readResumeIdFromHash();
    if (fromHash) {
      sessionIdRef.current = fromHash;
      resumedRef.current = true;
    } else {
      sessionIdRef.current = genSessionId();
    }
  }

  const [state, setState] = useState<WizardState>(() => {
    if (resumedRef.current) {
      const restored = readStorage(sessionIdRef.current);
      if (restored) return restored;
      // Resume marker pointed at a session we don't have locally — rare
      // (private window, cleared sessionStorage). Fall through to a
      // fresh wizard and let the admin start over.
      resumedRef.current = false;
    }
    return defaultState(sessionIdRef.current);
  });

  const [resumeChecking, setResumeChecking] = useState<boolean>(resumedRef.current);

  // Strip the resume fragment from the URL so a refresh doesn't re-fire
  // the resume logic. We keep the session_id alive in React state and
  // sessionStorage, neither of which depends on the URL anymore.
  useEffect(() => {
    if (!resumedRef.current) return;
    if (typeof window !== 'undefined') {
      window.history.replaceState(null, '', window.location.pathname);
    }
  }, []);

  // Resume probe — confirm the OAuth dance landed a credential blob.
  useEffect(() => {
    if (!resumedRef.current) return;
    if (state.credential_owner !== 'admin_shared') {
      setResumeChecking(false);
      return;
    }
    let alive = true;
    (async () => {
      try {
        const status = await apiGet<{
          credential_type: string;
          upstream_subject?: string | null;
          expires_at?: string | null;
          scopes: string[];
        }>(
          `/api/admin/mcp/wizards/${encodeURIComponent(
            sessionIdRef.current,
          )}/credential-status`,
        );
        if (!alive) return;
        const sharedPending: SharedPending = {
          kind: 'oauth_done',
          upstream_subject: status.upstream_subject ?? null,
          expires_at: status.expires_at ?? null,
          scopes: status.scopes ?? [],
        };
        setState((s) => ({ ...s, shared_pending: sharedPending, step: 3 }));
      } catch {
        // 404 = blob not there (TTL expired, or callback hasn't run).
        // Leave shared_pending null; Step 3 will show "OAuth flow
        // didn't complete — re-run authorize".
      } finally {
        if (alive) setResumeChecking(false);
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Persist on every mutation.
  useEffect(() => {
    writeStorage(state);
  }, [state]);

  const patch = useCallback((partial: Partial<WizardState>) => {
    setState((s) => ({ ...s, ...partial }));
  }, []);

  const patchOAuth = useCallback((partial: Partial<WizardState['oauth']>) => {
    setState((s) => ({ ...s, oauth: { ...s.oauth, ...partial } }));
  }, []);

  const goToStep = useCallback((step: 1 | 2 | 3 | 4) => {
    setState((s) => ({ ...s, step }));
  }, []);

  const reset = useCallback(async () => {
    const sid = sessionIdRef.current;
    clearStorage(sid);
    // Best-effort — discard the Redis blob if any. Failure is fine,
    // it'll TTL out within an hour.
    try {
      await apiDelete(`/api/admin/mcp/wizards/${encodeURIComponent(sid)}/credential-status`);
    } catch {
      /* ignore */
    }
  }, []);

  return {
    state,
    patch,
    patchOAuth,
    goToStep,
    reset,
    resumed: resumedRef.current,
    resumeChecking,
  };
}
