// base64 ↔ bytes 工具，用于终端数据的二进制传输。

const LOOKUP = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

export function bytesToBase64(bytes: Uint8Array): string {
  let out = "";
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out += LOOKUP[(n >> 18) & 63];
    out += LOOKUP[(n >> 12) & 63];
    out += LOOKUP[(n >> 6) & 63];
    out += LOOKUP[n & 63];
  }
  const rem = bytes.length - i;
  if (rem === 1) {
    const n = bytes[i] << 16;
    out += LOOKUP[(n >> 18) & 63];
    out += LOOKUP[(n >> 12) & 63];
    out += "==";
  } else if (rem === 2) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8);
    out += LOOKUP[(n >> 18) & 63];
    out += LOOKUP[(n >> 12) & 63];
    out += LOOKUP[(n >> 6) & 63];
    out += "=";
  }
  return out;
}

export function base64ToBytes(b64: string): Uint8Array {
  const clean = b64.replace(/[^A-Za-z0-9+/]/g, "");
  const len = clean.length;
  const bytesLen = (len * 3) >> 2;
  const out = new Uint8Array(bytesLen);
  let p = 0;
  for (let i = 0; i < len; i += 4) {
    const c0 = rev(clean.charCodeAt(i));
    const c1 = rev(clean.charCodeAt(i + 1));
    const c2 = i + 2 < len ? rev(clean.charCodeAt(i + 2)) : 0;
    const c3 = i + 3 < len ? rev(clean.charCodeAt(i + 3)) : 0;
    const n = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
    if (p < bytesLen) out[p++] = (n >> 16) & 255;
    if (p < bytesLen) out[p++] = (n >> 8) & 255;
    if (p < bytesLen) out[p++] = n & 255;
  }
  return out;
}

const REV_MAP: Record<number, number> = (() => {
  const m: Record<number, number> = {};
  for (let i = 0; i < LOOKUP.length; i++) m[LOOKUP.charCodeAt(i)] = i;
  return m;
})();

function rev(code: number): number {
  return REV_MAP[code] ?? 0;
}
