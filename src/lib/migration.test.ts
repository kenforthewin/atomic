import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.fn();

vi.mock('./transport', () => ({
  getTransport: () => ({ invoke: invokeMock }),
}));

import { pushDatabaseToCloud, type MigrationJob } from './api';

function job(overrides: Partial<MigrationJob>): MigrationJob {
  return {
    id: 'job-1',
    kind: 'push',
    status: 'queued',
    phase: 'queued',
    db_name: 'My KB',
    total_rows: 100,
    processed_rows: 0,
    db_id: null,
    report: null,
    error: null,
    created_at: '2026-07-10T00:00:00Z',
    updated_at: '2026-07-10T00:00:00Z',
    completed_at: null,
    ...overrides,
  };
}

describe('pushDatabaseToCloud', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('polls the push job until complete and reports progress', async () => {
    invokeMock
      .mockResolvedValueOnce(job({ status: 'queued' }))
      .mockResolvedValueOnce(job({ status: 'running', phase: 'uploading', processed_rows: 40 }))
      .mockResolvedValueOnce(
        job({ status: 'complete', phase: 'complete', processed_rows: 100, db_id: 'remote-db' }),
      );

    const seen: string[] = [];
    const resultPromise = pushDatabaseToCloud(
      {
        targetUrl: 'https://cloud.example',
        targetToken: 'tok',
        databaseId: 'db-1',
        pauseFeeds: true,
      },
      j => seen.push(j.status),
    );

    await vi.advanceTimersByTimeAsync(1000);
    await vi.advanceTimersByTimeAsync(1000);
    const result = await resultPromise;

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'start_migration_push', {
      targetUrl: 'https://cloud.example',
      targetToken: 'tok',
      databaseId: 'db-1',
      pauseFeeds: true,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'get_migration_job', { id: 'job-1' });
    expect(result.db_id).toBe('remote-db');
    expect(seen).toEqual(['queued', 'running', 'complete']);
  });

  it('throws with the job error when the migration fails', async () => {
    invokeMock
      .mockResolvedValueOnce(job({ status: 'running' }))
      .mockResolvedValueOnce(job({ status: 'failed', error: 'remote rejected upload' }));

    const resultPromise = pushDatabaseToCloud({
      targetUrl: 'https://cloud.example',
      targetToken: 'tok',
      databaseId: 'db-1',
    });
    // Attach the rejection handler before advancing timers so the expected
    // failure is never an unhandled rejection.
    const assertion = expect(resultPromise).rejects.toThrow('remote rejected upload');
    await vi.advanceTimersByTimeAsync(1000);
    await assertion;
  });
});
