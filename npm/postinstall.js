#!/usr/bin/env node
'use strict';

// Shared postinstall for all @travsr-plugin/<lang> packages.
// Reads the package name from package.json in the caller's directory,
// derives the binary name, downloads from the tagged GitHub Release,
// verifies SHA256, and writes the binary to ./bin/<name>.
//
// Optional: if the package bundles a share/<binaryName>/ directory, it is
// also copied to ~/.travsr/share/<binaryName>/ so that sidecar binaries
// placed directly in ~/.travsr/bin/ (by `travsr lang install`) can resolve
// their emitter-path via the path-3 heuristic in emitter_path().
// Languages that ship no share/ dir (go, java, …) are unaffected.

const https = require('https');
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');

// When npm runs a postinstall script it sets cwd to the package directory,
// so __dirname and process.cwd() both point there.
const pkg = require(path.join(process.cwd(), 'package.json'));
// "@travsr-plugin/go" -> "travsr-lang-go"
const lang = pkg.name.split('/')[1];
const binaryName = `travsr-lang-${lang}`;
const version = pkg.version;
const REPO = 'Travsr-com/travsr-lang';

// Must match the release matrix in .github/workflows/release.yml. A target
// missing here makes `npm i @travsr-plugin/<lang>` decline to fetch a binary
// that the release does publish (travsr#588).
function platformTarget() {
  const { platform, arch } = process;
  if (platform === 'darwin' && arch === 'x64') return 'x86_64-apple-darwin';
  if (platform === 'darwin' && arch === 'arm64') return 'aarch64-apple-darwin';
  if (platform === 'linux' && arch === 'x64') return 'x86_64-unknown-linux-gnu';
  if (platform === 'linux' && arch === 'arm64') return 'aarch64-unknown-linux-gnu';
  if (platform === 'win32' && arch === 'x64') return 'x86_64-pc-windows-msvc';
  return null;
}

// `.exe` on Windows, for both the release asset name and the file written to
// disk. The same rule as .github/scripts/package-wrappers.sh and travsr's
// installer: an extensionless file on Windows is not resolvable through
// PATHEXT, so getting the download right and the write wrong still leaves an
// unusable install.
function exeSuffix(target) {
  return target.includes('windows') ? '.exe' : '';
}

function fetch(url) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    https
      .get(url, { headers: { 'User-Agent': 'travsr-plugin-postinstall' } }, res => {
        if (res.statusCode === 301 || res.statusCode === 302) {
          fetch(res.headers.location).then(resolve, reject);
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode}: ${url}`));
          return;
        }
        res.on('data', c => chunks.push(c));
        res.on('end', () => resolve(Buffer.concat(chunks)));
        res.on('error', reject);
      })
      .on('error', reject);
  });
}

async function main() {
  const target = platformTarget();
  if (!target) {
    console.warn(
      `travsr-plugin: ${binaryName} has no binary for ${process.platform}/${process.arch}, Phase B unavailable on this platform`
    );
    return;
  }

  const suffix = exeSuffix(target);
  const assetName = `${binaryName}-${target}${suffix}`;
  const base = `https://github.com/${REPO}/releases/download/v${version}`;

  console.log(`travsr-plugin: downloading ${assetName} …`);
  const [binary, sha256Buf] = await Promise.all([
    fetch(`${base}/${assetName}`),
    fetch(`${base}/${assetName}.sha256`),
  ]);

  const expected = sha256Buf.toString('utf8').trim();
  const actual = crypto.createHash('sha256').update(binary).digest('hex');
  if (actual !== expected) {
    throw new Error(
      `SHA256 mismatch for ${assetName}\n  expected ${expected}\n  got      ${actual}`
    );
  }

  const binDir = path.join(process.cwd(), 'bin');
  fs.mkdirSync(binDir, { recursive: true });
  const dest = path.join(binDir, `${binaryName}${suffix}`);
  fs.writeFileSync(dest, binary, { mode: 0o755 });
  console.log(`travsr-plugin: ${binaryName}${suffix} installed`);

  // Install bundled share/<binaryName>/ to ~/.travsr/share/<binaryName>/ so
  // that sidecar binaries in ~/.travsr/bin/ resolve emitter_path() correctly.
  // This only applies to languages that bundle a share/ dir (e.g. dart, swift).
  const bundledShare = path.join(process.cwd(), 'share', binaryName);
  if (fs.existsSync(bundledShare)) {
    const travrsHome = process.env.TRAVSR_HOME || path.join(os.homedir(), '.travsr');
    const shareTarget = path.join(travrsHome, 'share', binaryName);
    try {
      copyDir(bundledShare, shareTarget);
      console.log(`travsr-plugin: emitter files installed to ${shareTarget}`);
    } catch (err) {
      console.warn(`travsr-plugin: share dir copy failed (non-fatal): ${err.message}`);
    }
    // Regenerate package_config.json with correct local paths if dart is available.
    const pubspec = path.join(shareTarget, 'pubspec.yaml');
    if (fs.existsSync(pubspec)) {
      try {
        const { execSync } = require('child_process');
        execSync('dart pub get', { cwd: shareTarget, stdio: 'pipe' });
        console.log(`travsr-plugin: dart pub get completed in ${shareTarget}`);
      } catch (_) {
        console.warn('travsr-plugin: dart pub get failed. The dart emitter requires dart on PATH at runtime');
      }
    }
  }
}

function copyDir(src, dst) {
  fs.mkdirSync(dst, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, entry.name);
    const d = path.join(dst, entry.name);
    if (entry.isDirectory()) copyDir(s, d);
    else fs.copyFileSync(s, d);
  }
}

main().catch(err => {
  console.error(`travsr-plugin install error: ${err.message}`);
  process.exit(1);
});
