import type { Transport, HttpTransportConfig } from './types';
import { HttpTransport } from './http';
import { useUIStore } from '../../stores/ui';
import { syncSharedConfig, clearSharedConfig } from '../mobile/shared-config';
export type { Transport, HttpTransportConfig };

let activeTransport: Transport | null = null;
let localServerConfig: HttpTransportConfig | null = null;

export const TRANSPORT_CHANGED_EVENT = 'atomic:transport-changed';
export const TRANSPORT_CONNECTION_EVENT = 'atomic:transport-connection';

function dispatchTransportChanged(): void {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(new CustomEvent(TRANSPORT_CHANGED_EVENT));
}

function dispatchTransportConnection(connected: boolean): void {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(new CustomEvent(TRANSPORT_CONNECTION_EVENT, {
    detail: { connected },
  }));
}

function wireConnectionCallback(transport: Transport): void {
  (transport as HttpTransport).onConnectionChange = (connected) => {
    useUIStore.getState().setServerConnected(connected);
    dispatchTransportConnection(connected);
  };
}

function connectInBackground(transport: Transport): void {
  void transport.connect().catch((err) => {
    console.error('Transport connection failed:', err);
  });
}

export function getTransport(): Transport {
  if (!activeTransport) throw new Error('Transport not initialized. Call initTransport() first.');
  return activeTransport;
}

/**
 * True when the product app is served on an Atomic Cloud tenant subdomain —
 * the cloud server injects `<meta name="atomic-cloud-tenant" content="true">`
 * into the product `index.html` it serves at the tenant root. In that case the
 * app authenticates by the same-origin session cookie (set by the cloud
 * dashboard login), so there's no server URL or token to configure. Self-hosted
 * and Tauri builds never carry the marker (the placeholder stays unreplaced),
 * so this is `false` and their existing flows are untouched.
 */
export function isCloudTenant(): boolean {
  if (typeof document === 'undefined') return false;
  const meta = document.querySelector('meta[name="atomic-cloud-tenant"]');
  return meta?.getAttribute('content') === 'true';
}

/** Resolved once during initTransport on cloud tenants; null everywhere else. */
let demoConfig: { signup_url: string } | null = null;

/**
 * True when this session is an ANONYMOUS visitor on the cloud's public demo
 * instance. The server tells us: `GET /api/demo-config` answers 200 only for
 * demo visitors (the logged-in demo operator and every real tenant get 404,
 * other unauthenticated hosts 401). UI uses this to render the demo chrome —
 * signup banner, chat CTA, read-only editor, hidden edit affordances. The
 * server-side whitelist is the actual enforcement; this flag is presentation.
 */
export function isDemoInstance(): boolean {
  return demoConfig !== null;
}

/** The signup CTA target the demo server advertises. */
export function demoSignupUrl(): string {
  return demoConfig?.signup_url ?? 'https://atomicapp.ai/cloud';
}

export async function initTransport(): Promise<void> {
  if (isCloudTenant()) {
    // Cloud tenant: same-origin, cookie-authenticated. No localStorage config,
    // no setup prompt — the dashboard session cookie is the credential.
    // Raw fetch (not the transport) so a 401/404 here can't trigger the
    // transport's auth-expired redirect.
    try {
      const res = await fetch('/api/demo-config');
      if (res.ok) {
        const body = (await res.json()) as { demo?: boolean; signup_url?: string };
        if (body.demo) demoConfig = { signup_url: body.signup_url ?? '' };
      }
    } catch {
      // Network hiccup: boot as a normal tenant; API calls will sort it out.
    }
    activeTransport = new HttpTransport({ baseUrl: '', authToken: '', cookieAuth: true });
    wireConnectionCallback(activeTransport);
    // Demo visitors skip the WebSocket: the server closes /ws to them
    // (403), and without a session there are no live events to receive —
    // connecting would just cycle the reconnect backoff forever.
    if (!isDemoInstance()) {
      connectInBackground(activeTransport);
    }
    return;
  }
  if (typeof window !== 'undefined' && (window as any).__TAURI_INTERNALS__) {
    // Desktop app: get sidecar config via single Tauri IPC command
    const { invoke } = await import('@tauri-apps/api/core');
    localServerConfig = await invoke<HttpTransportConfig>('get_local_server_config');

    // Check if user has saved a remote server config
    const saved = localStorage.getItem('atomic-server-config');
    const config = saved ? JSON.parse(saved) as HttpTransportConfig : localServerConfig;

    activeTransport = new HttpTransport(config);
    wireConnectionCallback(activeTransport);
    connectInBackground(activeTransport);
  } else {
    // Web SPA — require explicit config from localStorage or prompt user
    const saved = localStorage.getItem('atomic-server-config');
    if (saved) {
      const config: HttpTransportConfig = JSON.parse(saved);
      activeTransport = new HttpTransport(config);
      wireConnectionCallback(activeTransport);
      connectInBackground(activeTransport);
      void syncSharedConfig({ serverURL: config.baseUrl, apiToken: config.authToken });
    } else {
      // Create a disconnected HttpTransport — user must configure via settings
      activeTransport = new HttpTransport({ baseUrl: '', authToken: '' });
    }
  }
}

/// Switch to a remote server (saves config to localStorage)
export async function switchTransport(config: HttpTransportConfig): Promise<void> {
  if (activeTransport) activeTransport.disconnect();
  activeTransport = new HttpTransport(config);
  wireConnectionCallback(activeTransport);
  await activeTransport.connect();
  localStorage.setItem('atomic-server-config', JSON.stringify(config));
  void syncSharedConfig({ serverURL: config.baseUrl, apiToken: config.authToken });
  dispatchTransportChanged();
}

/// Switch back to the local sidecar server (desktop only)
export async function switchToLocal(): Promise<void> {
  if (!localServerConfig) {
    throw new Error('No local server config available — not running in desktop app');
  }
  if (activeTransport) activeTransport.disconnect();
  activeTransport = new HttpTransport(localServerConfig);
  wireConnectionCallback(activeTransport);
  await activeTransport.connect();
  localStorage.removeItem('atomic-server-config');
  void clearSharedConfig();
  dispatchTransportChanged();
}

/// True when running inside the Tauri desktop app (sidecar available)
export function isDesktopApp(): boolean {
  return localServerConfig !== null;
}

/// True when connected to the embedded local sidecar (not a remote server)
export function isLocalServer(): boolean {
  if (!localServerConfig || !activeTransport) return false;
  const currentConfig = (activeTransport as HttpTransport).getConfig();
  return currentConfig.baseUrl === localServerConfig.baseUrl;
}

/// Get the local server config (for MCP setup display, etc.)
export function getLocalServerConfig(): HttpTransportConfig | null {
  return localServerConfig;
}

/// Get the resolved path to the bundled atomic-mcp-bridge binary (desktop only).
export async function getMcpBridgePath(): Promise<string | null> {
  if (!isDesktopApp()) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<string>('get_mcp_bridge_path');
  } catch {
    return null;
  }
}
