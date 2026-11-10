// Settings persisted in this browser's localStorage only. Nothing is sent
// anywhere except the signed actions to the configured API.

export const DEFAULT_API_URL: string =
  (import.meta.env.VITE_API_URL as string | undefined) ?? "http://127.0.0.1:3001";

const K_API = "hlbft.apiUrl";
const K_KEY = "hlbft.privateKey";

function get(k: string): string | null {
  try {
    return localStorage.getItem(k);
  } catch {
    return null;
  }
}
function set(k: string, v: string | null) {
  try {
    if (v === null) localStorage.removeItem(k);
    else localStorage.setItem(k, v);
  } catch {
    /* storage unavailable: settings live in memory only */
  }
}

export function initialApiUrl(): string {
  const q = new URLSearchParams(location.search).get("api");
  if (q) return normalizeApiUrl(q);
  return get(K_API) ?? DEFAULT_API_URL;
}
export function saveApiUrl(url: string) {
  set(K_API, url);
}

export function storedKey(): string | null {
  return get(K_KEY);
}
export function saveKey(key: string | null) {
  set(K_KEY, key);
}

export function normalizeApiUrl(u: string): string {
  let s = u.trim();
  if (!/^https?:\/\//.test(s)) s = "http://" + s;
  return s.replace(/\/+$/, "");
}

/** http://host:port -> ws://host:port/ws */
export function wsUrlFor(api: string): string {
  return api.replace(/^http/, "ws") + "/ws";
}

export function normalizeKey(k: string): `0x${string}` | null {
  const s = k.trim().replace(/^0x/i, "");
  if (!/^[0-9a-fA-F]{64}$/.test(s)) return null;
  return `0x${s.toLowerCase()}`;
}
