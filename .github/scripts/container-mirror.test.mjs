import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, rmSync, statSync, chmodSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const github = fileURLToPath(new URL('../', import.meta.url));
const mirror = 'https://mirror.gcr.io';
const buildkit = 'moby/buildkit@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea';
const fixture = fn => {
  const dir = mkdtempSync(join(resolve(process.env.RUNNER_TEMP ?? process.env.TMPDIR ?? '../opencode'), 'mirror-test-'));
  try { fn(join(dir, 'daemon.json')); } finally { rmSync(dir, { recursive: true }); }
};
const configure = path => spawnSync('python3', [join(github, 'scripts/configure-docker-mirror.py'), path], { encoding: 'utf8' });

test('host mirror creates missing config and preserves existing Docker settings', () => fixture(path => {
  let result = configure(path);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(readFileSync(path)), { 'registry-mirrors': [mirror] });
  const existing = { 'log-driver': 'journald', features: { 'containerd-snapshotter': true },
    'registry-mirrors': ['https://existing.example', `${mirror}/`, mirror] };
  writeFileSync(path, JSON.stringify(existing), { mode: 0o640 });
  chmodSync(path, 0o640);
  result = configure(path);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(readFileSync(path)), { ...existing, 'registry-mirrors': [mirror, 'https://existing.example'] });
  assert.equal(statSync(path).mode & 0o777, 0o640);
  const first = readFileSync(path, 'utf8');
  assert.equal(configure(path).status, 0);
  assert.equal(readFileSync(path, 'utf8'), first, 'configuration must be idempotent');
}));

test('malformed or unreadable host configuration fails without overwriting it', () => fixture(path => {
  for (const input of ['', 'not json', 'null', '[]', '42', '{"registry-mirrors":null}',
    '{"registry-mirrors":"https://wrong"}', '{"registry-mirrors":[3]}']) {
    writeFileSync(path, input);
    assert.notEqual(configure(path).status, 0, input);
    assert.equal(readFileSync(path, 'utf8'), input);
  }
  // A directory produces a read error even when tests run as root.
  const result = configure(resolve(path, '..'));
  assert.notEqual(result.status, 0);
}));

test('all four container builder consumers configure host and BuildKit mirrors before bootstrap', () => {
  let count = 0;
  for (const file of ['container_validate.yml', 'container_publish_platform.yml', 'container_publish.yml']) {
    const yaml = readFileSync(join(github, 'workflows', file), 'utf8');
    const steps = yaml.split(/\n {6}- /).slice(1);
    for (let i = 0; i < steps.length; i++) {
      if (!steps[i].includes('uses: docker/setup-buildx-action@')) continue;
      count++;
      assert.match(steps[i - 1], /sudo python3 \.github\/scripts\/configure-docker-mirror\.py/);
      assert.match(steps[i - 1], /sudo systemctl restart docker/);
      assert.ok(steps[i - 1].indexOf('configure-docker-mirror.py') < steps[i - 1].indexOf('systemctl restart docker'));
      assert.match(steps[i], /buildkitd-config: \.github\/buildkitd\.toml/);
      assert.ok(steps[i].includes(`driver-opts: image=${buildkit}`), 'same digest-pinned multiarch bootstrap image');
    }
  }
  assert.equal(count, 4);
  const config = readFileSync(join(github, 'buildkitd.toml'), 'utf8');
  assert.match(config, /\[registry\."docker\.io"\]\s+mirrors = \["mirror\.gcr\.io"\]/);
  assert.ok(!/http = true|insecure = true/.test(config), 'mirror must use TLS');
});

test('Dockerfile keeps canonical official references pinned to both verified multiarch indexes', () => {
  const dockerfile = readFileSync(join(github, '../Dockerfile'), 'utf8');
  assert.match(dockerfile, /^FROM rust:1\.92-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder$/m);
  assert.match(dockerfile, /^FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587$/m);
  assert.ok(!dockerfile.includes('FROM mirror.gcr.io'), 'use cache configuration, not unsupported permanent direct cache references');
});
