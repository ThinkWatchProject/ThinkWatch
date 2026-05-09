import { useCallback, useEffect, useRef, useState } from 'react';
import { apiDelete, apiGet } from '@/lib/api';
import type {
  PersistedWizardState,
  SharedPending,
  WizardState,
} from './types';

const STORAGE_KEY_PREFIX = 'mcp:wizard:';
const RESUME_FRAGMENT = 'wizard_resume=';
const TEMPLATE_QUERY_PARAM = 'template';

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

function readTemplateSlugFromQuery(): string | null {
  if (typeof window === 'undefined') return null;
  const params = new URLSearchParams(window.location.search);
  const slug = params.get(TEMPLATE_QUERY_PARAM);
  return slug && slug.trim() ? slug.trim() : null;
}

/** Subset of `McpStoreTemplate` the wizard actually consumes for prefill. */
interface StoreTemplateForWizard {
  slug: string;
  name: string;
  endpoint_template?: string | null;
  oauth_issuer?: string | null;
  oauth_authorization_endpoint?: string | null;
  oauth_token_endpoint?: string | null;
  oauth_revocation_endpoint?: string | null;
  oauth_userinfo_endpoint?: string | null;
  oauth_default_scopes?: string[];
  auth_shape: string;
  static_token_help_url?: string | null;
  auth_header_name?: string | null;
  auth_value_template?: string | null;
}

/** Apply a freshly-fetched template's defaults onto a fresh wizard
 *  state. Only used for the first-mount prefill — subsequent edits
 *  live in sessionStorage and are never re-applied.
 *
 *  Important: many seeded templates store empty-string `''` for
 *  optional URL fields (self-deploy templates leave `endpoint_template`
 *  blank intentionally — admin pastes their own). `??` lets `''`
 *  through as a "real" value and overwrites the wizard's default. We
 *  use `pickFilled` to coalesce empty + null into the base. */
function pickFilled<T extends string | undefined | null>(
  fromTemplate: T,
  fallback: string,
): string {
  return fromTemplate && fromTemplate.length > 0 ? fromTemplate : fallback;
}

function applyTemplateDefaults(
  base: WizardState,
  tmpl: StoreTemplateForWizard,
): WizardState {
  const shape = (tmpl.auth_shape || 'anonymous') as WizardState['auth_shape'];
  return {
    ...base,
    template_slug: tmpl.slug,
    template_name: tmpl.name,
    endpoint_url: pickFilled(tmpl.endpoint_template, base.endpoint_url),
    auth_shape: shape,
    static_token_help_url: pickFilled(
      tmpl.static_token_help_url,
      base.static_token_help_url,
    ),
    auth_header_name: pickFilled(tmpl.auth_header_name, base.auth_header_name),
    auth_value_template: pickFilled(
      tmpl.auth_value_template,
      base.auth_value_template,
    ),
    oauth: {
      ...base.oauth,
      issuer: pickFilled(tmpl.oauth_issuer, base.oauth.issuer),
      authorization_endpoint: pickFilled(
        tmpl.oauth_authorization_endpoint,
        base.oauth.authorization_endpoint,
      ),
      token_endpoint: pickFilled(
        tmpl.oauth_token_endpoint,
        base.oauth.token_endpoint,
      ),
      revocation_endpoint: pickFilled(
        tmpl.oauth_revocation_endpoint,
        base.oauth.revocation_endpoint,
      ),
      userinfo_endpoint: pickFilled(
        tmpl.oauth_userinfo_endpoint,
        base.oauth.userinfo_endpoint,
      ),
      scopes: (tmpl.oauth_default_scopes ?? []).join(' ') || base.oauth.scopes,
    },
    // Pre-set the metadata defaults from the template name so Step 4
    // already shows something sensible. Admin can edit before submit.
    name: base.name || tmpl.name,
  };
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
  /** True while the `?template=<slug>` prefill fetch is in flight. */
  templateLoading: boolean;
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
  // Template prefill from `?template=<slug>` only fires on the very
  // first mount of a fresh session. Resumes (which already have a
  // sessionStorage blob carrying `template_slug`) skip the fetch.
  const initialTemplateSlugRef = useRef<string | null>(null);
  if (!sessionIdRef.current) {
    const fromHash = readResumeIdFromHash();
    if (fromHash) {
      sessionIdRef.current = fromHash;
      resumedRef.current = true;
    } else {
      sessionIdRef.current = genSessionId();
      initialTemplateSlugRef.current = readTemplateSlugFromQuery();
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
  // Template fetch is deferred to a useEffect (network call) — track
  // the in-flight state so Step 1 can show a spinner instead of
  // letting the admin type into a URL field that's about to be
  // overwritten by the template's `endpoint_template`.
  const [templateLoading, setTemplateLoading] = useState<boolean>(
    initialTemplateSlugRef.current !== null,
  );

  // Strip the resume fragment / `?template=` from the URL so a refresh
  // doesn't re-fire the prefill logic and doesn't re-add the same
  // template to a wizard the admin has since edited away from. The
  // session_id stays alive in React state + sessionStorage, neither of
  // which depends on the URL anymore.
  useEffect(() => {
    if (!resumedRef.current && initialTemplateSlugRef.current === null) return;
    if (typeof window !== 'undefined') {
      window.history.replaceState(null, '', window.location.pathname);
    }
  }, []);

  // Template prefill — fetch the template by slug and apply its
  // defaults onto the wizard state. Runs once on mount when the wizard
  // was opened from `/mcp/store` via `/mcp/servers/new?template=...`.
  useEffect(() => {
    const slug = initialTemplateSlugRef.current;
    if (!slug) return;
    let alive = true;
    (async () => {
      try {
        const tmpl = await apiGet<StoreTemplateForWizard>(
          `/api/mcp/store/${encodeURIComponent(slug)}`,
        );
        if (!alive) return;
        setState((s) => applyTemplateDefaults(s, tmpl));
      } catch (err) {
        // Slug doesn't exist (404) or backend hiccup — leave the
        // wizard in its empty default state. The admin can still
        // register a server manually; we just can't claim it came
        // from this template. Log to console so a misrouted slug or
        // backend regression isn't entirely silent in DevTools.
        // eslint-disable-next-line no-console
        console.warn(
          `[wizard] template prefill failed for slug=${slug}:`,
          err,
        );
      } finally {
        if (alive) setTemplateLoading(false);
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
    templateLoading,
  };
}
