export function isBrowser(): boolean {
  return typeof globalThis.window !== "undefined" && typeof globalThis.document !== "undefined";
}
