// Zip safety check + extraction for the submitted web-app zip.
//
// Ported from the prototype grader's common/zipsafety.py and Python's
// zipfile.extractall so a zip is accepted or rejected for the same reasons
// and extracts to the same tree (including: no permission bits restored,
// "", "." and ".." path components dropped). Stdlib only (node:zlib).

import { mkdirSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve, sep } from 'node:path';
import { crc32, inflateRawSync } from 'node:zlib';

export const DEFAULT_MAX_FILES = 2000;
export const DEFAULT_MAX_TOTAL_BYTES = 50 * 1024 * 1024;

export class ZipSafetyError extends Error {}

export interface ZipEntry {
  name: string;
  rawName: Buffer;
  isDir: boolean;
  flags: number;
  method: number;
  crc: number;
  compressedSize: number;
  size: number;
  localHeaderOffset: number;
  externalAttr: number;
}

export interface ZipLimits {
  maxFiles?: number;
  maxTotalBytes?: number;
  requireDockerfile?: boolean;
}

const SIG_EOCD = 0x06054b50;
const SIG_EOCD64 = 0x06064b50;
const SIG_EOCD64_LOCATOR = 0x07064b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_LOCAL = 0x04034b50;

// CP437 upper half, which Python's zipfile uses for names without the UTF-8 flag.
const CP437_HIGH =
  'ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■ ';

function decodeName(raw: Buffer, flags: number): string {
  let name: string;
  if (flags & 0x800) {
    name = raw.toString('utf8');
  } else {
    name = '';
    for (const b of raw) name += b < 0x80 ? String.fromCharCode(b) : CP437_HIGH[b - 0x80];
  }
  // Python's ZipInfo truncates at the first NUL.
  const nul = name.indexOf('\0');
  return nul >= 0 ? name.slice(0, nul) : name;
}

export class ZipFile {
  readonly buf: Buffer;
  readonly entries: ZipEntry[];
  private readonly concat: number;

  constructor(buf: Buffer) {
    this.buf = buf;
    const { entries, concat } = parseCentralDirectory(buf);
    this.entries = entries;
    this.concat = concat;
  }

  static open(path: string): ZipFile {
    return new ZipFile(readFileSync(path));
  }

  read(e: ZipEntry): Buffer {
    const b = this.buf;
    const off = e.localHeaderOffset + this.concat;
    if (off < 0 || off + 30 > b.length || b.readUInt32LE(off) !== SIG_LOCAL) {
      throw new ZipSafetyError(`bad local header for ${JSON.stringify(e.name)}`);
    }
    const nameLen = b.readUInt16LE(off + 26);
    const extraLen = b.readUInt16LE(off + 28);
    const localName = b.subarray(off + 30, off + 30 + nameLen);
    if (!localName.equals(e.rawName)) {
      throw new ZipSafetyError(`file name in directory and header differ: ${JSON.stringify(e.name)}`);
    }
    if (e.flags & 0x1) throw new ZipSafetyError(`encrypted entry: ${JSON.stringify(e.name)}`);
    const start = off + 30 + nameLen + extraLen;
    const data = b.subarray(start, start + e.compressedSize);
    if (data.length !== e.compressedSize) throw new ZipSafetyError(`truncated entry: ${JSON.stringify(e.name)}`);
    let out: Buffer;
    if (e.method === 0) {
      out = Buffer.from(data);
    } else if (e.method === 8) {
      try {
        out = inflateRawSync(data, { maxOutputLength: Math.max(e.size, 1) });
      } catch (err) {
        throw new ZipSafetyError(`bad deflate data in ${JSON.stringify(e.name)}: ${(err as Error).message}`);
      }
    } else {
      throw new ZipSafetyError(`unsupported compression method ${e.method} in ${JSON.stringify(e.name)}`);
    }
    if (out.length !== e.size) throw new ZipSafetyError(`size mismatch in ${JSON.stringify(e.name)}`);
    if ((crc32(out) >>> 0) !== e.crc) throw new ZipSafetyError(`bad CRC-32 for ${JSON.stringify(e.name)}`);
    return out;
  }
}

function parseCentralDirectory(b: Buffer): { entries: ZipEntry[]; concat: number } {
  if (b.length < 22) throw new ZipSafetyError('not a zip file');
  let eocd = -1;
  const lowest = Math.max(0, b.length - 22 - 0xffff);
  for (let i = b.length - 22; i >= lowest; i--) {
    if (b.readUInt32LE(i) === SIG_EOCD && i + 22 + b.readUInt16LE(i + 20) <= b.length) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new ZipSafetyError('not a zip file');

  let count = b.readUInt16LE(eocd + 10);
  let cdSize = b.readUInt32LE(eocd + 12);
  let cdOffset = b.readUInt32LE(eocd + 16);
  let concat = eocd - cdSize - cdOffset;
  const loc = eocd - 20;
  if (loc >= 0 && b.readUInt32LE(loc) === SIG_EOCD64_LOCATOR) {
    const z = Number(b.readBigUInt64LE(loc + 8));
    if (z < 0 || z + 56 > b.length || b.readUInt32LE(z) !== SIG_EOCD64) {
      // Python tolerates a locator whose record sits elsewhere only via concat;
      // be strict instead: an inconsistent zip64 trailer is not a valid zip.
      const z2 = loc - 56;
      if (z2 < 0 || b.readUInt32LE(z2) !== SIG_EOCD64) throw new ZipSafetyError('corrupt zip64 end record');
      count = Number(b.readBigUInt64LE(z2 + 32));
      cdSize = Number(b.readBigUInt64LE(z2 + 40));
      cdOffset = Number(b.readBigUInt64LE(z2 + 48));
    } else {
      count = Number(b.readBigUInt64LE(z + 32));
      cdSize = Number(b.readBigUInt64LE(z + 40));
      cdOffset = Number(b.readBigUInt64LE(z + 48));
    }
    concat = eocd - cdSize - cdOffset - 56 - 20;
  }
  if (concat < 0) throw new ZipSafetyError('bad central directory offset');

  const entries: ZipEntry[] = [];
  let p = cdOffset + concat;
  const end = p + cdSize;
  while (p < end) {
    if (p + 46 > b.length || b.readUInt32LE(p) !== SIG_CENTRAL) throw new ZipSafetyError('bad central directory');
    const flags = b.readUInt16LE(p + 8);
    const method = b.readUInt16LE(p + 10);
    const crc = b.readUInt32LE(p + 16);
    let compressedSize = b.readUInt32LE(p + 20);
    let size = b.readUInt32LE(p + 24);
    const nameLen = b.readUInt16LE(p + 28);
    const extraLen = b.readUInt16LE(p + 30);
    const commentLen = b.readUInt16LE(p + 32);
    const externalAttr = b.readUInt32LE(p + 38);
    let localHeaderOffset = b.readUInt32LE(p + 42);
    const rawName = b.subarray(p + 46, p + 46 + nameLen);
    const extra = b.subarray(p + 46 + nameLen, p + 46 + nameLen + extraLen);
    // zip64 extended information extra field.
    for (let q = 0; q + 4 <= extra.length; ) {
      const id = extra.readUInt16LE(q);
      const len = extra.readUInt16LE(q + 2);
      if (id === 0x0001) {
        let r = q + 4;
        const take = (): number => {
          if (r + 8 > q + 4 + len) throw new ZipSafetyError('corrupt zip64 extra field');
          const v = Number(extra.readBigUInt64LE(r));
          r += 8;
          return v;
        };
        if (size === 0xffffffff) size = take();
        if (compressedSize === 0xffffffff) compressedSize = take();
        if (localHeaderOffset === 0xffffffff) localHeaderOffset = take();
      }
      q += 4 + len;
    }
    const name = decodeName(rawName, flags);
    entries.push({
      name,
      rawName: Buffer.from(rawName),
      isDir: name.endsWith('/'),
      flags,
      method,
      crc,
      compressedSize,
      size,
      localHeaderOffset,
      externalAttr,
    });
    p += 46 + nameLen + extraLen + commentLen;
    if (entries.length > count + 1_000_000) break; // paranoia
  }
  return { entries, concat };
}

/** `Path(name).parts` on POSIX: split on "/", drop empty and "." parts
 *  (keeping a leading "/" as its own part). */
function posixParts(name: string): string[] {
  const parts = name.split('/').filter((x) => x !== '' && x !== '.');
  return name.startsWith('/') ? ['/', ...parts] : parts;
}

/** Throw ZipSafetyError if the zip is unsafe to extract (same rules and
 *  order as zipsafety.validate). */
export function validate(zf: ZipFile, limits: ZipLimits = {}): void {
  const maxFiles = limits.maxFiles ?? DEFAULT_MAX_FILES;
  const maxTotal = limits.maxTotalBytes ?? DEFAULT_MAX_TOTAL_BYTES;
  const files = zf.entries.filter((e) => !e.isDir);
  if (files.length > maxFiles) throw new ZipSafetyError(`zip has more than ${maxFiles} files`);
  let total = 0;
  let hasDockerfile = false;
  for (const e of zf.entries) {
    if (e.name.startsWith('/') || posixParts(e.name).includes('..')) {
      throw new ZipSafetyError(`unsafe path in zip: ${JSON.stringify(e.name)}`);
    }
    const mode = (e.externalAttr >>> 16) & 0xffff;
    if (mode && (mode & 0o170000) === 0o120000) {
      throw new ZipSafetyError(`symlink not allowed: ${JSON.stringify(e.name)}`);
    }
    total += e.size;
    if (total > maxTotal) {
      throw new ZipSafetyError(`zip expands to more than ${Math.floor(maxTotal / (1024 * 1024))} MB`);
    }
    if (!e.isDir && e.name === 'Dockerfile') hasDockerfile = true;
  }
  if ((limits.requireDockerfile ?? false) && !hasDockerfile) {
    throw new ZipSafetyError('zip must contain a Dockerfile at its root');
  }
}

/** Extract a zip already proven safe by validate(), like Python's
 *  ZipFile.extractall: sanitised names, default permissions, later entries
 *  overwrite earlier ones. Still guards against escaping `dest`. */
export function extract(zf: ZipFile, dest: string): void {
  mkdirSync(dest, { recursive: true });
  const root = realpathSync(dest);
  for (const e of zf.entries) {
    const target = resolve(root, e.name);
    if (target !== root && !target.startsWith(root + sep)) {
      throw new ZipSafetyError(`path escapes destination: ${JSON.stringify(e.name)}`);
    }
  }
  for (const e of zf.entries) {
    const arc = e.name.split('/').filter((x) => x !== '' && x !== '.' && x !== '..');
    const target = arc.length ? join(root, ...arc) : root;
    if (e.isDir) {
      mkdirSync(target, { recursive: true });
      continue;
    }
    if (arc.length > 1) mkdirSync(join(root, ...arc.slice(0, -1)), { recursive: true });
    if (arc.length === 0) throw new ZipSafetyError(`empty file name: ${JSON.stringify(e.name)}`);
    writeFileSync(target, zf.read(e));
  }
}

export function checkAndExtract(zipPath: string, dest: string, limits: ZipLimits = {}): void {
  if (!statSync(zipPath).isFile()) throw new ZipSafetyError('not a zip file');
  const zf = ZipFile.open(zipPath);
  validate(zf, limits);
  extract(zf, dest);
}
