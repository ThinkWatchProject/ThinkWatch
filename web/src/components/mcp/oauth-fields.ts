// The shape and its pure transforms, split out of `oauth-fieldset.tsx` so
// that file exports only the component — Fast Refresh needs the separation.

export interface OAuthFields {
  issuer: string;
  authorizationEndpoint: string;
  tokenEndpoint: string;
  revocationEndpoint: string;
  userinfoEndpoint: string;
  clientId: string;
  clientSecret: string;
  scopes: string;
}

export const emptyOAuth = (): OAuthFields => ({
  issuer: '',
  authorizationEndpoint: '',
  tokenEndpoint: '',
  revocationEndpoint: '',
  userinfoEndpoint: '',
  clientId: '',
  clientSecret: '',
  scopes: '',
});

export function oauthFromServer(s: {
  oauth_issuer: string | null;
  oauth_authorization_endpoint: string | null;
  oauth_token_endpoint: string | null;
  oauth_revocation_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  oauth_client_id: string | null;
  oauth_scopes: string[];
}): OAuthFields {
  return {
    issuer: s.oauth_issuer ?? '',
    authorizationEndpoint: s.oauth_authorization_endpoint ?? '',
    tokenEndpoint: s.oauth_token_endpoint ?? '',
    revocationEndpoint: s.oauth_revocation_endpoint ?? '',
    userinfoEndpoint: s.oauth_userinfo_endpoint ?? '',
    clientId: s.oauth_client_id ?? '',
    clientSecret: '',
    scopes: (s.oauth_scopes ?? []).join(' '),
  };
}

export function oauthPayload(f: OAuthFields, includeSecret: boolean) {
  const scopes = f.scopes.trim()
    ? f.scopes.split(/\s+/).filter(Boolean)
    : [];
  return {
    oauth_issuer: f.issuer || null,
    oauth_authorization_endpoint: f.authorizationEndpoint || null,
    oauth_token_endpoint: f.tokenEndpoint || null,
    oauth_revocation_endpoint: f.revocationEndpoint || null,
    oauth_userinfo_endpoint: f.userinfoEndpoint || null,
    oauth_client_id: f.clientId || null,
    oauth_scopes: scopes,
    ...(includeSecret ? { oauth_client_secret: f.clientSecret } : {}),
  };
}
