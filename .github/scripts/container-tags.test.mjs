import test from 'node:test';
import assert from 'node:assert/strict';
import { containerTags, packageVersion, validateCargoVersion } from './container-tags.mjs';

const sha = '1234567890abcdef1234567890abcdef12345678';
for (const [ref, expected] of [
  ['refs/heads/main', ['latest', 'main']],
  ['refs/tags/v0.5.0', ['0.5.0', '0.5']],
  ['refs/tags/v0.6.3', ['0.6.3', '0.6']],
  ['refs/tags/v0.6.0-beta.1', ['0.6.0-beta.1']],
  ['refs/tags/v1.0.0', ['1.0.0', '1.0', '1']],
  ['refs/tags/v2.7.4', ['2.7.4', '2.7', '2']],
  ['refs/tags/v2.8.0-rc.1', ['2.8.0-rc.1']],
  ['refs/tags/v1.5.0-beta.2', ['1.5.0-beta.2']],
]) {
  test(ref, () => assert.deepEqual(containerTags(ref, sha), [...expected, `sha-${sha}`]));
}
test('reject development refs, malformed versions and short SHAs', () => {
  for (const ref of ['refs/heads/wip', 'refs/pull/1/merge', 'refs/tags/v1', 'refs/tags/v01.2.3', 'refs/tags/v1.2.3-01', 'refs/tags/v1.2.3+build']) {
    assert.throws(() => containerTags(ref, sha));
  }
  assert.throws(() => containerTags('refs/heads/main', '1234567'));
});
test('validate only package version, including prereleases', () => {
  const cargo = '[package]\nname = "test"\nversion = "2.8.0-rc.1"\n\n[dependencies]\nversion = "9.9.9"\n';
  assert.equal(packageVersion(cargo), '2.8.0-rc.1');
  validateCargoVersion('refs/tags/v2.8.0-rc.1', sha, cargo);
  assert.throws(() => validateCargoVersion('refs/tags/v9.9.9', sha, cargo));
  assert.throws(() => validateCargoVersion('refs/heads/main', sha, cargo));
});
test('package version validation fails closed for a missing or inherited version', () => {
  for (const cargo of ['[dependencies]\nversion = "0.5.0"\n', '[package]\nversion.workspace = true\n']) {
    assert.throws(() => validateCargoVersion('refs/tags/v0.5.0', sha, cargo));
  }
  validateCargoVersion('refs/tags/v0.5.0', sha, '[package]\nversion = "0.5.0"\n');
});
