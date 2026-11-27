/// <reference types="vitest/globals" />
/* Vitest setup. Loaded before each test file. */

import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";

/* Whatever a test stubbed or faked, the next one starts with the real
 * globals and timers and every spy restored. */
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});
