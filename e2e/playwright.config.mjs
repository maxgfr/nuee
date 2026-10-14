// Playwright configuration of the end-to-end suite. Run it through scripts/e2e.sh, which
// builds everything first; the local relay and static server start in setup.mjs.
import { defineConfig, devices } from '@playwright/test';

const a11y = (name, device) => ({
  name: `a11y-${name}`,
  testMatch: /a11y\.spec\.mjs$/,
  use: { ...devices[device] },
});

export default defineConfig({
  testDir: './tests',
  outputDir: './test-results',
  globalSetup: './setup.mjs',
  // One test at a time: each one already starts several browsers and CLI processes.
  workers: 1,
  fullyParallel: false,
  retries: 0,
  forbidOnly: true,
  reporter: 'list',
  // Job runs take up to 3 minutes; the whole test gets setup time on top.
  timeout: 6 * 60_000,
  expect: { timeout: 3 * 60_000 },
  use: {
    locale: 'en-US',
    actionTimeout: 30_000,
    navigationTimeout: 30_000,
  },
  projects: [
    {
      name: 'cross',
      testMatch: /(compute|cheat|files|skycmd|host-in-browser|webrtc|exec|gpu|embed|train|host|durable|anchor|website)\.spec\.mjs$/,
    },
    a11y('chromium', 'Desktop Chrome'),
    a11y('firefox', 'Desktop Firefox'),
    a11y('webkit', 'Desktop Safari'),
  ],
});
