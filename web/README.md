# ThinkWatch Web Console

The management console for ThinkWatch, built with React 19, TypeScript, and Vite.

## Tech Stack

- **React 19** with TypeScript
- **TanStack Router** for file-based routing
- **TanStack Query** for server state — see [Data fetching](#data-fetching)
- **shadcn/ui** (Radix UI + Tailwind CSS 4) for components
- **react-i18next** for internationalization (English + Chinese)
- **Vitest** + React Testing Library for testing
- **Web Crypto API** for HMAC-SHA256 request signing

## Development

```bash
pnpm install
pnpm dev          # Start dev server on http://localhost:5173
pnpm build        # Production build
pnpm test         # Run tests
pnpm exec tsc --noEmit  # Type check
```

## Data fetching

Server state goes through [TanStack Query](https://tanstack.com/query). A
component asks for what it renders with `useQuery`, and the query layer owns
the rest: the loading flag, the cache, cancelling a request nobody is waiting
for, and keeping a late response from landing under filters that have since
changed. No load runs in an effect.

- **Keys follow the endpoint.** `GET /api/admin/teams/{id}/members` is keyed
  `['admin', 'teams', id, 'members']`, with query-string parameters in a
  trailing object. A key names everything its `queryFn` reads —
  `@tanstack/query/exhaustive-deps` enforces that — and invalidating a
  prefix such as `['admin', 'teams']` reaches every query under it. Pass
  `exact: true` when a prefix would reach further than the write did.
- **Writes invalidate.** After a mutation, `await
  queryClient.invalidateQueries({ queryKey })` for what the screen shows.
  Awaiting keeps orderings like "close the dialog once the list is fresh".
  Screens that aren't open need nothing: every successful write through
  `api()` drops the cached queries no screen is using (`onSuccessfulWrite`,
  wired up in `main.tsx`), so none of them reopens onto pre-write data.
- **Pass the signal.** `queryFn: ({ signal }) => api(path, { signal })` lets
  the layer cancel a request once no component needs its answer.
- **Derive, don't copy.** Read `query.data` where it is rendered. When a
  screen needs local state that follows what was loaded — a form draft, a
  selection — adjust it during render, the way `useResetOnChange` does.

`src/lib/query-client.ts` builds the client and says why its defaults differ
from the library's. Tests render through `renderWithQueryClient` from
`src/test/render.tsx`, which gives every call an empty cache.

Do not add new suppressions of `react-hooks/set-state-in-effect` — every
finding it reports is a real one, and the codebase is clean of them.

## Project Structure

```
src/
├── components/
│   ├── layout/         # AppShell, Sidebar, Header, LanguageSwitcher
│   └── ui/             # shadcn/ui components (Button, Card, Dialog, Table, etc.)
├── hooks/
│   ├── use-auth.ts     # Authentication state & token management
│   └── use-mobile.ts   # Responsive breakpoint detection
├── lib/
│   ├── api.ts          # HTTP client with HMAC signing & auto token refresh
│   ├── query-client.ts # TanStack Query client & its defaults
│   └── utils.ts        # Utility functions
├── i18n/
│   ├── en.json         # English translations
│   ├── zh.json         # Chinese translations
│   └── index.ts        # i18next configuration
├── routes/
│   ├── setup.tsx           # First-run setup wizard
│   ├── login.tsx           # Login page (email/password + SSO)
│   ├── register.tsx        # Registration page
│   ├── dashboard.tsx       # Overview dashboard
│   ├── profile.tsx         # User profile & password change
│   ├── gateway/
│   │   ├── providers.tsx   # LLM provider CRUD
│   │   ├── models.tsx      # Model listing
│   │   ├── api-keys.tsx    # API key lifecycle management
│   │   └── logs.tsx        # Gateway request logs
│   ├── mcp/
│   │   ├── servers.tsx     # MCP server management
│   │   ├── tools.tsx       # MCP tool discovery
│   │   └── logs.tsx        # MCP invocation logs
│   ├── analytics/
│   │   ├── usage.tsx       # Token usage analytics
│   │   ├── costs.tsx       # Cost tracking
│   │   └── audit.tsx       # Audit log viewer
│   └── admin/
│       ├── users.tsx       # User management
│       ├── roles.tsx       # Role definitions
│       ├── settings.tsx    # Dynamic system settings (7 tabs)
│       └── log-forwarders.tsx  # Log forwarding configuration
├── test/
│   ├── render.tsx      # render() inside a fresh query cache
│   └── setup.ts        # Test setup (jest-dom + i18n)
└── router.tsx          # Route definitions & setup redirect logic
```

## Key Pages

### Setup Wizard (`/setup`)
Shown on first run when no users exist. Guides admin through:
1. Welcome + language selection
2. Admin account creation
3. Site name configuration
4. Optional first AI provider setup
5. API key display (shown once)

### Settings (`/admin/settings`)
7-tab configuration panel:
- **General** — System info + site name
- **Auth** — JWT TTLs, signature parameters
- **Gateway** — Cache TTL, timeouts
- **Security** — Content filter rules, PII redactor patterns
- **Budget** — Alert thresholds, webhook URL
- **API Keys** — Default expiry, rotation, inactivity policies
- **Data** — Usage/audit log retention periods

### API Keys (`/gateway/api-keys`)
Full lifecycle management:
- Create, edit, revoke, rotate keys
- Status badges (active/expired/inactive/rotated/revoked)
- Expiry warnings (yellow < 7d, red < 1d)
- "Expiring Soon" filter

## API Client

The API client (`src/lib/api.ts`) handles:
- Bearer token authentication via localStorage
- HMAC-SHA256 request signing for POST/PATCH/DELETE operations
- Automatic token refresh on 401 responses
- Deduplication of concurrent refresh attempts

## Testing

```bash
pnpm test              # Run all tests in watch mode
pnpm test -- --run     # Run once (CI mode)
```

Test files follow the pattern `*.test.tsx` / `*.test.ts` alongside source files.
