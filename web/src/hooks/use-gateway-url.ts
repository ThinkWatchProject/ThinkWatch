import { useQuery } from '@tanstack/react-query';
import { api } from '@/lib/api';

/// `GET /api/gateway/info`: the configured parts of the gateway's public
/// address. `null` is a part that is not set.
export interface GatewayInfo {
  public_protocol: 'http' | 'https' | null;
  /// As it goes into a URL: IPv6 in brackets.
  public_host: string | null;
  public_port: number | null;
  /// The port the gateway listens on.
  gateway_port: number;
}

/// The gateway's port until its configuration is known.
const DEFAULT_GATEWAY_PORT = 3000;

/// The gateway's base URL (`https://gateway.example.com`): what `info` says
/// is configured, the scheme and host of the address the console is open
/// at (`at`, the page's `location`) for what is not, and the gateway's
/// listening port when no public port is set. A default port is left out.
/// Without `info` (still loading, or not readable) the URL is made from the
/// browser's address and port 3000.
export function gatewayBaseUrl(
  info: GatewayInfo | undefined,
  at: Pick<Location, 'protocol' | 'hostname'>,
): string {
  const scheme = info?.public_protocol ?? (at.protocol === 'https:' ? 'https' : 'http');
  const host = info?.public_host ?? (at.hostname || 'localhost');
  const port = info?.public_port ?? info?.gateway_port ?? DEFAULT_GATEWAY_PORT;
  const defaultPort = (scheme === 'http' && port === 80) || (scheme === 'https' && port === 443);
  return defaultPort ? `${scheme}://${host}` : `${scheme}://${host}:${port}`;
}

/// The gateway's public base URL, for any signed-in user.
///
/// `baseUrl` is never empty: until the configuration has loaded, and when
/// it could not be read, it is made from the browser's address alone.
/// `known` says it comes from the configuration; `failed`, that the
/// configuration could not be read.
///
/// A 401 does not sign the user out from here: this is read on screens
/// that show a key once (a new key, a rotated key, the end of setup), and
/// leaving them would lose it.
export function useGatewayUrl(): { baseUrl: string; known: boolean; failed: boolean } {
  const query = useQuery({
    queryKey: ['gateway', 'info'],
    queryFn: ({ signal }) =>
      api<GatewayInfo>('/api/gateway/info', { signal, no401Redirect: true }),
    staleTime: 5 * 60_000,
  });
  return {
    baseUrl: gatewayBaseUrl(query.data, window.location),
    known: query.isSuccess,
    failed: query.isError,
  };
}
