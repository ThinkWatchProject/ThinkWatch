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

/// The upstream name the link gives; Lite asks for another when it is
/// taken.
const UPSTREAM_NAME = 'thinkwatch';

/// Lite's charset for a key: letters, digits and `- _ . ~ + / = :`.
const KEY_RE = /^[A-Za-z0-9\-_.~+/=:]{1,512}$/;

/// Lite takes `http://` only for an address on the same machine.
const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]']);

export type LiteLinkProblem =
  /// The gateway is reached over plain HTTP at an address other than
  /// this machine's.
  | 'insecure-url'
  /// The base URL is not a bare `scheme://host[:port]`.
  | 'bad-url'
  /// The key has characters Lite does not take.
  | 'bad-key';

export type LiteLink = { link: string } | { problem: LiteLinkProblem };

/// The import link for the gateway at `baseUrl` (its root: no path, no
/// trailing slash) with `key`, or why Lite would refuse one.
export function liteImportLink(baseUrl: string, key: string): LiteLink {
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
  if (url.protocol === 'http:' && !LOOPBACK_HOSTS.has(url.hostname)) {
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
  ]
    .map(([k, v]) => `${k}=${encodeURIComponent(v)}`)
    .join('&');
  return { link: `thinkwatch://import?${params}` };
}

/// Where ThinkWatch Lite is downloaded, in the console's language.
export function liteDownloadUrl(language: string): string {
  return language.startsWith('zh') ? 'https://thinkwat.ch/zh-CN/lite' : 'https://thinkwat.ch/lite';
}
