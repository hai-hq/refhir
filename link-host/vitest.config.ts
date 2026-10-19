import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['test/**/*.test.ts'],
    testTimeout: 30_000,
    hookTimeout: 30_000,
    // Tests start servers and processes of their own, on ports of their own.
    pool: 'forks',
  },
});
