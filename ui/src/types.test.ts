import { describe, it, expect } from 'vitest';
import type { AdminModel } from './types';

/**
 * Compile-time assertions for the shape of `AdminModel`. These tests produce
 * no runtime value beyond `expect(true).toBe(true)` — their real job is to
 * fail `tsc` if the field shape regresses.
 */
describe('AdminModel type shape', () => {
  it('exposes mmproj_filename as `string | null`', () => {
    const nullValue: AdminModel['mmproj_filename'] = null;
    const stringValue: AdminModel['mmproj_filename'] = 'mmproj-foo-f16.gguf';

    // Reference the bindings so ESLint's no-unused-vars doesn't fire.
    expect(nullValue).toBeNull();
    expect(typeof stringValue).toBe('string');
  });
});
