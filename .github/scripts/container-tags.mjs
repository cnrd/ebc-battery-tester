import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

// Explicit aliases: metadata-action must never infer ownership of latest.
export function containerTags(ref, sha) {
  if (!/^[a-f0-9]{40}$/.test(sha)) throw new Error('Expected a full 40-character Git SHA');
  const tags = [];
  if (ref === 'refs/heads/main') {
    tags.push('latest', 'main');
  } else {
    const match = /^refs\/tags\/v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(ref);
    if (!match) throw new Error('Expected main or a vX.Y.Z[-prerelease] release tag (no build metadata)');
    const [, major, minor, , prerelease] = match;
    if (prerelease?.split('.').some(id => /^\d+$/.test(id) && id.length > 1 && id.startsWith('0'))) {
      throw new Error('Numeric prerelease identifiers must not have leading zeros');
    }
    const version = ref.slice('refs/tags/v'.length);
    if (version.length > 128) throw new Error('Release version exceeds registry tag length');
    tags.push(version);
    if (!prerelease) {
      tags.push(`${major}.${minor}`);
      if (major !== '0') tags.push(major);
    }
  }
  tags.push(`sha-${sha}`);
  return tags;
}

export function packageVersion(cargoToml) {
  const packageSection = /^\[package\]\s*\n([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(cargoToml)?.[1];
  const version = /^version\s*=\s*"([^"]+)"\s*(?:#.*)?$/m.exec(packageSection ?? '')?.[1];
  if (!version) throw new Error('Expected an explicit Cargo.toml package version');
  return version;
}

export function validateCargoVersion(ref, sha, cargoToml) {
  containerTags(ref, sha);
  if (!ref.startsWith('refs/tags/v')) throw new Error('Expected a release tag');
  const version = packageVersion(cargoToml);
  if (version !== ref.slice('refs/tags/v'.length)) {
    throw new Error(`Version mismatch: Cargo.toml package version ${version} vs ${ref}`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [command, ref, sha] = process.argv.slice(2);
  if (command === 'tags') console.log(containerTags(ref, sha).join('\n'));
  else if (command === 'version') console.log(packageVersion(readFileSync('Cargo.toml', 'utf8')));
  else if (command === 'validate-release') validateCargoVersion(ref, sha, readFileSync('Cargo.toml', 'utf8'));
  else throw new Error('Usage: container-tags.mjs version OR (tags|validate-release) <full ref> <full SHA>');
}
