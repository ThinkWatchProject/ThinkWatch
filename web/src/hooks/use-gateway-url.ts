import { useQuery } from '@tanstack/react-query';
import { api } from '@/lib/api';

interface GatewayInfo {
  /// The gateway's root as clients reach it, e.g. `https://gateway.example.com`.
  base_url: string;
}

/// The gateway's public base URL (`GET /api/gateway/info`), which any
/// signed-in user may read. `undefined` until it has loaded, and when it
/// could not be read.
///
/// A 401 does not sign the user out from here: this is read on screens
/// that show a key once (a new key, a rotated key, the end of setup), and
/// leaving them would lose it.
export function useGatewayUrl(): { baseUrl: string | undefined; failed: boolean } {
  const query = useQuery({
    queryKey: ['gateway', 'info'],
    queryFn: ({ signal }) =>
      api<GatewayInfo>('/api/gateway/info', { signal, no401Redirect: true }),
    staleTime: 5 * 60_000,
  });
  return { baseUrl: query.data?.base_url, failed: query.isError };
}
