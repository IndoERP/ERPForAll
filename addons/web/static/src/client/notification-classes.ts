/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from './transport.js';

/** Bounded addon CSS hooks; never CSS source, markup or an action identity. */
export function notificationClasses(value: unknown, owner: 'notification' | 'onchange warning'): string {
  const invalid = (reason: string): never => { throw new ClientError(owner === 'notification' ? 'validation' : 'server', `Invalid ${owner} className: ${reason}`); };
  if (typeof value !== 'string') return invalid('expected text');
  // The character guard bounds encoder work even for a hostile multi-byte input.
  if (value.length > 512 || new TextEncoder().encode(value).byteLength > 512) return invalid('exceeds 512 UTF-8 bytes');
  const tokens = value.split(' ').filter(token => token !== '');
  if (tokens.some(token => !/^[A-Za-z_][A-Za-z0-9_-]*$/.test(token))) return invalid('tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces');
  if (tokens.length > 8) return invalid('expected at most 8 tokens');
  const seen = new Set<string>();
  for (const token of tokens) {
    if (token.length > 64) return invalid('token exceeds 64 bytes');
    if (seen.has(token)) return invalid(`duplicate token '${token}'`);
    seen.add(token);
  }
  return tokens.join(' ');
}
