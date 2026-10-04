import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const sha = '1234567890abcdef1234567890abcdef12345678';
test('exact image revision guard fails closed except for a missing manifest', () => {
  const dir = mkdtempSync(join(resolve(process.env.RUNNER_TEMP ?? process.env.TMPDIR ?? '../opencode'), 'image-guard-test-'));
  try {
    writeFileSync(join(dir, 'docker'), `#!/usr/bin/env bash
if [ -n "$MOCK_ERROR" ]; then printf '%s\\n' "$MOCK_ERROR" >&2; exit 1; fi
printf '%s\\n' "$MOCK_CONFIG"
`, { mode: 0o755 });
    const config = revision => JSON.stringify({
      'linux/amd64': { config: { Labels: { 'org.opencontainers.image.revision': revision } } },
      'linux/arm64': { config: { Labels: { 'org.opencontainers.image.revision': revision } } },
    });
    for (const [configs, error, accepted] of [
      [config(sha), '', true],
      [config('another-commit'), '', false],
      [JSON.stringify({ 'linux/amd64': { config: { Labels: { 'org.opencontainers.image.revision': sha } } }, 'linux/arm64': { config: { Labels: { 'org.opencontainers.image.revision': 'another-commit' } } } }), '', false],
      [JSON.stringify({ 'linux/amd64': { config: { Labels: { 'org.opencontainers.image.revision': sha } } }, 'linux/arm64': { config: {} } }), '', false],
      ['{}', '', false],
      ['not-json', '', false],
      ['', 'ERROR: ghcr.io/cnrd/ebc-battery-tester:0.5.0: not found', true],
      ['', 'ERROR: manifest unknown', true],
      ['', 'ERROR: denied: requested access to the resource is denied', false],
      ['', 'ERROR: network connection timed out', false],
      ['', 'ERROR: docker-credential-helper: not found', false],
      ['', 'ERROR: failed to authorize: auth endpoint: not found', false],
      ['', 'ERROR: ghcr.io/another/image:0.5.0: not found', false],
      ['', 'ERROR: manifest unknown\nERROR: denied: authentication failed', false],
    ]) {
      const result = spawnSync('bash', [fileURLToPath(new URL('./check-release-image.sh', import.meta.url)), 'ghcr.io/cnrd/ebc-battery-tester', '0.5.0', sha], {
        env: { ...process.env, PATH: `${dir}:${process.env.PATH}`, TMPDIR: dir, RUNNER_TEMP: dir, MOCK_CONFIG: configs, MOCK_ERROR: error },
        encoding: 'utf8',
      });
      assert.equal(result.status === 0, accepted, `${configs} ${error}: ${result.stderr}`);
    }
  } finally {
    rmSync(dir, { recursive: true });
  }
});
