// "Add to ThinkWatch Lite": the `thinkwatch://import` link that opens
// ThinkWatch Lite's New upstream dialog with this gateway and an API key
// filled in. The link is built here, in the browser, from a key the
// console shows once; nothing is sent anywhere to build it.
//
// The parameters and their rules are ThinkWatch Lite's import-link
// contract (v2, "Enterprise"): Lite rejects a whole link when any value
// breaks its rule, so a value that would be rejected is reported instead
// of being put in a link.

/// The first ThinkWatch Lite release that takes `service=thinkwatch`.
export const LITE_MIN_VERSION = '2026.10.16';

/// The first ThinkWatch Lite release that takes `limits`. An older one
/// refuses a link that carries it, so a link without limits leaves it out.
export const LITE_LIMITS_MIN_VERSION = '2026.10.26';

/// The upstream name the link gives; Lite asks for another when it is
/// taken.
const UPSTREAM_NAME = 'thinkwatch';

/// Lite's charset for a key: letters, digits and `- _ . ~ + / = :`.
const KEY_RE = /^[A-Za-z0-9\-_.~+/=:]{1,512}$/;

/// Hosts Lite takes over plain `http://` (contract v2.2): this machine, and
/// addresses that only resolve inside a private network.
const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]']);

/// Domain suffixes of intranet-style names.
const PRIVATE_SUFFIXES = ['.local', '.lan', '.internal', '.intranet', '.corp', '.home.arpa', '.localhost'];

/// Whether Lite takes `http://` for `hostname` (as `URL` normalises it:
/// lower case, IPv4 dotted, IPv6 in brackets).
export function plainHttpAllowed(hostname: string): boolean {
  if (LOOPBACK_HOSTS.has(hostname)) return true;
  if (hostname.startsWith('[')) {
    // fc00::/7 and fe80::/10, from the first 16 bits. `URL` writes a
    // leading run of zeros as `::`, which is neither.
    const first = hostname.slice(1).split(':')[0];
    const bits = first === '' ? 0 : parseInt(first, 16);
    return (bits & 0xfe00) === 0xfc00 || (bits & 0xffc0) === 0xfe80;
  }
  if (/^\d+\.\d+\.\d+\.\d+$/.test(hostname)) {
    // 10/8, 172.16/12, 192.168/16, 100.64/10 (shared address space),
    // 169.254/16 (link-local).
    const [a, b] = hostname.split('.').map(Number);
    return (
      a === 10 ||
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168) ||
      (a === 100 && b >= 64 && b <= 127) ||
      (a === 169 && b === 254)
    );
  }
  return !hostname.includes('.') || PRIVATE_SUFFIXES.some((s) => hostname.endsWith(s));
}

export type LiteLinkProblem =
  /// The gateway is reached over plain HTTP at a public address.
  | 'insecure-url'
  /// The base URL is not a bare `scheme://host[:port]`.
  | 'bad-url'
  /// The key has characters Lite does not take.
  | 'bad-key';

export type LiteLink = { link: string } | { problem: LiteLinkProblem };

/// The import link for the gateway at `baseUrl` (its root: no path, no
/// trailing slash) with `key` and, when there are any, the key's `limits`
/// (from [`liteLimits`]), or why Lite would refuse one.
export function liteImportLink(baseUrl: string, key: string, limits: string[] = []): LiteLink {
  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    return { problem: 'bad-url' };
  }
  const bare =
    url.origin === baseUrl &&
    url.username === '' &&
    url.password === '' &&
    !/[${}\s]/.test(baseUrl);
  if (!bare || (url.protocol !== 'https:' && url.protocol !== 'http:')) {
    return { problem: 'bad-url' };
  }
  if (url.protocol === 'http:' && !plainHttpAllowed(url.hostname)) {
    return { problem: 'insecure-url' };
  }
  if (!KEY_RE.test(key)) {
    return { problem: 'bad-key' };
  }
  const params = [
    ['service', 'thinkwatch'],
    ['name', UPSTREAM_NAME],
    ['url', baseUrl],
    ['key', key],
    ...(limits.length > 0 ? [['limits', limits.join(';')]] : []),
  ]
    .map(([k, v]) => `${k}=${encodeURIComponent(v)}`)
    .join('&');
  return { link: `thinkwatch://import?${params}` };
}

/// A rate-limit rule of an API key, as the limits endpoints list it.
export interface KeyRateRule {
  surface: 'ai_gateway' | 'mcp_gateway';
  metric: 'requests' | 'tokens';
  window_secs: number;
  max_count: number;
  enabled: boolean;
  expires_at?: string | null;
}

/// Lite's limit on a request or token count (fifteen digits).
const MAX_LITE_COUNT = 999_999_999_999_999;
/// The most limits one link carries.
export const MAX_LITE_LIMITS = 16;

/// The key's own rate-limit rules as entries of Lite's `limits` parameter
/// (`per|measure|max|every`), the ones that mean the same limit there:
///
/// - Only enabled, unexpired rules on the AI gateway: Lite is an AI
///   client's upstream, and MCP tools do not go through it.
/// - Only request counts. A token rule here counts weighted tokens (each
///   model's input and output weights, cache reads at a fraction of the
///   input weight and cache writes at a premium), while Lite counts plain
///   tokens; no `cache_reads` setting makes the two the same number.
/// - A sliding window of whole minutes up to 60 becomes Lite's rolling
///   `minute` window, one of whole hours up to 24 its rolling `hour`
///   window (an hour, or 24, is written in hours). Any other window, a
///   sliding 7 days among them, has no counterpart: Lite's days, weeks
///   and months are calendar periods, a different limit.
/// - Two rules on one window keep the smaller count. Of more than 16
///   windows the 16 that allow the fewest requests per second stay: a
///   looser rate is the likelier to be implied by a tighter one.
///
/// The entries come ordered by window, shortest first; none means the
/// link carries no `limits`.
export function liteLimits(rules: KeyRateRule[], now: number = Date.now()): string[] {
  const kept = new Map<number, number>();
  for (const r of rules) {
    if (!r.enabled || r.surface !== 'ai_gateway' || r.metric !== 'requests') continue;
    if (r.expires_at && !(Date.parse(r.expires_at) > now)) continue;
    if (!Number.isInteger(r.max_count) || r.max_count < 1 || r.max_count > MAX_LITE_COUNT) continue;
    if (liteWindow(r.window_secs) === undefined) continue;
    const before = kept.get(r.window_secs);
    if (before === undefined || r.max_count < before) kept.set(r.window_secs, r.max_count);
  }
  return [...kept]
    .sort(([wa, ma], [wb, mb]) => ma / wa - mb / wb || wa - wb)
    .slice(0, MAX_LITE_LIMITS)
    .sort(([wa], [wb]) => wa - wb)
    .map(([secs, max]) => {
      const { per, every } = liteWindow(secs)!;
      return every === 1 ? `${per}|requests|${max}` : `${per}|requests|${max}|${every}`;
    });
}

/// Lite's rolling window of `secs` seconds, if it has one.
function liteWindow(secs: number): { per: 'minute' | 'hour'; every: number } | undefined {
  if (!Number.isInteger(secs) || secs <= 0) return undefined;
  if (secs % 3600 === 0 && secs / 3600 <= 24) return { per: 'hour', every: secs / 3600 };
  if (secs % 60 === 0 && secs / 60 <= 60) return { per: 'minute', every: secs / 60 };
  return undefined;
}

/// Where ThinkWatch Lite is downloaded, in the console's language.
export function liteDownloadUrl(language: string): string {
  return language.startsWith('zh') ? 'https://thinkwat.ch/zh-CN/lite' : 'https://thinkwat.ch/lite';
}
