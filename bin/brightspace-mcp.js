#!/usr/bin/env node

import { createWriteStream, existsSync, chmodSync, mkdirSync } from 'node:fs';
import { get } from 'node:https';
import { homedir, tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { spawn } from 'node:child_process';
import { createGunzip } from 'node:zlib';
import { createRequire } from 'node:module';
import { pipeline } from 'node:stream/promises';

const require = createRequire(import.meta.url);
const { version } = require('../package.json');
const triples = {
  'linux-x64': ['x86_64-unknown-linux-gnu', 'brightspace-mcp'],
  'linux-arm64': ['aarch64-unknown-linux-gnu', 'brightspace-mcp'],
  'darwin-x64': ['x86_64-apple-darwin', 'brightspace-mcp'],
  'darwin-arm64': ['aarch64-apple-darwin', 'brightspace-mcp'],
  'win32-x64': ['x86_64-pc-windows-msvc', 'brightspace-mcp.exe'],
  'win32-arm64': ['aarch64-pc-windows-msvc', 'brightspace-mcp.exe']
};

const target = triples[`${process.platform}-${process.arch}`];
if (!target) {
  console.error(`Unsupported platform: ${process.platform}-${process.arch}`);
  process.exit(1);
}

const [triple, executable] = target;
const root = join(homedir(), '.brightspace-mcp-rs', 'bin', version, triple);
const binary = join(root, executable);
const asset = `brightspace-mcp-${triple}.tar.gz`;
const url = `https://github.com/Exhabition/brightspace-mcp-rs/releases/download/v${version}/${asset}`;

async function download(url, destination, redirects = 0) {
  if (redirects > 5) throw new Error('Too many download redirects');
  await new Promise((resolve, reject) => {
    get(url, response => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
        response.resume();
        return download(response.headers.location, destination, redirects + 1).then(resolve, reject);
      }
      if (response.statusCode !== 200) {
        response.resume();
        return reject(new Error(`Download failed: HTTP ${response.statusCode}`));
      }
      pipeline(response, createGunzip(), createWriteStream(destination)).then(resolve, reject);
    }).on('error', reject);
  });
}

async function ensureBinary() {
  if (existsSync(binary)) return;
  mkdirSync(root, { recursive: true, mode: 0o700 });
  const archive = join(tmpdir(), `brightspace-mcp-${process.pid}.tar.gz`);
  try {
    await download(url, archive);
    const tar = process.platform === 'win32' ? 'tar.exe' : 'tar';
    await new Promise((resolve, reject) => {
      const child = spawn(tar, ['-xzf', archive, '-C', root], { stdio: 'inherit' });
      child.on('error', reject);
      child.on('exit', code => code === 0 ? resolve() : reject(new Error(`tar exited with ${code}`)));
    });
    if (!existsSync(binary)) throw new Error(`Release archive did not contain ${executable}`);
    if (process.platform !== 'win32') chmodSync(binary, 0o700);
  } finally {
    try { (await import('node:fs/promises')).unlink(archive); } catch {}
  }
}

try {
  await ensureBinary();
  const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', env: process.env });
  child.on('error', error => { console.error(error.message); process.exitCode = 1; });
  child.on('exit', (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
} catch (error) {
  console.error(`Could not start brightspace-mcp: ${error.message}`);
  console.error(`Expected release asset: ${url}`);
  process.exit(1);
}
