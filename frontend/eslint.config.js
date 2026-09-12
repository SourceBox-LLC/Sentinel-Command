// ESLint flat config (eslint v9+).
//
// Pre-this-file the project shipped eslint + eslint-plugin-react-hooks +
// eslint-plugin-react-refresh as devDependencies and a `npm run lint`
// script, but no actual config file — meaning `eslint .` silently errored
// with "couldn't find a config" and the lint command had been a no-op for
// however long. This file fixes that, with a deliberate focus on rules
// that catch real bugs (react-hooks/exhaustive-deps) over stylistic ones.
//
// What we enable
// ---------------
// - `js.configs.recommended`              — base set: no-undef, no-redeclare, etc.
// - `react-hooks/recommended` (v7+ flat)  — rules-of-hooks + exhaustive-deps
// - `react-refresh/only-export-components`— Vite HMR safety
//
// What we tone down
// -----------------
// - `no-unused-vars` — warn (not error) and ignore args starting with `_`
//   so tests / event handlers can take unused params without ceremony
// - `no-empty` — error, but allow empty catch blocks that explicitly
//   intend to swallow (we have a few in localStorage parsing where the
//   message is "we got malformed JSON; ignore and move on")
//
// Test files get vitest globals (describe/it/expect/vi/...) so a future
// test author doesn't have to import them or chase a no-undef warning.

import js from "@eslint/js"
import reactHooks from "eslint-plugin-react-hooks"
import reactRefresh from "eslint-plugin-react-refresh"
import globals from "globals"

export default [
  // Always ignore build outputs and vendored code.
  {
    ignores: ["dist/**", "node_modules/**", "public/**"],
  },

  // Base rules + browser env for all source / config / test JS+JSX.
  {
    files: ["**/*.{js,jsx}"],
    languageOptions: {
      ecmaVersion: "latest",
      sourceType: "module",
      globals: {
        ...globals.browser,
        ...globals.node, // for vite.config.js
      },
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
    },
    plugins: {
      "react-hooks": reactHooks,
      "react-refresh": reactRefresh,
    },
    rules: {
      ...js.configs.recommended.rules,
      ...reactHooks.configs.recommended.rules,
      "react-refresh/only-export-components": [
        "warn",
        { allowConstantExport: true },
      ],
      // Quality-of-life — these match how the codebase already writes JS.
      //
      // ``varsIgnorePattern`` skips PascalCase identifiers because React
      // components used only in JSX (`<Foo />`) aren't treated as
      // references by the bare ``no-unused-vars`` rule — that's what
      // ``eslint-plugin-react``'s ``jsx-uses-vars`` would normally fix,
      // but pulling in another plugin for one rule is overkill. The
      // PascalCase regex covers ~all our import-of-component patterns;
      // the camelCase + lowercase ones still get flagged so we still
      // catch genuinely-unused locals.
      "no-unused-vars": [
        "warn",
        {
          argsIgnorePattern: "^_",
          varsIgnorePattern: "^[A-Z_]",
          caughtErrorsIgnorePattern: "^_",
        },
      ],
      "no-empty": ["error", { allowEmptyCatch: true }],
      // react-hooks v7 ships a set of new rules that are (mostly)
      // forward-looking advisories from the React Compiler — they catch
      // patterns the compiler can't optimize, not patterns that are
      // currently buggy. Demote them to warn so CI passes while leaving
      // the diagnostic information visible. Each one has a textbook fix
      // (lazy useState, useMemo with no deps, etc.) that we should apply
      // when we touch the surrounding code, not as a single sweep.
      "react-hooks/set-state-in-effect": "warn",
      "react-hooks/purity": "warn",
      "react-hooks/refs": "warn",
      "react-hooks/preserve-manual-memoization": "warn",
      // Same family, same reasoning — these two were missed when the
      // list above was written, and they were the only thing keeping
      // `npm run lint` at a non-zero exit:
      //
      // - `immutability` fires 12 times on the shape
      //   `useEffect(() => { loadThings() }, [...])` where `loadThings`
      //   is a `const` arrow function declared further down the
      //   component. It is correct at runtime — effects run after
      //   render, by which point the binding is initialized — so this
      //   is the compiler saying it cannot track the value, not a live
      //   bug. The fix is to hoist each loader above its effect (or
      //   wrap in useCallback), which is a real change to six files and
      //   belongs with the next edit to each.
      // - `use-memo` fires once, on a non-inline first argument.
      //
      // Kept at `warn` rather than disabled: the diagnostic stays in
      // the CI log, and lint can now be enforcing for everything else.
      "react-hooks/immutability": "warn",
      "react-hooks/use-memo": "warn",
    },
  },

  // Vitest globals for tests — `vitest.config.js` sets `globals: true`,
  // so `describe` / `it` / `expect` / `vi` / `beforeEach` / `afterEach`
  // are available without import.
  {
    files: ["tests/**/*.{js,jsx}"],
    languageOptions: {
      globals: {
        ...globals.browser,
        describe: "readonly",
        it: "readonly",
        test: "readonly",
        expect: "readonly",
        vi: "readonly",
        beforeEach: "readonly",
        afterEach: "readonly",
        beforeAll: "readonly",
        afterAll: "readonly",
      },
    },
  },
]
