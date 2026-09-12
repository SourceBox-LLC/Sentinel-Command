import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    chunkSizeWarningLimit: 600,
  },
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://localhost:8000',
        changeOrigin: true,
      },
      '/video_feed': {
        target: 'http://localhost:8000',
        changeOrigin: true,
      },
    },
  },
  // Vitest config — happy-dom is faster than jsdom for our component tests
  // and we don't need anything jsdom-only (yet). Tests live under tests/
  // at the frontend root; setup.js wires in @testing-library/jest-dom
  // matchers (toBeInTheDocument, etc.) and globals.
  test: {
    environment: 'happy-dom',
    globals: true,
    setupFiles: ['./tests/setup.js'],
    include: ['tests/**/*.test.{js,jsx}'],
    // Pin the auth provider for tests instead of inheriting whatever the
    // developer has in .env.
    //
    // Vitest loads .env like any Vite build, so a machine configured for
    // self-hosted development (VITE_AUTH_PROVIDER=local, which AGENTS.md
    // tells you to set) flipped the whole suite onto the local-auth code
    // path and produced 54 failures of the form "Local auth hooks must be
    // used within AuthProvider" — none of them related to the change
    // under test.
    //
    // CI never saw it, because CI has no .env. That combination is the
    // worst kind: red locally, green on the PR, and nothing in the diff
    // to explain it.
    env: {
      VITE_AUTH_PROVIDER: 'clerk',
    },
  },
})