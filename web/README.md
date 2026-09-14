# ThinkWatch Web Console

The management console for ThinkWatch, built with React 19, TypeScript, and Vite.

## Tech Stack

- **React 19** with TypeScript
- **TanStack Router** for file-based routing
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

Loads are hand-rolled: a `useCallback` that fetches and sets state, plus a
`useEffect` that calls it. There is no data-fetching layer in this app.

That shape trips `react-hooks/set-state-in-effect`, and the sites that it
flags carry a one-line suppression saying which of two things is going on:

- **The rule is wrong.** `useEffect(() => { load(); }, [load])` where `load`
  is async and its first statement is the `await`. Every setState inside
  runs in the continuation — never synchronously with the effect, never a
  cascading render. The rule's cross-function analysis does not model
  `await`.
- **The rule is right and the fix is architectural.** The loader's first
  statement flips a spinner. Hoisting that flag out to render-time silences
  the rule, but it splits "start a load" across two places and leaves every
  other caller of the loader responsible for remembering half of it.

**Both go away with a data-fetching layer** (TanStack Query or equivalent),
which owns the loading flag and the cache and removes the effect entirely.
That is a deliberate piece of work, not something to fold into a lint pass.
Until then, do not add new suppressions of this rule without one of the two
reasons above — every other finding it reports is a real one, and the rest
of the codebase is clean of them.

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
