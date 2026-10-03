import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { parseReleaseVersion } from './container-tags.mjs';

const digestPattern = /^sha256:[a-f0-9]{64}$/;
const platforms = ['linux/amd64', 'linux/arm64'];
const indexTypes = new Set(['application/vnd.oci.image.index.v1+json', 'application/vnd.docker.distribution.manifest.list.v2+json']);
const imageTypes = new Set(['application/vnd.oci.image.manifest.v1+json', 'application/vnd.docker.distribution.manifest.v2+json']);

// Check the exact native outputs before attesting or assigning channel aliases.
// No registry writes here; callers supply registry-inspected JSON by digest.
export function checkContainerManifest(manifest, configs, amd64Digest, arm64Digest, sha, version) {
  if (!digestPattern.test(amd64Digest) || !digestPattern.test(arm64Digest) || amd64Digest === arm64Digest) {
    throw new Error('Expected two distinct platform SHA256 digests');
  }
  if (!/^[a-f0-9]{40}$/.test(sha)) throw new Error('Expected full commit SHA');
  parseReleaseVersion(version); // Main package version and release versions share syntax.
  if (manifest?.schemaVersion !== 2 || !indexTypes.has(manifest.mediaType) ||
      !Array.isArray(manifest.manifests) || manifest.manifests.length !== 2) {
    throw new Error('Expected a two-platform runtime image index');
  }
  const expectedDigests = { 'linux/amd64': amd64Digest, 'linux/arm64': arm64Digest };
  const seen = new Set();
  for (const descriptor of manifest.manifests) {
    const platform = `${descriptor.platform?.os}/${descriptor.platform?.architecture}`;
    if (!platforms.includes(platform) || !imageTypes.has(descriptor.mediaType) ||
        descriptor.digest !== expectedDigests[platform] || seen.has(platform)) {
      throw new Error(`Unexpected, duplicated or swapped runtime descriptor: ${platform}`);
    }
    seen.add(platform);
  }
  if (!configs || typeof configs !== 'object' || Array.isArray(configs) ||
      Object.keys(configs).sort().join(',') !== platforms.join(',')) {
    throw new Error('Expected exactly amd64 and arm64 runtime configs');
  }
  const requiredLabels = {
    'org.opencontainers.image.source': 'https://github.com/cnrd/ebc-battery-tester',
    'org.opencontainers.image.revision': sha,
    'org.opencontainers.image.version': version,
    'org.opencontainers.image.licenses': 'MIT',
    'org.opencontainers.image.description': 'Headless EBC Battery Tester server with remote browser UI',
  };
  const metadata = platforms.map(platform => {
    const config = configs[platform];
    if (config?.os !== 'linux' || config?.architecture !== platform.split('/')[1]) {
      throw new Error(`Incorrect native runtime config: ${platform}`);
    }
    const labels = config.config?.Labels;
    for (const [key, value] of Object.entries(requiredLabels)) {
      if (labels?.[key] !== value) throw new Error(`Missing or incorrect ${key}: ${platform}`);
    }
    return JSON.stringify(Object.entries(labels).filter(([key]) => key.startsWith('org.opencontainers.image.')).sort());
  });
  if (metadata[0] !== metadata[1]) throw new Error('Native platform OCI metadata differs');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [manifestPath, configsPath, amd64Digest, arm64Digest, sha, version] = process.argv.slice(2);
  checkContainerManifest(JSON.parse(readFileSync(manifestPath, 'utf8')), JSON.parse(readFileSync(configsPath, 'utf8')),
    amd64Digest, arm64Digest, sha, version);
  console.log('Native multiarchitecture manifest and OCI metadata verified');
}
