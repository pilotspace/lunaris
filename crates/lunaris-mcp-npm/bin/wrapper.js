#!/usr/bin/env node
// wrapper.js — spawns the native lunaris-mcp binary with inherited stdio.
// This is the "bin" entry point for `npx @pilotspace/lunaris-mcp` and `lunaris-mcp` CLI.
// Trust: the binary path is either operator-set (LUNARIS_MCP_BIN_PATH) or
// resolved from this package's own bin/ directory (populated by postinstall.js).
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const PKG_DIR = join(__dirname, '..');
const BINARY_NAME = process.platform === 'win32' ? 'lunaris-mcp.exe' : 'lunaris-mcp';

// Air-gap / CI bypass: operator points at a pre-staged binary
const binaryPath = process.env.LUNARIS_MCP_BIN_PATH || join(PKG_DIR, 'bin', BINARY_NAME);

if (!existsSync(binaryPath)) {
  console.error(`[lunaris-mcp] Binary not found at ${binaryPath}`);
  console.error('Run: npm install @pilotspace/lunaris-mcp  (postinstall will download the binary)');
  console.error('Or set LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp');
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
child.on('error', (err) => {
  console.error(`[lunaris-mcp] Failed to start binary: ${err.message}`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  if (signal) process.exit(1);
  process.exit(code ?? 0);
});
