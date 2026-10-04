import { afterEach, describe, expect, it, vi } from 'vitest';
import { HttpTransport } from './http';

describe('HttpTransport cancellation', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('passes the caller signal to fetch', async () => {
    vi.stubGlobal('fetch', vi.fn((_url, init: RequestInit) => new Promise((_resolve, reject) => {
      init.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
    })));
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });
    (transport as unknown as { connected: boolean }).connected = true;
    const controller = new AbortController();

    const request = transport.invoke('search_global_keyword', { query: 'fictional' }, { signal: controller.signal });
    controller.abort();

    await expect(request).rejects.toMatchObject({ name: 'AbortError' });
  });
});
