import { createHash, timingSafeEqual } from 'node:crypto';
import { BlockList, isIP } from 'node:net';

export const sha256 = (text: string): Buffer => createHash('sha256').update(text).digest();
export const base64url = (bytes: Uint8Array): string => Buffer.from(bytes).toString('base64url');

/** Two secrets, compared by their digests, which are the same length, in constant time. */
export const same = (a: string, b: string): boolean => timingSafeEqual(sha256(a), sha256(b));

/** Addresses and ranges (`203.0.113.7`, `10.0.0.0/8`, `2001:db8::/32`) as something to ask of. */
export function addressList(entries: string[], what = 'an address list'): (ip: string) => boolean {
  const list = new BlockList();
  for (const entry of entries) {
    const [address = '', prefix, ...extra] = String(entry).trim().split('/');
    const family = isIP(address);
    const length = prefix === undefined ? undefined : Number(prefix);
    const widest = family === 6 ? 128 : 32;
    if (!family || extra.length || (length !== undefined && (!Number.isInteger(length) || length < 0 || length > widest))) {
      throw new Error(`${what}: ${entry} is not an address or a range such as 10.0.0.0/8`);
    }
    const kind = family === 6 ? 'ipv6' : 'ipv4';
    if (length === undefined) list.addAddress(address, kind);
    else list.addSubnet(address, length, kind);
  }
  return (ip) => {
    const family = isIP(ip);
    return Boolean(family) && list.check(ip, family === 6 ? 'ipv6' : 'ipv4');
  };
}

const ADDRESS = /^[0-9a-fA-F:.]{2,45}$/;

/**
 * The address a request came from. With `trustProxy` proxies in front, it is the one that many places
 * from the right of `X-Forwarded-For`, which is the part each proxy writes: what a caller wrote
 * before that is not believed, and nonsense is taken to be the proxy itself.
 */
export function clientAddress(peer: string | undefined, forwarded: string | undefined, trustProxy: number): string {
  const direct = (peer ?? '').replace(/^::ffff:/, '');
  if (!trustProxy) return direct;
  const chain = String(forwarded ?? '')
    .split(',')
    .map((part) => part.trim())
    .filter(Boolean);
  const claimed = (chain[chain.length - trustProxy] ?? '').replace(/^::ffff:/, '');
  return ADDRESS.test(claimed) ? claimed : direct;
}
