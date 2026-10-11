import { execFileSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { containerTags, parseReleaseVersion } from './container-tags.mjs';

const VERSION_LABEL = 'org.opencontainers.image.version';
const REVISION_LABEL = 'org.opencontainers.image.revision';
const PLATFORMS = ['linux/amd64', 'linux/arm64'];

export function publishedVersion(configs) {
  if (!configs || typeof configs !== 'object' || Array.isArray(configs) ||
      Object.keys(configs).sort().join(',') !== PLATFORMS.join(',')) {
    throw new Error('Expected exactly linux/amd64 and linux/arm64 runtime configs');
  }
  const metadata = PLATFORMS.map(platform => {
    const image = configs[platform];
    if (image?.os !== 'linux' || image?.architecture !== platform.split('/')[1]) {
      throw new Error(`Invalid runtime platform config: ${platform}`);
    }
    const labels = image.config?.Labels;
    const version = labels?.[VERSION_LABEL];
    const revision = labels?.[REVISION_LABEL];
    if (typeof version !== 'string' || typeof revision !== 'string' || !/^[a-f0-9]{40}$/.test(revision)) {
      throw new Error(`Missing or malformed OCI version/revision labels: ${platform}`);
    }
    const parsed = parseReleaseVersion(version);
    if (parsed.prerelease) throw new Error('Stable floating alias contains a prerelease');
    return { version, revision, parsed };
  });
  if (metadata.some(item => item.version !== metadata[0].version || item.revision !== metadata[0].revision)) {
    throw new Error('Inconsistent OCI version/revision labels across runtime platforms');
  }
  return metadata[0];
}

export function compareStableVersions(left, right) {
  const a = parseReleaseVersion(left);
  const b = parseReleaseVersion(right);
  if (a.prerelease || b.prerelease) throw new Error('Floating aliases require stable versions');
  for (const key of ['major', 'minor', 'patch']) {
    const difference = BigInt(a[key]) - BigInt(b[key]);
    if (difference !== 0n) return difference > 0n ? 1 : -1;
  }
  return 0;
}

// Registry state alone determines ownership; never enumerate Git tags.
// Returns exact + eligible floating + full-SHA tags from the canonical policy.
export function selectReleaseAliases(version, sha, inspectAlias, log = () => {}) {
  const possible = containerTags(`refs/tags/v${version}`, sha);
  const candidate = parseReleaseVersion(version);
  if (candidate.prerelease) return possible;
  return possible.filter(alias => {
    if (alias === version || alias === `sha-${sha}`) return true;
    const configs = inspectAlias(alias);
    if (configs === null) return true; // Only a proven missing manifest is null.
    const current = publishedVersion(configs);
    if (current.parsed.major !== candidate.major ||
        (alias.includes('.') && current.parsed.minor !== candidate.minor)) {
      throw new Error(`Floating alias ${alias} contains unexpected version line ${current.version}`);
    }
    const comparison = compareStableVersions(version, current.version);
    if (comparison === 0 && current.revision !== sha) {
      throw new Error(`Floating alias ${alias} reports ${version} at a different revision`);
    }
    if (comparison < 0) {
      log(`Skipping floating alias ${alias}: current published version ${current.version} is newer than candidate ${version}.`);
      return false;
    }
    return true;
  });
}

export function inspectRegistryAlias(image, alias, run = execFileSync) {
  const reference = `${image}:${alias}`;
  let output;
  try {
    output = run('docker', ['buildx', 'imagetools', 'inspect', reference, '--format', '{{json .Image}}'], {
      encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
    });
  } catch (error) {
    const message = String(error.stderr ?? '');
    // Accept only a manifest-not-found error, not arbitrary "not found" text
    // (e.g. a missing credential helper, auth endpoint or network resource).
    const missing = new Set([
      `ERROR: ${reference}: not found`,
      `ERROR: ${reference}: manifest unknown`,
      'ERROR: manifest unknown',
      'ERROR: manifest unknown: manifest unknown',
    ]).has(message.trim());
    if (error.status > 0 && missing) return null;
    throw new Error(`Cannot inspect floating alias ${reference}: ${message || error.message}`);
  }
  try {
    const configs = JSON.parse(output);
    if (!configs || typeof configs !== 'object' || Array.isArray(configs)) throw new Error('Invalid configs');
    return configs;
  } catch {
    throw new Error(`Malformed registry config response for ${reference}`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [image, version, sha] = process.argv.slice(2);
  if (!image || !version || !sha) throw new Error('Usage: select-release-aliases.mjs IMAGE VERSION SHA');
  const selected = selectReleaseAliases(version, sha, alias => inspectRegistryAlias(image, alias), message => console.error(message));
  console.log(selected.map(alias => `${image}:${alias}`).join('\n'));
}
