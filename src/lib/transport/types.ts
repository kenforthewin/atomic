export interface Transport {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  subscribe<T>(event: string, callback: (payload: T) => void): () => void;
  connect(): Promise<void>;
  disconnect(): void;
  isConnected(): boolean;
  readonly mode: 'http';
  onConnectionChange?: (connected: boolean) => void;
}

export interface HttpTransportConfig {
  baseUrl: string;
  authToken: string;
  /**
   * Cloud-tenant mode: authenticate by the same-origin session cookie
   * (`credentials: 'include'`, no `Authorization` header, no `?token=` on the
   * WebSocket) instead of a bearer token. Set only when the product app is
   * served on an Atomic Cloud tenant subdomain (detected via the
   * server-injected `atomic-cloud-tenant` meta). `baseUrl` is `''` in this
   * mode (same origin). Tauri and self-hosted-web leave this unset and use
   * the existing bearer-token flow unchanged.
   */
  cookieAuth?: boolean;
}
