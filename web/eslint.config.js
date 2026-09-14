import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import reactCompiler from 'eslint-plugin-react-compiler'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    plugins: {
      // Surfaces patterns the React Compiler can't auto-memoise
      // (mutating refs in render, identity-changing closures captured
      // by hooks, etc.) so we get the developer-feedback half of the
      // compiler win even before the oxc transform integration ships.
      'react-compiler': reactCompiler,
    },
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    rules: {
      // `warn` rather than `error` while we land the underlying
      // refactors; flip to error once the existing finds are cleaned up.
      'react-compiler/react-compiler': 'warn',
      // A leading underscore already means "deliberately unused" throughout
      // this codebase — constructor parameters that exist only to match an
      // upstream signature, `{ _clientId: _, ...rest }` to drop a field.
      // Without this the convention reads as five defects.
      '@typescript-eslint/no-unused-vars': [
        'error',
        {
          argsIgnorePattern: '^_',
          varsIgnorePattern: '^_',
          caughtErrorsIgnorePattern: '^_',
          destructuredArrayIgnorePattern: '^_',
          ignoreRestSiblings: true,
        },
      ],
    },
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
  },
  {
    // Playwright fixtures, not React. `base.extend({ adminPage: async
    // ({ page }, use) => ... })` hands the fixture a callback named `use`,
    // and the hooks rule reads that call as a `use()` hook outside a
    // component. There is no React in this directory at all.
    files: ['e2e/**'],
    rules: {
      'react-hooks/rules-of-hooks': 'off',
    },
  },
  {
    // `src/components/ui/` is shadcn output, not code we write. Its house
    // style deliberately ships a component and its variants from one file
    // (`Button` + `buttonVariants`, `Sidebar` + `useSidebar`), which costs
    // Fast Refresh on those modules.
    //
    // **Splitting them would not survive.** The next `pnpm dlx shadcn add`
    // overwrites the file and the finding comes straight back, so enforcing
    // the rule here buys a warning that has to be re-fixed forever. The
    // files are leaf primitives that rarely change; losing HMR on them is
    // the cheaper side of the trade.
    //
    // The compiler rule is off here for the same reason: shadcn's sidebar
    // writes `document.cookie` inside a `useCallback` to persist the open
    // state. That is the upstream implementation, and editing it has the
    // same problem — the next `add` puts it back.
    files: ['src/components/ui/**'],
    rules: {
      'react-refresh/only-export-components': 'off',
      'react-compiler/react-compiler': 'off',
    },
  },
])
