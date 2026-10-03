import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { checkContainerManifest } from './check-container-manifest.mjs';

const AMD = `sha256:${'a'.repeat(64)}`;
const ARM = `sha256:${'b'.repeat(64)}`;
const SHA = '0123456789abcdef0123456789abcdef01234567';
const VERSION = '1.2.3';
const OCI = 'org.opencontainers.image.';
const script = name => fileURLToPath(new URL(`./${name}`, import.meta.url));

const descriptor = (digest, architecture, mediaType = 'application/vnd.oci.image.manifest.v1+json') =>
  ({ mediaType, digest, size: 4321, platform: { os: 'linux', architecture } });

const indexManifest = (amd = descriptor(AMD, 'amd64'), arm = descriptor(ARM, 'arm64')) => ({
  schemaVersion: 2,
  mediaType: 'application/vnd.oci.image.index.v1+json',
  manifests: [amd, arm],
});

const labels = (version = VERSION, revision = SHA, extra = {}) => ({
  [`${OCI}source`]: 'https://github.com/cnrd/ebc-battery-tester',
  [`${OCI}revision`]: revision,
  [`${OCI}version`]: version,
  [`${OCI}created`]: '2026-10-03T00:00:00+00:00',
  [`${OCI}licenses`]: 'MIT',
  [`${OCI}description`]: 'Headless EBC Battery Tester server with remote browser UI',
  ...extra,
});

const config = (architecture, labelOverrides = labels()) =>
  ({ os: 'linux', architecture, config: { Labels: labelOverrides } });

const configs = (amd = config('amd64'), arm = config('arm64')) =>
  ({ 'linux/amd64': amd, 'linux/arm64': arm });

const valid = () => ({ manifest: indexManifest(), configs: configs() });

// m01-m03 happy paths: accepted registry-inspected shapes
test('m01 accepts the exact OCI two-platform index with consistent full label sets', () => {
  const { manifest, configs: cfgs } = valid();
  checkContainerManifest(manifest, cfgs, AMD, ARM, SHA, VERSION);
});
test('m02 accepts Docker schema2 index and manifest media types too', () => {
  checkContainerManifest(
    { ...indexManifest(descriptor(AMD, 'amd64', 'application/vnd.docker.distribution.manifest.v2+json'),
      descriptor(ARM, 'arm64', 'application/vnd.docker.distribution.manifest.v2+json')),
    mediaType: 'application/vnd.docker.distribution.manifest.list.v2+json' },
    configs(), AMD, ARM, SHA, VERSION);
});
test('m03 prerelease versions and main package versions share the accepted syntax, arm64 variant tolerated', () => {
  const arm = { ...descriptor(ARM, 'arm64'), platform: { os: 'linux', architecture: 'arm64', variant: 'v8' } };
  checkContainerManifest({ ...indexManifest(), manifests: [descriptor(AMD, 'amd64'), arm] },
    configs(config('amd64', labels('1.11.0-rc.1')), config('arm64', labels('1.11.0-rc.1'))), AMD, ARM, SHA, '1.11.0-rc.1');
  checkContainerManifest(indexManifest(), configs(config('amd64', labels('0.1.0')), config('arm64', labels('0.1.0'))), AMD, ARM, SHA, '0.1.0');
});

// m04-m06 index envelope: strict schemaVersion 2, index mediaType, exactly two entries
for (const [name, mutate] of [
  ['m04 schemaVersion 1 fails', m => ({ ...m, schemaVersion: 1 })],
  ['m04b missing schemaVersion fails', m => { delete m.schemaVersion; return m; }],
  ['m05 single-image mediaType on the index fails', m => ({ ...m, mediaType: 'application/vnd.oci.image.manifest.v1+json' })],
  ['m05b unknown index mediaType fails', m => ({ ...m, mediaType: 'application/vnd.oci.image.index.v2+json' })],
  ['m06 one descriptor (missing platform) fails', m => ({ ...m, manifests: [descriptor(AMD, 'amd64')] })],
  ['m06b three descriptors including extra arm/v7 fail', m => ({ ...m, manifests: [...m.manifests, { ...descriptor(`sha256:${'c'.repeat(64)}`, 'arm'), platform: { os: 'linux', architecture: 'arm', variant: 'v7' } }] })],
  ['m06c empty manifest array fails', m => ({ ...m, manifests: [] })],
  ['m06d manifests replaced by an object fails', m => ({ ...m, manifests: {} })],
]) test(name, () => {
  const { configs: cfgs } = valid();
  assert.throws(() => checkContainerManifest(mutate(indexManifest()), cfgs, AMD, ARM, SHA, VERSION), /two-platform runtime image index/);
});

// m07-m08 descriptor mapping: exact digest per platform, no duplicates
test('m07 swapped digests fail closed', () => {
  const manifest = indexManifest(descriptor(ARM, 'amd64'), descriptor(AMD, 'arm64'));
  assert.throws(() => checkContainerManifest(manifest, configs(), AMD, ARM, SHA, VERSION), /duplicated or swapped runtime descriptor: linux\/amd64/);
});
test('m08 duplicated amd64 descriptors fail closed', () => {
  const manifest = indexManifest(descriptor(AMD, 'amd64'), descriptor(AMD, 'amd64'));
  assert.throws(() => checkContainerManifest(manifest, configs(), AMD, ARM, SHA, VERSION), /duplicated or swapped runtime descriptor: linux\/amd64/);
});
for (const [name, mutate, pattern] of [
  ['m09 unknown descriptor platforms fail closed',
    m => ({ ...m, manifests: [descriptor(AMD, 'amd64'), { ...descriptor(ARM, 'arm'), platform: { os: 'linux', architecture: 'arm', variant: 'v7' } }] }), /descriptor/],
  ['m09b windows descriptors fail closed',
    m => ({ ...m, manifests: [descriptor(AMD, 'amd64'), { ...descriptor(ARM, 'amd64'), platform: { os: 'windows', architecture: 'amd64' } }] }), /descriptor/],
  ['m10 index mediaType reused inside a descriptor fails',
    m => ({ ...m, manifests: [descriptor(AMD, 'amd64'), descriptor(ARM, 'arm64', 'application/vnd.oci.image.index.v1+json')] }), /descriptor/],
  ['m11 missing platform object fails',
    m => ({ ...m, manifests: [descriptor(AMD, 'amd64'), { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: ARM }] }), /descriptor/],
]) test(name, () => {
  const { configs: cfgs } = valid();
  const manifest = mutate(indexManifest());
  assert.throws(() => checkContainerManifest(manifest, cfgs, AMD, ARM, SHA, VERSION), pattern);
});

// m12-m14 inputs: digest format, distinctness, full sha, version syntax
for (const [name, amd, arm, sha, version, pattern] of [
  ['m12 short amd64 digest fails', `sha256:${'a'.repeat(63)}`, ARM, SHA, VERSION, /platform SHA256 digests/],
  ['m12b uppercase hex digest fails', `sha256:${'A'.repeat(64)}`, ARM, SHA, VERSION, /platform SHA256 digests/],
  ['m12c tag instead of digest fails', 'latest', ARM, SHA, VERSION, /platform SHA256 digests/],
  ['m12d equal digests fail even when well-formed', AMD, AMD, SHA, VERSION, /distinct/],
  ['m13 short or non-hex commit sha fails', AMD, ARM, 'abc1234', VERSION, /full commit SHA/],
  ['m13b uppercase sha fails', AMD, ARM, SHA.toUpperCase(), VERSION, /full commit SHA/],
  ['m14 two-part version fails', AMD, ARM, SHA, '1.2', /X\.Y\.Z/],
  ['m14b build metadata version fails', AMD, ARM, SHA, '1.2.3+build5', /X\.Y\.Z/],
]) test(name, () => {
  const { manifest, configs: cfgs } = valid();
  assert.throws(() => checkContainerManifest(manifest, cfgs, amd, arm, sha, version), pattern);
});

// m15 configs: exactly the two native runtime configs
for (const [name, cfgs] of [
  ['m15 missing arm64 config fails', { 'linux/amd64': config('amd64') }],
  ['m15b extra linux/arm/v7 config fails', { ...configs(), 'linux/arm/v7': config('arm') }],
  ['m15c array configs fail', [{ 'linux/amd64': config('amd64'), 'linux/arm64': config('arm64') }]],
  ['m15d null configs fail', null],
]) test(name, () => {
  assert.throws(() => checkContainerManifest(indexManifest(), cfgs, AMD, ARM, SHA, VERSION), /exactly amd64 and arm64 runtime configs/);
});
test('m15g string configs fail', () => {
  assert.throws(() => checkContainerManifest(indexManifest(), 'configs', AMD, ARM, SHA, VERSION), /exactly amd64 and arm64 runtime configs/);
});
for (const [name, mutate, pattern] of [
  ['m15e wrong os in one config fails', c => ({ ...c, 'linux/arm64': { ...c['linux/arm64'], os: 'linuxkit' } }), /native runtime config/],
  ['m15f swapped architecture fields fail', c => ({ ...c, 'linux/amd64': config('arm64'), 'linux/arm64': config('amd64') }), /native runtime config/],
]) test(name, () => {
  assert.throws(() => checkContainerManifest(indexManifest(), mutate(configs()), AMD, ARM, SHA, VERSION), pattern);
});

// m16-m18 required OCI labels and cross-platform metadata consistency
for (const [key, value] of [
  [`${OCI}source`, 'https://github.com/other/ebc-battery-tester'],
  [`${OCI}revision`, 'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'],
  [`${OCI}version`, '1.2.2'],
  [`${OCI}licenses`, 'GPL-3.0'],
  [`${OCI}description`, 'Something else entirely'],
]) test(`m16 incorrect ${key} fails closed`, () => {
  const cfgs = configs(config('amd64', labels(VERSION, SHA, { [key]: value })));
  assert.throws(() => checkContainerManifest(indexManifest(), cfgs, AMD, ARM, SHA, VERSION), new RegExp(`${key.replace(/\./g, '\\.')}: linux/amd64`));
});
for (const key of ['source', 'revision', 'version', 'licenses', 'description']) test(`m16b missing ${key} label fails closed`, () => {
  const arm = config('arm64');
  delete arm.config.Labels[`${OCI}${key}`];
  assert.throws(() => checkContainerManifest(indexManifest(), configs(config('amd64'), arm), AMD, ARM, SHA, VERSION), new RegExp(`${key}: linux/arm64`));
});

test('unknown attestation platform cannot replace a runtime platform, even without a digest', () => {
  const unknown = { mediaType: 'application/vnd.oci.image.manifest.v1+json', platform: { os: 'unknown', architecture: 'unknown' } };
  assert.throws(() => checkContainerManifest(indexManifest(descriptor(AMD, 'amd64'), unknown), configs(), AMD, ARM, SHA, VERSION), /runtime descriptor/);
});
test('m17 revision label must equal the supplied commit sha argument', () => {
  const other = '1'.repeat(40);
  assert.throws(() => checkContainerManifest(indexManifest(), configs(), AMD, ARM, other, VERSION), /org\.opencontainers\.image\.revision/);
  checkContainerManifest(indexManifest(), configs(config('amd64', labels(VERSION, other)), config('arm64', labels(VERSION, other))), AMD, ARM, other, VERSION);
});

// m18 cross-platform OCI metadata comparison
for (const [name, armLabels, pattern] of [
  ['m18 differing created timestamp across platforms fails', labels(VERSION, SHA, { [`${OCI}created`]: '2026-10-03T00:00:05+00:00' }), /metadata differs/],
  ['m18b extra OCI label on one platform only fails', labels(VERSION, SHA, { [`${OCI}title`]: 'extra' }), /metadata differs/],
]) test(name, () => {
  assert.throws(() => checkContainerManifest(indexManifest(), configs(config('amd64'), config('arm64', armLabels)), AMD, ARM, SHA, VERSION), pattern);
});
test('m19 non-OCI labels are outside the platform comparison and tolerated', () => {
  checkContainerManifest(indexManifest(),
    configs(config('amd64', labels(VERSION, SHA, { 'build.arch': 'x86_64' })),
      config('arm64', labels(VERSION, SHA, { 'build.arch': 'aarch64' }))), AMD, ARM, SHA, VERSION);
});

// m20-m22 CLI fixture tests, no registry access
const DOCKER_STUB = '#!/usr/bin/env bash\nprintf \'%s\\n\' "$*" >> "$DOCKER_LOG"\nexit 0\n';
const runCli = (fixture, args, extraEnv = {}) => {
  const dir = mkdtempSync(join(resolve(process.env.RUNNER_TEMP ?? process.env.TMPDIR ?? '../opencode'), 'check-manifest-cli-'));
  try {
    writeFileSync(join(dir, 'docker'), DOCKER_STUB, { mode: 0o755 });
    if (fixture) {
      writeFileSync(join(dir, 'manifest.json'), fixture.manifest ?? JSON.stringify(indexManifest()));
      writeFileSync(join(dir, 'configs.json'), fixture.configs ?? JSON.stringify(configs()));
    }
    const manifestArg = fixture?.manifestFile ?? join(dir, 'manifest.json');
    const configsArg = fixture?.configsFile ?? join(dir, 'configs.json');
    const env = { ...process.env, PATH: `${dir}:${process.env.PATH}`, DOCKER_LOG: join(dir, 'docker.log'), ...extraEnv };
    const result = spawnSync('node', [script('check-container-manifest.mjs'), manifestArg, configsArg, ...args], { env, encoding: 'utf8' });
    return { result, dockerLog: existsSync(join(dir, 'docker.log')) ? readFileSync(join(dir, 'docker.log'), 'utf8') : '' };
  } finally { rmSync(dir, { recursive: true }); }
};

test('m20 CLI accepts valid fixture JSON and reports success without touching any registry', () => {
  const { result, dockerLog } = runCli({}, [AMD, ARM, SHA, VERSION]);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Native multiarchitecture manifest and OCI metadata verified/);
  assert.equal(dockerLog, '', 'verification must never invoke docker or reach a registry');
});
test('m21 CLI fails closed on equal and short digest arguments', () => {
  for (const args of [[AMD, AMD, SHA, VERSION], [`sha256:${'a'.repeat(63)}`, ARM, SHA, VERSION]]) {
    const { result, dockerLog } = runCli({}, args);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /platform SHA256 digests/);
    assert.equal(dockerLog, '');
  }
});
test('m22 CLI fails closed on malformed fixture JSON, wrong labels and missing arguments', () => {
  for (const [name, fixture, args] of [
    ['malformed manifest JSON', { manifest: '{"schemaVersion":' }, [AMD, ARM, SHA, VERSION]],
    ['wrong OCI license label', { configs: JSON.stringify(configs(config('amd64'), config('arm64', labels(VERSION, SHA, { [`${OCI}licenses`]: 'GPL-3.0' })))) }, [AMD, ARM, SHA, VERSION]],
    ['mismatched revision label vs sha argument', { configs: JSON.stringify(configs(config('amd64', labels(VERSION, '2'.repeat(40))), config('arm64', labels(VERSION, '2'.repeat(40))))) }, [AMD, ARM, SHA, VERSION]],
  ]) {
    const { result } = runCli(fixture, args);
    assert.notEqual(result.status, 0, `${name} must fail closed`);
  }
  const missing = spawnSync('node', [script('check-container-manifest.mjs')], { encoding: 'utf8' });
  assert.notEqual(missing.status, 0, 'missing arguments must fail');
});
