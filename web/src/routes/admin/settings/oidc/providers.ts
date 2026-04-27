/// Catalog of common OIDC providers. The wizard uses this to:
///   1. pre-fill the issuer URL (or template) so the admin doesn't
///      have to look it up;
///   2. pre-fill the claim mapping for providers known to deviate
///      from the standard `email` claim;
///   3. surface a "where do I find these in <provider>" deep link
///      next to the credentials step.
///
/// `Generic` is the catch-all — no presets, admin fills everything.

export type ProviderId =
  | 'google'
  | 'microsoft_entra'
  | 'auth0'
  | 'okta'
  | 'keycloak'
  | 'zitadel'
  | 'generic';

export interface ProviderPreset {
  id: ProviderId;
  /// i18n key for the provider's display name. Falls back to a
  /// hardcoded label if the key isn't translated.
  labelKey: string;
  /// Concrete issuer URL when the provider has one (Google), an
  /// empty string when the admin has to fill in their tenant ID.
  defaultIssuer: string;
  /// Placeholder shown in the issuer field when `defaultIssuer` is
  /// empty — usually the template with a `{tenant}` slot.
  issuerPlaceholder: string;
  /// JWT claim that carries the user's email at this provider. Some
  /// providers (Microsoft Entra without the `email` scope) don't
  /// populate `email` and we have to fall back to a different field.
  defaultEmailClaim: string;
  /// JWT claim that carries the display name.
  defaultNameClaim: string;
  /// Where the admin finds client_id / client_secret in this
  /// provider's UI. i18n key. Empty for `generic`.
  credentialsHintKey: string;
  /// Documentation deep link to the provider's "create OIDC app"
  /// page. Optional.
  docsUrl?: string;
}

export const PROVIDER_CATALOG: ProviderPreset[] = [
  {
    id: 'google',
    labelKey: 'settings.oidc.providers.google',
    defaultIssuer: 'https://accounts.google.com',
    issuerPlaceholder: 'https://accounts.google.com',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.google_hint',
    docsUrl: 'https://console.cloud.google.com/apis/credentials',
  },
  {
    id: 'microsoft_entra',
    labelKey: 'settings.oidc.providers.microsoft_entra',
    defaultIssuer: '',
    issuerPlaceholder: 'https://login.microsoftonline.com/{tenant_id}/v2.0',
    /// Entra without the `email` scope leaves `email` blank but
    /// reliably populates `preferred_username` with the UPN.
    defaultEmailClaim: 'preferred_username',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.microsoft_entra_hint',
    docsUrl: 'https://entra.microsoft.com/',
  },
  {
    id: 'auth0',
    labelKey: 'settings.oidc.providers.auth0',
    defaultIssuer: '',
    issuerPlaceholder: 'https://{your-tenant}.auth0.com/',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.auth0_hint',
    docsUrl: 'https://manage.auth0.com/',
  },
  {
    id: 'okta',
    labelKey: 'settings.oidc.providers.okta',
    defaultIssuer: '',
    issuerPlaceholder: 'https://{your-org}.okta.com/oauth2/default',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.okta_hint',
    docsUrl: 'https://developer.okta.com/docs/guides/find-your-domain/',
  },
  {
    id: 'keycloak',
    labelKey: 'settings.oidc.providers.keycloak',
    defaultIssuer: '',
    issuerPlaceholder: 'https://{host}/realms/{realm}',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.keycloak_hint',
    docsUrl: 'https://www.keycloak.org/docs-api/latest/rest-api/',
  },
  {
    id: 'zitadel',
    labelKey: 'settings.oidc.providers.zitadel',
    defaultIssuer: '',
    issuerPlaceholder: 'https://{your-instance}.zitadel.cloud',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: 'settings.oidc.providers.zitadel_hint',
    docsUrl: 'https://zitadel.com/docs',
  },
  {
    id: 'generic',
    labelKey: 'settings.oidc.providers.generic',
    defaultIssuer: '',
    issuerPlaceholder: 'https://idp.example.com/',
    defaultEmailClaim: 'email',
    defaultNameClaim: 'name',
    credentialsHintKey: '',
  },
];

export function findPreset(id: string | null | undefined): ProviderPreset | undefined {
  if (!id) return undefined;
  return PROVIDER_CATALOG.find((p) => p.id === id);
}
