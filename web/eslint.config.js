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
    },
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
  },
])
