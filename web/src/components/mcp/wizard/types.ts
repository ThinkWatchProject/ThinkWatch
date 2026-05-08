/**
 * Shape of the new-server registration wizard's persistent state.
 *
 * Backed by sessionStorage under `mcp:wizard:{wizard_session_id}` so a
 * full page reload (the OAuth admin_shared dance, in particular)
 * survives without the admin re-typing every field. The static-token
 * paste case keeps the *plaintext token* in React state ONLY — never
 * in sessionStorage — to avoid leaving plaintext secrets on disk if
 * the tab is closed mid-wizard.
 */

/**
 * Auth shape per server is single-valued: each server is exactly one
 * of these. There is intentionally NO "OAuth + PAT" combined option
 * — OAuth and PAT are different authentication shapes, not stages of
 * a fallback chain. If the upstream advertises both, pick whichever
 * matches the role this server plays for your users; admins who
 * really need both can register the same upstream URL twice with
 * different namespace prefixes.
 *
 * "Who supplies the credential?" lives on a separate axis
 * (`CredentialOwner` — user-configured vs admin-preset). Conflating
 * those two axes was the mistake the old wizard's "OAuth + PAT
 * fallback" radio made.
 */
export type AuthShape = 'anonymous' | 'oauth' | 'static';

export type CredentialOwner = 'per_user' | 'admin_shared';

export interface ToolPreview {
  name: string;
  description?: string | null;
}

export interface ProbeResult {
  /** Anonymous tools/list returned 200 ⇒ no credential required. */
  anonymous_ok: boolean;
  /** Anonymous tools/list returned 401/403 ⇒ credential required. */
  requires_auth: boolean;
  /** Detected transport (always streamable_http for now). */
  transport_type: string;
  /** Tools the anonymous probe enumerated, if any. Empty when auth-gated. */
  tools: ToolPreview[];
  /** Round-trip latency for the probe, ms. */
  latency_ms: number;
  /** Free-form server message ("OK 200 / 1 tool", "401 — auth required", …). */
  message: string;
}

export interface OAuthProbeResult {
  /** Discovered RFC 8414 issuer URL, if any. */
  issuer: string | null;
  /** Whether the AS advertises public-client (PKCE-only) support. */
  public_client: boolean;
  /** Pre-computed OAuth callback URL the admin must paste into the
   *  upstream's OAuth-app config when registering manually. */
  redirect_uri: string;
  /** Discovered endpoints (filled when issuer probe succeeded). */
  authorization_endpoint?: string | null;
  token_endpoint?: string | null;
  revocation_endpoint?: string | null;
  userinfo_endpoint?: string | null;
  /** Default scopes from issuer metadata, if any. */
  default_scopes?: string[];
  /** Dynamically-registered client_id, when DCR worked. */
  client_id?: string | null;
  /** Dynamically-registered client_secret, when DCR worked. Plaintext — only valid for the lifetime of this wizard. */
  client_secret?: string | null;
}

export interface OAuthFields {
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
  revocation_endpoint: string;
  userinfo_endpoint: string;
  client_id: string;
  /** Plaintext on the wire when "Save" fires; the backend encrypts at rest. */
  client_secret: string;
  scopes: string;
}

/** Step 3's "ready to save" state for admin_shared. The plaintext
 *  static token is kept here in memory ONLY — see the file-level note. */
export type SharedPending =
  | { kind: 'oauth_done'; upstream_subject?: string | null; expires_at?: string | null; scopes: string[] }
  | { kind: 'static_paste'; token: string };

export interface WizardState {
  wizard_session_id: string;

  // Step 1 — URL & probe
  endpoint_url: string;
  transport_type: string;
  probe: ProbeResult | null;
  oauth_probe: OAuthProbeResult | null;

  // Step 2 — Auth shape & header injection
  auth_shape: AuthShape;
  oauth: OAuthFields;
  static_token_help_url: string;
  auth_header_name: string;
  auth_value_template: string;

  // Step 3 — Credential ownership
  credential_owner: CredentialOwner;
  /** Set when the admin completed the OAuth dance (resume from
   *  callback) or pasted a token. NOT persisted to sessionStorage
   *  for the static-paste variant. */
  shared_pending: SharedPending | null;

  // Step 4 — Metadata
  name: string;
  namespace_prefix: string;
  display_label: string;
  description: string;
  custom_headers: [string, string][];
  cache_ttl_secs: string;

  // Wizard control
  step: 1 | 2 | 3 | 4;
}

/** What the controller persists to sessionStorage. Static-paste
 *  tokens are scrubbed on serialize; resumed wizards start back at
 *  Step 3 and ask the admin to re-paste. */
export type PersistedWizardState = Omit<WizardState, 'shared_pending'> & {
  shared_pending:
    | { kind: 'oauth_done'; upstream_subject?: string | null; expires_at?: string | null; scopes: string[] }
    | null;
};
