import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

// Portable workflow structure tests: plain string checks, no YAML parser dependency.
// actionlint (where installed) covers schema separately; these guard invariants
// the container publication policy depends on.

const workflow = name => readFileSync(fileURLToPath(new URL(`../workflows/${name}`, import.meta.url)), 'utf8');
const publish = workflow('container_publish.yml');
const platform = workflow('container_publish_platform.yml');
const validate = workflow('container_validate.yml');

// Slice a job body: from its two-space header up to the next job header or EOF.
const job = (yaml, name) => {
  const start = yaml.indexOf(`\n  ${name}:\n`);
  assert.notEqual(start, -1, `job ${name} missing`);
  const rest = yaml.slice(start + 1);
  const next = rest.slice(1).search(/\n {2}[A-Za-z_][\w-]*:\n/);
  return next === -1 ? rest : rest.slice(0, next + 1);
};
const step = (yaml, needle) => {
  const at = yaml.indexOf(needle);
  assert.notEqual(at, -1, `step ${needle} missing`);
  const rest = yaml.slice(at);
  const next = rest.slice(1).search(/\n {6}- /);
  return next === -1 ? rest : rest.slice(0, next + 1);
};
const ascending = (yaml, needles, message) => {
  const positions = needles.map(n => {
    const at = yaml.indexOf(n);
    assert.notEqual(at, -1, `expected ${n}`);
    return at;
  });
  assert.ok(positions.every((p, i) => i === 0 || p > positions[i - 1]), message);
};

const GUARD = "github.repository == 'cnrd/ebc-battery-tester' && github.event_name == 'push'";

// n01-n04 explicit named native build jobs, no matrix, no output collision
test('n01 publish declares explicit named publish-amd64 and publish-arm64 jobs delegating to the reusable platform workflow', () => {
  for (const [name, arch] of [['publish-amd64', 'amd64'], ['publish-arm64', 'arm64']]) {
    const j = job(publish, name);
    assert.match(j, new RegExp(`uses: \\./\\.github/workflows/container_publish_platform\\.yml`));
    assert.match(j, new RegExp(`with:\\n\\s+arch: ${arch}\\b`));
    assert.ok(!j.includes('matrix'), 'named per-arch jobs must not reintroduce a matrix');
    assert.match(j, new RegExp(GUARD.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  }
});
test('n02 publish workflow carries no build matrix at all', () => {
  assert.ok(!publish.includes('matrix'), 'native builds are explicit named jobs, not matrix output slots');
});
test('n03 finalizer consumes each platform digest through its named job needs output', () => {
  const j = job(publish, 'publish');
  assert.match(j, /needs: \[publish-amd64, publish-arm64\]/);
  assert.match(j, /needs\.publish-amd64\.outputs\.digest/);
  assert.match(j, /needs\.publish-arm64\.outputs\.digest/);
});
test('n04 reusable workflow exports one digest output per invocation without collision', () => {
  assert.match(platform, /on:\n  workflow_call:/);
  assert.match(platform, /inputs:\n      arch:\n        required: true\n        type: string/);
  assert.match(platform, /outputs:\n      digest:\n        value: \$\{\{ jobs\.platform\.outputs\.digest \}\}/);
  assert.match(job(platform, 'platform'), /outputs:\n      digest: \$\{\{ steps\.push\.outputs\.digest \}\}/);
});

// n05-n08 finalizer: assemble by digest, verify, attest, then alias
test('n05 finalizer assembles the index with imagetools create and never rebuilds platform images', () => {
  const j = job(publish, 'publish');
  assert.match(j, /docker buildx imagetools create/);
  assert.ok(!j.includes('build-push-action'), 'the finalizer must not run build-push-action');
  assert.match(j, /"\$IMAGE@\$AMD64_DIGEST" "\$IMAGE@\$ARM64_DIGEST"/);
  assert.match(j, /\[\[ "\$AMD64_DIGEST" =~ \^sha256:/);
  assert.match(j, /\[\[ "\$ARM64_DIGEST" =~ \^sha256:/);
  assert.match(j, /test "\$AMD64_DIGEST" != "\$ARM64_DIGEST"/);
});
test('n06 finalizer verifies the pushed index with check-container-manifest before emitting its digest', () => {
  ascending(publish,
    ['docker buildx imagetools create',
      'imagetools inspect "$IMAGE@$DIGEST" --raw',
      '--format \'{{json .Image}}\'',
      'check-container-manifest.mjs',
      'echo "digest=$DIGEST"'],
    'index must be created, inspected raw and configs, verified, then published as digest');
  const check = step(publish, 'check-container-manifest.mjs');
  assert.match(check, /"\$AMD64_DIGEST" "\$ARM64_DIGEST" "\$GITHUB_SHA" "\$VERSION"/);
});
test('n07 provenance attests the combined manifest digest after verification', () => {
  const attest = step(publish, 'actions/attest@');
  assert.match(attest, /subject-name: \$\{\{ env\.IMAGE \}\}/);
  assert.match(attest, /subject-digest: \$\{\{ steps\.push\.outputs\.digest \}\}/);
  assert.match(attest, /push-to-registry: true/);
  assert.ok(publish.indexOf('check-container-manifest.mjs') < publish.indexOf('actions/attest@'));
  const j = job(publish, 'publish');
  assert.match(j, /attestations: write/);
  assert.match(j, /id-token: write/);
});
test('n08 channel aliases point at the attested digest by copy, with no platform-specific tags', () => {
  const alias = step(publish, 'Assign main aliases only after provenance succeeds');
  assert.ok(publish.indexOf('actions/attest@') < publish.indexOf('Assign main aliases'), 'aliases move only after provenance');
  assert.match(alias, /DIGEST: \$\{\{ steps\.push\.outputs\.digest \}\}/);
  assert.match(alias, /docker buildx imagetools create "\$\{TAG_ARGS\[@\]\}" "\$IMAGE@\$DIGEST"/);
  assert.ok(!/IMAGE:(amd64|arm64)\b/.test(publish), 'no platform-suffixed public tags');
  assert.ok(!publish.includes('linux/amd64') && !publish.includes('linux/arm64'), 'platform strings belong to the platform workflow only');
});

// n09-n10 PR/fork/wip caller gate unchanged
test('n09 every publish job keeps the push-only repository trust gate', () => {
  for (const name of ['publish-amd64', 'publish-arm64', 'publish']) {
    assert.match(job(publish, name), new RegExp(`if: ${GUARD.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} && \\(github\\.ref == 'refs/heads/main' \\|\\| startsWith\\(github\\.ref, 'refs/tags/v'\\)\\)`));
  }
});
test('n10 obsolete-main and tag-scoped alias gates remain unchanged', () => {
  assert.match(publish, /git ls-remote origin refs\/heads\/main/);
  assert.match(publish, /test "\$MAIN_SHA" = "\$GITHUB_SHA"/);
  const aliases = job(publish, 'release-aliases');
  assert.match(aliases, new RegExp(`if: ${GUARD.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} && startsWith\\(github\\.ref, 'refs/tags/v'\\)`));
  assert.ok(!aliases.includes('refs/heads/main'), 'main must not enter the serialized alias job');
  assert.match(aliases, /needs: publish/, 'existing select-release-aliases expectations stay valid');
});

// n11-n16 native platform workflow invariants
test('n11 native runner labels selected per input, two supported architectures only', () => {
  assert.match(platform, /runs-on: \$\{\{ inputs\.arch == 'arm64' && 'ubuntu-24\.04-arm' \|\| 'ubuntu-24\.04' \}\}/);
});
test('n12 native runner architecture guard fails closed on mismatch', () => {
  const guard = step(platform, 'Verify native runner architecture');
  assert.match(guard, /amd64:x86_64\|arm64:aarch64/);
  assert.match(guard, /exit 1/);
});
test('n13 native images push by digest only, never platform tags', () => {
  const push = step(platform, 'Push native platform image by digest only');
  assert.match(push, /outputs: type=image,name=\$\{\{ env\.IMAGE \}\},push-by-digest=true,name-canonical=true,push=true/);
  assert.ok(!/\n\s+tags:/.test(push), 'the native push step must not assign public tags');
  assert.ok(!platform.includes('"$IMAGE:'), 'the platform workflow never pushes or references a tagged image reference');
});
test('n14 BuildKit provenance and SBOM disabled: GitHub attests the combined index', () => {
  const push = step(platform, 'Push native platform image by digest only');
  assert.match(push, /provenance: false/);
  assert.match(push, /sbom: false/);
});
test('n15 build caches are scoped per architecture', () => {
  assert.match(platform, /cache-from: type=gha,scope=container-\$\{\{ inputs\.arch \}\}/);
  assert.match(platform, /cache-to: type=gha,mode=max,scope=container-\$\{\{ inputs\.arch \}\}/);
});
test('n16 identical logical OCI metadata on both platforms including created, no implicit latest', () => {
  const meta = step(platform, 'OCI metadata (no implicit latest)');
  assert.match(meta, /flavor: latest=false/);
  for (const label of ['source=https://github.com/cnrd/ebc-battery-tester', 'revision=${{ github.sha }}',
    'version=${{ steps.policy.outputs.version }}', 'created=${{ steps.policy.outputs.created }}',
    'licenses=MIT', 'description=Headless EBC Battery Tester server with remote browser UI'])
    assert.ok(meta.includes(label), `missing label ${label}`);
  assert.match(platform, /created=\$\(git show -s --format=%cI "\$GITHUB_SHA"\)/);
  assert.ok(platform.indexOf('created=$(git show') < platform.indexOf('id: meta'), 'created comes from the policy step the meta step consumes');
});

// n17-n19 emulation ban and validation workflow parity
for (const [name, yaml] of [['publish', publish], ['publish_platform', platform], ['validate', validate]])
  test(`n17 ${name} never uses QEMU or emulation tooling`, () => {
    assert.ok(!/qemu/i.test(yaml));
    assert.ok(!yaml.includes('docker/setup-qemu-action'));
    assert.ok(!yaml.includes('binfmt'));
    assert.ok(!yaml.includes('multiarch/'), 'no multiarch emulation helper images');
    assert.ok(!yaml.includes('--platform linux/arm/v7'), 'only the two native amd64/arm64 platforms are built');
  });
test('n18 validate smoke builds run natively per arch and never push', () => {
  assert.match(validate, /arch: amd64\n\s+runner: ubuntu-24\.04\b/);
  assert.match(validate, /arch: arm64\n\s+runner: ubuntu-24\.04-arm/);
  assert.match(validate, /amd64:x86_64\|arm64:aarch64/);
  assert.match(validate, /load: true/);
  assert.match(validate, /push: false/);
  assert.match(validate, /scope=container-\$\{\{ matrix\.arch \}\}/);
});
test('n19 tag policy is generated by container-tags before metadata in both publishing workflows', () => {
  for (const yaml of [publish, platform]) {
    assert.ok(yaml.includes('container-tags.mjs tags "$GITHUB_REF" "$GITHUB_SHA"'));
    assert.ok(yaml.includes('container-tags.mjs version'));
    assert.ok(yaml.indexOf('container-tags.mjs tags') < yaml.indexOf('docker/metadata-action'), 'tags flow into metadata-action');
  }
});

test('finalizer extracts the actual imagetools metadata descriptor digest, not the build-action metadata key', () => {
  const expression = /DIGEST=\$\(jq -er '([^']+)'/.exec(job(publish, 'publish'))?.[1];
  assert.ok(expression, 'finalizer must parse imagetools metadata');
  const digest = `sha256:${'c'.repeat(64)}`;
  const extract = input => spawnSync('jq', ['-er', expression], { input: JSON.stringify(input), encoding: 'utf8' });
  const result = extract({ 'containerimage.descriptor': { mediaType: 'application/vnd.oci.image.index.v1+json', digest, size: 645 }, 'image.name': 'fixture' });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), digest);
  assert.notEqual(extract({ 'containerimage.digest': digest }).status, 0, 'wrong metadata shape must fail closed');
  assert.notEqual(extract({}).status, 0, 'missing descriptor must fail closed');
});
