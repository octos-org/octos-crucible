import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { crc32, deflateRawSync } from 'node:zlib';
import { checkAndExtract, ZipFile, ZipSafetyError, validate } from '../image/src/zip.ts';

interface Item {
  name: string;
  data?: string | Buffer;
  mode?: number; // unix mode for external attributes
  deflate?: boolean;
  badCrc?: boolean;
}

/** Minimal zip writer for tests. */
function makeZip(items: Item[]): Buffer {
  const locals: Buffer[] = [];
  const centrals: Buffer[] = [];
  let offset = 0;
  for (const it of items) {
    const name = Buffer.from(it.name, 'utf8');
    const raw = Buffer.isBuffer(it.data) ? it.data : Buffer.from(it.data ?? '');
    const body = it.deflate ? deflateRawSync(raw) : raw;
    const crc = (crc32(raw) ^ (it.badCrc ? 1 : 0)) >>> 0;
    const method = it.deflate ? 8 : 0;
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0);
    lh.writeUInt16LE(20, 4);
    lh.writeUInt16LE(0x800, 6);
    lh.writeUInt16LE(method, 8);
    lh.writeUInt32LE(crc, 14);
    lh.writeUInt32LE(body.length, 18);
    lh.writeUInt32LE(raw.length, 22);
    lh.writeUInt16LE(name.length, 26);
    locals.push(lh, name, body);
    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0);
    ch.writeUInt16LE((3 << 8) | 20, 4);
    ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(0x800, 8);
    ch.writeUInt16LE(method, 10);
    ch.writeUInt32LE(crc, 16);
    ch.writeUInt32LE(body.length, 20);
    ch.writeUInt32LE(raw.length, 24);
    ch.writeUInt16LE(name.length, 28);
    ch.writeUInt32LE(((it.mode ?? (it.name.endsWith('/') ? 0o40755 : 0o100644)) << 16) >>> 0, 38);
    ch.writeUInt32LE(offset, 42);
    centrals.push(ch, name);
    offset += 30 + name.length + body.length;
  }
  const cd = Buffer.concat(centrals);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(items.length, 8);
  eocd.writeUInt16LE(items.length, 10);
  eocd.writeUInt32LE(cd.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, eocd]);
}

function withTmp(fn: (dir: string) => void): void {
  const dir = mkdtempSync(join(tmpdir(), 'zip-test-'));
  try {
    fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function unpack(items: Item[]): { dir: string; error?: Error } & { cleanup: () => void } {
  const dir = mkdtempSync(join(tmpdir(), 'zip-test-'));
  const zip = join(dir, 'app.zip');
  writeFileSync(zip, makeZip(items));
  let error: Error | undefined;
  try {
    checkAndExtract(zip, join(dir, 'out'));
  } catch (e) {
    error = e as Error;
  }
  return { dir, error, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

test('extracts stored and deflated entries, without restoring permission bits', () => {
  const r = unpack([
    { name: 'Dockerfile', data: 'FROM scratch\n' },
    { name: 'src/', mode: 0o40755 },
    { name: 'src/start.sh', data: '#!/bin/sh\necho hi\n'.repeat(50), deflate: true, mode: 0o100755 },
    { name: './a//b/c.txt', data: 'c' },
  ]);
  try {
    assert.equal(r.error, undefined);
    const out = join(r.dir, 'out');
    assert.equal(readFileSync(join(out, 'Dockerfile'), 'utf8'), 'FROM scratch\n');
    assert.equal(readFileSync(join(out, 'src/start.sh'), 'utf8'), '#!/bin/sh\necho hi\n'.repeat(50));
    assert.equal(readFileSync(join(out, 'a/b/c.txt'), 'utf8'), 'c');
    // Python's extractall does not restore the exec bit; neither do we.
    assert.equal(statSync(join(out, 'src/start.sh')).mode & 0o111, 0);
  } finally {
    r.cleanup();
  }
});

for (const [label, items] of [
  ['parent traversal', [{ name: '../evil', data: 'x' }]],
  ['nested traversal', [{ name: 'a/../../evil', data: 'x' }]],
  ['absolute path', [{ name: '/etc/evil', data: 'x' }]],
  ['symlink', [{ name: 'link', data: '/etc/passwd', mode: 0o120777 }]],
  ['bad CRC', [{ name: 'f', data: 'hello', badCrc: true }]],
] as [string, Item[]][]) {
  test(`rejects ${label}`, () => {
    const r = unpack(items);
    try {
      assert.ok(r.error instanceof ZipSafetyError, String(r.error));
      assert.equal(existsSync(join(r.dir, 'evil')), false);
    } finally {
      r.cleanup();
    }
  });
}

test('rejects too many files and too many bytes', () => {
  const many = Array.from({ length: 5 }, (_, i) => ({ name: `f${i}`, data: 'x' }));
  assert.throws(() => validate(new ZipFile(makeZip(many)), { maxFiles: 4 }), /more than 4 files/);
  validate(new ZipFile(makeZip(many)), { maxFiles: 5 });
  const big = [{ name: 'big', data: Buffer.alloc(2 * 1024 * 1024), deflate: true }];
  assert.throws(() => validate(new ZipFile(makeZip(big)), { maxTotalBytes: 1024 * 1024 }), /more than 1 MB/);
});

test('directory entries do not count as files', () => {
  const items = [{ name: 'd/' }, { name: 'd/e/' }, { name: 'f', data: 'x' }];
  validate(new ZipFile(makeZip(items)), { maxFiles: 1 });
});

test('rejects non-zip input', () => {
  withTmp((dir) => {
    const p = join(dir, 'x.zip');
    writeFileSync(p, 'not a zip at all, definitely not');
    assert.throws(() => checkAndExtract(p, join(dir, 'out')), ZipSafetyError);
  });
});

test('Dockerfile requirement is opt-in', () => {
  const zf = new ZipFile(makeZip([{ name: 'app/Dockerfile', data: 'x' }]));
  validate(zf);
  assert.throws(() => validate(zf, { requireDockerfile: true }), /Dockerfile at its root/);
});
