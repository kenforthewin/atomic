const CONFIG_KEY = 'serverConfig';
const DEFAULT_URL = 'http://localhost:44380';

/**
 * Normalize a user-entered server URL to an absolute origin. A scheme-less
 * value ("you.atomicapp.ai") would otherwise be passed to fetch() as a
 * RELATIVE url — resolved against the extension page itself
 * (chrome-extension://…/options/you.atomicapp.ai/api/…) and failing with
 * ERR_FILE_NOT_FOUND. Local hosts default to http, everything else https.
 */
export function normalizeServerUrl(raw) {
  const url = (raw || '').trim().replace(/\/+$/, '');
  if (!url || /^https?:\/\//i.test(url)) return url;
  const isLocal = /^(localhost|127\.0\.0\.1|\[::1\])([:/]|$)/i.test(url);
  return `${isLocal ? 'http' : 'https'}://${url}`;
}

export async function getConfig() {
  const result = await chrome.storage.local.get(CONFIG_KEY);
  const config = result[CONFIG_KEY] || { serverUrl: DEFAULT_URL, apiToken: '', database: '' };
  // Heal configs saved before normalization existed.
  return { ...config, serverUrl: normalizeServerUrl(config.serverUrl) };
}

export async function setConfig(config) {
  await chrome.storage.local.set({ [CONFIG_KEY]: config });
}

export function authHeaders(apiToken, database) {
  const headers = { 'Content-Type': 'application/json' };
  if (apiToken) headers['Authorization'] = `Bearer ${apiToken}`;
  if (database) headers['X-Atomic-Database'] = database;
  return headers;
}
