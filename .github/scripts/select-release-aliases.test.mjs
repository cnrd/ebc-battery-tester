import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { publishedVersion, compareStableVersions, selectReleaseAliases, inspectRegistryAlias } from './select-release-aliases.mjs';

const IMAGE = 'ghcr.io/cnrd/ebc-battery-tester';
const NEW = '1234567890abcdef1234567890abcdef12345678';
const OLD = 'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee';
const VERSION = 'org.opencontainers.image.version';
const REVISION = 'org.opencontainers.image.revision';

const platform = (version, revision, suffix = 'amd64', os = 'linux') => ({
  os, architecture: suffix, config: { Labels: { [VERSION]: version, [REVISION]: revision } },
});
const index = (amd, arm) => ({ 'linux/amd64': amd, 'linux/arm64': arm });
const okIndex = (version, revision) => index(platform(version, revision), platform(version, revision, 'arm64'));
const script = name => fileURLToPath(new URL(`./${name}`, import.meta.url));

// c01-c10 publishedVersion: strict dual-runtime labels, everything else fails closed
test('c01 accepts exactly linux/amd64+arm64 with consistent version/full revision', () => {
  const current = publishedVersion(okIndex('1.10.0', NEW));
  assert.equal(current.version, '1.10.0');
  assert.equal(current.revision, NEW);
});
for (const [name, configs] of [
  ['c02 extra linux/arm/v7 runtime', { ...okIndex('1.10.0', NEW), 'linux/arm/v7': platform('1.10.0', NEW, 'arm') }],
  ['c03 extra windows/amd64 runtime', { ...okIndex('1.10.0', NEW), 'windows/amd64': platform('1.10.0', NEW, 'amd64', 'windows') }],
  ['c04 missing arm64 runtime', { 'linux/amd64': platform('1.10.0', NEW) }],
  ['c05 wrong architecture field', index(platform('1.10.0', NEW), platform('1.10.0', NEW, 'amd64'))],
  ['c06 wrong os field', index(platform('1.10.0', NEW), platform('1.10.0', NEW, 'arm64', 'linuxkit'))],
  ['c07 missing config object', { 'linux/amd64': { os: 'linux', architecture: 'amd64' }, 'linux/arm64': platform('1.10.0', NEW, 'arm64') }],
  ['c08 short revision label', okIndex('1.10.0', 'abc1234')],
  ['c09 version differs across platforms', index(platform('1.10.0', NEW), platform('1.9.9', NEW, 'arm64'))],
  ['c10 revision differs across platforms', index(platform('1.10.0', NEW), platform('1.10.0', OLD, 'arm64'))],
]) test(`${name} fails closed`, () => assert.throws(() => publishedVersion(configs)));
test('c11 prerelease on stable alias fails closed, malformed version too', () => {
  assert.throws(() => publishedVersion(okIndex('1.10.0-rc.1', NEW)), /prerelease/);
  assert.throws(() => publishedVersion(okIndex('1.10', NEW)), /X\.Y\.Z/);
});

// c12-c14 numeric ordering with arbitrary precision
test('c12 orders 1.10 above 1.9 numerically both directions', () => {
  assert.equal(compareStableVersions('1.10.0', '1.9.0'), 1);
  assert.equal(compareStableVersions('1.9.0', '1.10.0'), -1);
});
test('c13 exact equality and patch ordering', () => {
  assert.equal(compareStableVersions('1.10.1', '1.10.1'), 0);
  assert.equal(compareStableVersions('1.10.2', '1.10.10'), -1);
});
test('c14 beyond MAX_SAFE_INTEGER precision via BigInt', () => {
  assert.equal(compareStableVersions('9007199254740993.0.0', '9007199254740992.0.0'), 1);
});
test('c15 prerelease or malformed comparison fails closed', () => {
  assert.throws(() => compareStableVersions('1.10.0-rc.1', '1.9.0'));
  assert.throws(() => compareStableVersions('1.10', '1.9.0'));
});

// c16-c23 selectReleaseAliases with pure inspect mock
test('c16 prerelease candidate returns exact+SHA with zero floating reads', () => {
  const inspected = [];
  const selected = selectReleaseAliases('1.11.0-rc.1', NEW, alias => { inspected.push(alias); return null; });
  assert.deepEqual(selected, ['1.11.0-rc.1', `sha-${NEW}`]);
  assert.deepEqual(inspected, []);
});
test('c17 major zero emits minor alias only and never inspects 0', () => {
  const inspected = [];
  const selected = selectReleaseAliases('0.6.0', NEW, alias => { inspected.push(alias); return null; });
  assert.deepEqual(selected, ['0.6.0', '0.6', `sha-${NEW}`]);
  assert.deepEqual(inspected, ['0.6']);
});
test('c18 missing manifests keep floating aliases', () => {
  assert.deepEqual(selectReleaseAliases('1.10.0', NEW, () => null), ['1.10.0', '1.10', '1', `sha-${NEW}`]);
});
test('c19 higher current version skips both aliases with captured log', () => {
  const logs = [];
  const selected = selectReleaseAliases('1.10.0', NEW, () => okIndex('1.10.5', OLD), message => logs.push(message));
  assert.deepEqual(selected, ['1.10.0', `sha-${NEW}`]);
  assert.match(logs.join('\n'), /Skipping floating alias 1\.10:.*1\.10\.5/s);
  assert.match(logs.join('\n'), /Skipping floating alias 1:.*1\.10\.5/s);
});
test('c20 major alias crosses minors numerically: lower updates', () => {
  const logs = [];
  const selected = selectReleaseAliases('1.10.0', NEW, alias => alias === '1' ? okIndex('1.9.9', OLD) : null, m => logs.push(m));
  assert.deepEqual(selected, ['1.10.0', '1.10', '1', `sha-${NEW}`]);
  assert.equal(logs.length, 0);
});
test('c21 same version same SHA updates, different SHA fails closed', () => {
  assert.ok(selectReleaseAliases('1.10.0', NEW, () => okIndex('1.10.0', NEW)).includes('1.10'));
  assert.throws(() => selectReleaseAliases('1.10.0', NEW, () => okIndex('1.10.0', OLD)), /different revision/);
});
test('c22 wrong version line on alias fails closed', () => {
  assert.throws(() => selectReleaseAliases('1.10.0', NEW, alias => alias === '1' ? okIndex('2.0.0', OLD) : okIndex('1.10.0', NEW)), /version line/);
  assert.throws(() => selectReleaseAliases('1.10.0', NEW, () => okIndex('1.9.0', OLD)), /version line/);
});
test('c23 prerelease registry content on stable alias fails closed', () => {
  assert.throws(() => selectReleaseAliases('1.10.0', NEW, () => okIndex('1.10.0-rc.1', NEW)), /prerelease/);
});

// c24-c28 inspectRegistryAlias with injected run mock
test('c24 uses the exact imagetools invocation', () => {
  const seen = [];
  const configs = inspectRegistryAlias(IMAGE, '1.10', (cmd, args) => {
    seen.push([cmd, args]);
    return JSON.stringify(okIndex('1.10.0', NEW));
  });
  assert.equal(seen[0][0], 'docker');
  assert.deepEqual(seen[0][1], ['buildx', 'imagetools', 'inspect', `${IMAGE}:1.10`, '--format', '{{json .Image}}']);
  assert.equal(configs['linux/amd64'].os, 'linux');
});
test('c25 only allowlisted manifest-not-found returns null', () => {
  for (const message of [
    `ERROR: ${IMAGE}:1: not found`,
    `ERROR: ${IMAGE}:1: manifest unknown`,
    'ERROR: manifest unknown',
    'ERROR: manifest unknown: manifest unknown',
  ]) assert.equal(inspectRegistryAlias(IMAGE, '1', () => { throw Object.assign(new Error('x'), { status: 1, stderr: message }); }), null);
});
test('c26 auth, network and unrelated not-found texts fail closed', () => {
  for (const stderr of [
    'ERROR: denied: requested access to the resource is denied',
    'ERROR: Get "https://ghcr.io/v2/": dial tcp: i/o timeout',
    'ERROR: credential helper docker-credential-ghcr not found',
  ]) assert.throws(() => inspectRegistryAlias(IMAGE, '1', () => { throw Object.assign(new Error('x'), { status: 1, stderr }); }), /Cannot inspect/);
});
test('c27 not-found text without nonzero status fails closed', () => {
  assert.throws(() => inspectRegistryAlias(IMAGE, '1', () => { throw Object.assign(new Error('x'), { stderr: 'ERROR: manifest unknown' }); }), /Cannot inspect/);
});
test('c28 successful null/array/malformed JSON fails not-missing', () => {
  for (const output of ['null', '[]', '{oops', '']) {
    assert.throws(() => inspectRegistryAlias(IMAGE, '1', () => output), /Malformed registry config/);
  }
});

// c29+ real CLI with fake docker and a Git view containing an unpublished newer tag
const DOCKER_STUB = `#!/usr/bin/env bash
set -u
ref="$4"
printf '%s\\n' "$*" >> "$DOCKER_LOG"
safe=\${ref//[\\/:]/_}
if [ -f "$MOCKDIR/$safe.err" ]; then
  cat "$MOCKDIR/$safe.err" >&2
  exit "$(cat "$MOCKDIR/$safe.code" 2>/dev/null || echo 1)"
fi
if [ -f "$MOCKDIR/$safe.json" ]; then cat "$MOCKDIR/$safe.json"; exit 0; fi
printf 'unexpected reference %s\\n' "$ref" >&2
exit 1
`;
const GIT_STUB = `#!/usr/bin/env bash
printf 'git invoked: %s\\n' "$*" >> "$GIT_LOG"
printf 'v1.99.0\\n'
`;
const runCli = (version, responses) => {
  const dir = mkdtempSync(join(resolve(process.env.RUNNER_TEMP ?? process.env.TMPDIR ?? '../opencode'), 'select-aliases-cli-'));
  try {
    writeFileSync(join(dir, 'docker'), DOCKER_STUB, { mode: 0o755 });
    writeFileSync(join(dir, 'git'), GIT_STUB, { mode: 0o755 });
    const mockDir = join(dir, 'responses');
    mkdirSync(mockDir);
    for (const [alias, mock] of Object.entries(responses)) {
      const safe = `${IMAGE}:${alias}`.replace(/[/:]/g, '_');
      if ('json' in mock) writeFileSync(join(mockDir, `${safe}.json`), mock.json);
      else { writeFileSync(join(mockDir, `${safe}.err`), mock.err); if (mock.code) writeFileSync(join(mockDir, `${safe}.code`), String(mock.code)); }
    }
    const env = { ...process.env, PATH: `${dir}:${process.env.PATH}`, DOCKER_LOG: join(dir, 'docker.log'), GIT_LOG: join(dir, 'git.log'), MOCKDIR: mockDir };
    const result = spawnSync('node', [script('select-release-aliases.mjs'), IMAGE, version, NEW], { env, encoding: 'utf8' });
    return {
      result,
      dockerLog: existsSync(join(dir, 'docker.log')) ? readFileSync(join(dir, 'docker.log'), 'utf8') : '',
      gitLog: existsSync(join(dir, 'git.log')) ? readFileSync(join(dir, 'git.log'), 'utf8') : '',
    };
  } finally { rmSync(dir, { recursive: true }); }
};
const okJson = (version, revision) => ({ json: JSON.stringify(okIndex(version, revision)) });
const err = text => ({ err: text });

const cliCases = [
  ['c29 missing manifests assign all aliases, git never invoked', '1.10.1', { '1.10': { err: `ERROR: ${IMAGE}:1.10: not found` }, '1': { err: `ERROR: ${IMAGE}:1: not found` } },
    stdout => assert.deepEqual(stdout, [`${IMAGE}:1.10.1`, `${IMAGE}:1.10`, `${IMAGE}:1`, `${IMAGE}:sha-${NEW}`])],
  ['c30 equal version same SHA is idempotent', '1.10.1', { '1.10': okJson('1.10.1', NEW), '1': okJson('1.10.1', NEW) },
    stdout => assert.equal(stdout.length, 4)],
  ['c31 forward update moves lower alias', '1.10.1', { '1.10': okJson('1.10.0', OLD), '1': { err: `ERROR: ${IMAGE}:1: not found` } },
    stdout => assert.ok(stdout.includes(`${IMAGE}:1.10`))],
  ['c32 numeric precision: skip higher patch, keep lower minor line', '1.10.0', { '1.10': okJson('1.10.4', OLD), '1': okJson('1.9.9', OLD) },
    stdout => { assert.ok(!stdout.includes(`${IMAGE}:1.10`)); assert.ok(stdout.includes(`${IMAGE}:1`)); }],
  ['c33 same version different revision fails', '1.10.0', { '1.10': okJson('1.10.0', OLD), '1': okJson('1.10.0', OLD) }, null],
  ['c34 wrong major line fails', '1.10.0', { '1.10': okJson('1.10.0', NEW), '1': okJson('2.0.0', OLD) }, null],
  ['c35 wrong minor line fails', '1.10.0', { '1.10': okJson('1.9.0', OLD), '1': okJson('1.9.0', OLD) }, null],
  ['c36 prerelease registry content on stable alias fails', '1.10.0', { '1.10': okJson('1.10.0-rc.1', NEW), '1': okJson('1.9.9', OLD) }, null],
  ['c37 extra platform config fails', '1.10.0', { '1.10': { json: JSON.stringify({ ...okIndex('1.10.0', NEW), 'linux/arm/v7': platform('1.10.0', NEW, 'arm') }) }, '1': okJson('1.9.9', OLD) }, null],
  ['c38 inconsistent platform revisions fail', '1.10.0', { '1.10': { json: JSON.stringify(index(platform('1.10.0', NEW), platform('1.10.0', OLD, 'arm64'))) }, '1': okJson('1.9.9', OLD) }, null],
  ['c39 short revision fails', '1.10.0', { '1.10': okJson('1.10.0', 'abc1234'), '1': okJson('1.9.9', OLD) }, null],
  ['c40 successful null JSON fails as malformed, not missing', '1.10.0', { '1.10': { json: 'null' }, '1': okJson('1.9.9', OLD) }, null],
  ['c41 malformed JSON fails', '1.10.0', { '1.10': { json: '{"linux/amd64":' }, '1': okJson('1.9.9', OLD) }, null],
  ['c42 auth denial fails closed', '1.10.0', { '1.10': okJson('1.10.0', NEW), '1': err('ERROR: denied: requested access to the resource is denied') }, null],
  ['c43 network failure fails closed', '1.10.0', { '1.10': err('ERROR: Get "https://ghcr.io/v2/": dial tcp: i/o timeout'), '1': okJson('1.9.9', OLD) }, null],
  ['c44 unrelated not-found text fails closed', '1.10.0', { '1.10': err('ERROR: credential helper docker-credential-ghcr not found'), '1': okJson('1.9.9', OLD) }, null],
];
for (const [name, version, responses, check] of cliCases) test(name, () => {
  const { result, gitLog } = runCli(version, responses);
  assert.equal(gitLog, '', 'floating ownership must never query Git tags');
  if (check) {
    assert.equal(result.status, 0, result.stderr);
    check(result.stdout.trim().split('\n'));
  } else assert.notEqual(result.status, 0, 'expected fail closed');
});
test('c45 prerelease CLI: exact+SHA only, zero docker or git calls', () => {
  const dir = mkdtempSync(join(resolve(process.env.RUNNER_TEMP ?? process.env.TMPDIR ?? '../opencode'), 'select-aliases-pre-'));
  try {
    writeFileSync(join(dir, 'docker'), DOCKER_STUB, { mode: 0o755 });
    writeFileSync(join(dir, 'git'), GIT_STUB, { mode: 0o755 });
    mkdirSync(join(dir, 'responses'));
    const env = { ...process.env, PATH: `${dir}:${process.env.PATH}`, DOCKER_LOG: join(dir, 'docker.log'), GIT_LOG: join(dir, 'git.log'), MOCKDIR: join(dir, 'responses') };
    const result = spawnSync('node', [script('select-release-aliases.mjs'), IMAGE, '1.11.0-rc.1', NEW], { env, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(result.stdout.trim().split('\n'), [`${IMAGE}:1.11.0-rc.1`, `${IMAGE}:sha-${NEW}`]);
    assert.ok(!existsSync(join(dir, 'docker.log')));
    assert.ok(!existsSync(join(dir, 'git.log')));
  } finally { rmSync(dir, { recursive: true }); }
});
test('c46 newer unpublished Git tags cannot suppress registry-authoritative updates', () => {
  const { result, dockerLog, gitLog } = runCli('1.10.0', { '1.10': { err: `ERROR: ${IMAGE}:1.10: not found` }, '1': { err: `ERROR: ${IMAGE}:1: not found` } });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(gitLog, '', 'even an unpublished newer tag cannot affect ownership');
  assert.ok(result.stdout.includes(`${IMAGE}:1.10`));
  assert.match(dockerLog, new RegExp(`inspect ${IMAGE}:1.10`));
  assert.match(dockerLog, new RegExp(`inspect ${IMAGE}:1`));
});
test('c47 usage error fails', () => {
  const result = spawnSync('node', [script('select-release-aliases.mjs'), IMAGE, '1.10.0'], { encoding: 'utf8' });
  assert.notEqual(result.status, 0);
});

// c48 workflow structure (string checks, no YAML parser): serialized alias job only
test('c48 release-aliases job serializes alias writes after publish without main', () => {
  const yaml = readFileSync(script('../workflows/container_publish.yml'), 'utf8');
  const job = yaml.slice(yaml.indexOf('  release-aliases:\n'));
  assert.ok(job.length > 0);
  assert.match(job, /if: github\.repository == 'cnrd\/ebc-battery-tester' && github\.event_name == 'push' && startsWith\(github\.ref, 'refs\/tags\/v'\)/);
  assert.ok(!job.includes('refs/heads/main'), 'main must not enter the serialized alias job');
  assert.match(job, /needs: publish/);
  assert.ok(yaml.indexOf('concurrency:', yaml.indexOf('  release-aliases:')) < yaml.indexOf('steps:', yaml.indexOf('  release-aliases:')));
  assert.match(job, /cancel-in-progress: false/);
  assert.match(job, /queue: max/);
  assert.match(job, /group: container-release-aliases-\$\{\{ contains\(github\.ref_name, '-'\) && github\.ref \|\| 'stable' \}\}/);
  assert.match(job, /check-release-image\.sh "\$IMAGE" "\$VERSION" "\$GITHUB_SHA"/);
  const check = job.indexOf('check-release-image.sh');
  const select = job.indexOf('select-release-aliases.mjs');
  const create = job.indexOf('imagetools create');
  assert.ok(check < select && select < create, 'exact guard, then registry-authoritative select, then single create');
});

for (const [candidate, expected] of [
  ['1.2.3', ['1.2.3', `sha-${NEW}`]],
  ['1.2.5', ['1.2.5', '1.2', `sha-${NEW}`]],
  ['1.4.0', ['1.4.0', '1.4', '1', `sha-${NEW}`]],
]) test(`registry 1.2=1.2.4, 1=1.3.0 selects ${candidate} correctly`, () => {
  const registry = { '1.2': okIndex('1.2.4', OLD), '1': okIndex('1.3.0', OLD) };
  assert.deepEqual(selectReleaseAliases(candidate, NEW, alias => registry[alias] ?? null), expected);
});

test('v0 minor moves forward but an old patch rerun cannot roll it back', () => {
  assert.deepEqual(selectReleaseAliases('0.6.5', NEW, () => okIndex('0.6.4', OLD)), ['0.6.5', '0.6', `sha-${NEW}`]);
  assert.deepEqual(selectReleaseAliases('0.6.3', NEW, () => okIndex('0.6.4', OLD)), ['0.6.3', `sha-${NEW}`]);
});
test('same major-scope version requires candidate revision, independently of missing minor alias', () => {
  assert.deepEqual(selectReleaseAliases('1.2.4', NEW, alias => alias === '1' ? okIndex('1.2.4', NEW) : null), ['1.2.4', '1.2', '1', `sha-${NEW}`]);
  assert.throws(() => selectReleaseAliases('1.2.4', NEW, alias => alias === '1' ? okIndex('1.2.4', OLD) : null), /different revision/);
});
test('missing version/revision labels and unknown config entries fail closed', () => {
  for (const label of [VERSION, REVISION]) {
    const configs = okIndex('1.2.4', NEW);
    delete configs['linux/arm64'].config.Labels[label];
    assert.throws(() => publishedVersion(configs), /Missing or malformed/);
  }
  assert.throws(() => publishedVersion({ ...okIndex('1.2.4', NEW), 'unknown/unknown': platform('999.0.0', OLD, 'unknown', 'unknown') }));
});
test('patch comparison retains precision beyond Number.MAX_SAFE_INTEGER', () => {
  assert.equal(compareStableVersions('1.2.9007199254740993', '1.2.9007199254740992'), 1);
});
test('successful CLI skips both newer aliases without blocking exact release', () => {
  const { result } = runCli('1.2.3', { '1.2': okJson('1.2.4', OLD), '1': okJson('1.3.0', OLD) });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(result.stdout.trim().split('\n'), [`${IMAGE}:1.2.3`, `${IMAGE}:sha-${NEW}`]);
  assert.match(result.stderr, /Skipping floating alias 1\.2:.*1\.2\.4/);
  assert.match(result.stderr, /Skipping floating alias 1:.*1\.3\.0/);
});
test('major alias auth failure is actually reached and fails before emitting tags', () => {
  const { result, dockerLog } = runCli('1.2.4', { '1.2': okJson('1.2.3', OLD), '1': err('ERROR: denied: requested access to the resource is denied') });
  assert.notEqual(result.status, 0);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /Cannot inspect floating alias/);
  assert.match(dockerLog, /inspect ghcr\.io\/cnrd\/ebc-battery-tester:1 --format/);
});
