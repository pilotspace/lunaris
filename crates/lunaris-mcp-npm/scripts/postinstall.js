#!/usr/bin/env node
// postinstall.js — downloads the platform-native lunaris-mcp binary on npm install.
// Trust boundary: sha256 verification against manifest.json before extraction.
// Security: abort with non-zero exit if sha256 mismatch. Extraction via tar(1).
// Windows note: requires tar.exe (Windows 10 Build 1803+, released April 2018).
//   Older systems: set LUNARIS_MCP_BIN_PATH to a manually downloaded binary instead.

import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream, existsSync, unlinkSync } from 'node:fs';
import { mkdir, readFile, chmod } from 'node:fs/promises';
import https from 'node:https';
import { spawn } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const __dirname = dirname(fileURLToPath(import.meta.url));
const PKG_DIR = join(__dirname, '..');

// Air-gap / CI bypass: if LUNARIS_MCP_BIN_PATH is set, the wrapper uses it directly.
// Postinstall exits 0 so CI environments with pre-staged binaries don't fail.
if (process.env.LUNARIS_MCP_BIN_PATH) {
  console.log('[lunaris-mcp] LUNARIS_MCP_BIN_PATH set — skipping postinstall download.');
  process.exit(0);
}

const PLATFORM_MAP = {
  'linux-x64':    'lunaris-mcp-x86_64-unknown-linux-gnu.tar.gz',
  'linux-arm64':  'lunaris-mcp-aarch64-unknown-linux-gnu.tar.gz',
  'darwin-x64':   'lunaris-mcp-x86_64-apple-darwin.tar.gz',
  'darwin-arm64': 'lunaris-mcp-aarch64-apple-darwin.tar.gz',
  'win32-x64':    'lunaris-mcp-x86_64-pc-windows-msvc.tar.gz',
};
const BINARY_NAME = process.platform === 'win32' ? 'lunaris-mcp.exe' : 'lunaris-mcp';

// Env-var overrides: for test harnesses ONLY. Production defaults must not change.
const BIN_DIR = process.env.LUNARIS_MCP_INSTALL_DIR || join(PKG_DIR, 'bin');
const BIN_PATH = join(BIN_DIR, BINARY_NAME);

// Idempotent: skip if binary already placed (e.g. reinstall after move)
if (existsSync(BIN_PATH)) {
  console.log('[lunaris-mcp] Binary already present, skipping download.');
  process.exit(0);
}

const key = `${process.platform}-${process.arch}`;
const tarball = PLATFORM_MAP[key];
if (!tarball) {
  console.error(`[lunaris-mcp] Unsupported platform: ${key}`);
  console.error('Install the Rust toolchain and run: cargo install lunaris-mcp');
  console.error('See: https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md');
  process.exit(1);
}

const pkg = JSON.parse(await readFile(join(PKG_DIR, 'package.json'), 'utf8'));
const version = pkg.version;

// Env-var override for manifest path (test harness only; production: PKG_DIR/manifest.json)
const manifestPath = process.env.LUNARIS_MCP_MANIFEST_PATH || join(PKG_DIR, 'manifest.json');
const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
const expectedHash = manifest[tarball];
if (!expectedHash || expectedHash === 'placeholder') {
  console.error(`[lunaris-mcp] No valid sha256 entry in manifest.json for ${tarball}`);
  console.error('If running from source, sha256 hashes are populated by release CI (mcp-prebuild.yml).');
  process.exit(1);
}

const ORG = 'pilotspace';
const REPO = 'lunaris';
// Env-var override for tarball URL (test harness only; production: GitHub Releases HTTPS URL)
const url = process.env.LUNARIS_MCP_TARBALL_URL
  || `https://github.com/${ORG}/${REPO}/releases/download/v${version}/${tarball}`;

console.log(`[lunaris-mcp] Downloading ${url}`);
await mkdir(BIN_DIR, { recursive: true });

// Step 1: Download to a temp file, computing sha256 during download (tee approach).
const tmpPath = join(tmpdir(), `lunaris-mcp-${Date.now()}.tar.gz`);
const hash = createHash('sha256');

if (url.startsWith('file://')) {
  // Test harness: read local file directly (file:// protocol for sha256_abort.sh)
  const filePath = url.replace(/^file:\/\//, '');
  const fileStream = createReadStream(filePath);
  const tmpWriter = createWriteStream(tmpPath);
  fileStream.on('data', (chunk) => {
    hash.update(chunk);
    tmpWriter.write(chunk);
  });
  fileStream.on('end', () => tmpWriter.end());
  await new Promise((resolve, reject) => {
    tmpWriter.on('finish', resolve);
    tmpWriter.on('error', reject);
    fileStream.on('error', reject);
  });
} else {
  // Production path: fetch from GitHub Releases over HTTPS, follow one redirect.
  await new Promise((resolve, reject) => {
    function handleResponse(res) {
      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode} fetching ${url}`));
        return;
      }
      const tmpWriter = createWriteStream(tmpPath);
      res.on('data', (chunk) => {
        hash.update(chunk);
        tmpWriter.write(chunk);
      });
      res.on('end', () => tmpWriter.end());
      tmpWriter.on('finish', resolve);
      tmpWriter.on('error', reject);
      res.on('error', reject);
    }

    function fetchUrl(targetUrl) {
      https.get(targetUrl, { headers: { 'User-Agent': `@pilotspace/lunaris-mcp@${version}` } }, (res) => {
        if (res.statusCode === 301 || res.statusCode === 302) {
          // Follow exactly one redirect (GitHub Releases -> CDN); all HTTPS.
          fetchUrl(res.headers.location);
        } else {
          handleResponse(res);
        }
      }).on('error', reject);
    }

    fetchUrl(url);
  });
}

// Step 2: Verify sha256 BEFORE extraction. If mismatch, delete temp and abort.
const actualHash = hash.digest('hex');
if (actualHash !== expectedHash) {
  try { unlinkSync(tmpPath); } catch {}
  console.error(`[lunaris-mcp] sha256 MISMATCH for ${tarball}`);
  console.error(`  expected: ${expectedHash}`);
  console.error(`  actual:   ${actualHash}`);
  console.error('Refusing to install. This may indicate a tampered artifact.');
  process.exit(1);
}

// Step 3: Extract via tar(1). A .tar.gz is gzip-compressed tar, NOT raw gzip.
// Piping through createGunzip() yields tar-format bytes — a file the OS cannot exec.
// spawn('tar', ['-xz', ...]) handles both decompression and tar-header stripping.
// Windows: tar.exe is required (Windows 10 Build 1803+, released April 2018).
// On older Windows systems, catch ENOENT and direct the user to the manual bypass.
const tarProc = spawn('tar', ['-xz', '-C', BIN_DIR, '--strip-components=0'], {
  stdio: ['pipe', 'inherit', 'inherit'],
});

tarProc.on('error', (err) => {
  if (err.code === 'ENOENT') {
    console.error(`
[lunaris-mcp postinstall] ERROR: 'tar' is not available.
  - On Windows: Windows 10 Build 1803+ ships tar.exe by default. Upgrade if older.
  - Workaround: install the binary manually, then set LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp before running 'npx @pilotspace/lunaris-mcp'.
  - GitHub Releases: https://github.com/pilotspace/lunaris/releases/tag/v${version}
`);
    process.exit(1);
  }
  console.error('[lunaris-mcp postinstall] tar process error:', err);
  process.exit(1);
});

createReadStream(tmpPath).pipe(tarProc.stdin);
await new Promise((resolve, reject) => {
  tarProc.on('close', code => code === 0 ? resolve() : reject(new Error(`tar exited ${code}`)));
});

// Step 4: Clean up temp tarball.
try { unlinkSync(tmpPath); } catch {}

// Step 5: Make binary executable on Unix.
if (process.platform !== 'win32') await chmod(BIN_PATH, 0o755);

console.log(`[lunaris-mcp] Binary installed to ${BIN_PATH}`);
