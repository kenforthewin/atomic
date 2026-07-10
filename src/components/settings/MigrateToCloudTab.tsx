import { useEffect, useState } from 'react';
import { toast } from 'sonner';
import { Button } from '../ui/Button';
import { useDatabasesStore } from '../../stores/databases';
import { useSettingsStore } from '../../stores/settings';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { switchTransport, isDesktopApp, isLocalServer } from '../../lib/transport';
import {
  cancelMigrationJob,
  pushDatabaseToCloud,
  type MigrationJob,
} from '../../lib/api';

type TestResult = 'success' | 'error' | null;

/**
 * Push a local SQLite database to a remote (Postgres-backed) Atomic server.
 * The local server snapshots the database, uploads it, and mirrors the remote
 * import job's progress — this tab just watches one local job. Embeddings are
 * not uploaded; the remote server re-embeds with its own provider.
 */
export function MigrateToCloudTab() {
  const databases = useDatabasesStore(s => s.databases);
  const activeId = useDatabasesStore(s => s.activeId);
  const fetchDatabases = useDatabasesStore(s => s.fetchDatabases);
  const fetchSettings = useSettingsStore(s => s.fetchSettings);
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);

  const [databaseId, setDatabaseId] = useState<string>('');
  const [targetUrl, setTargetUrl] = useState('');
  const [targetToken, setTargetToken] = useState('');
  const [pauseFeeds, setPauseFeeds] = useState(true);
  const [testResult, setTestResult] = useState<TestResult>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [isTesting, setIsTesting] = useState(false);
  const [job, setJob] = useState<MigrationJob | null>(null);
  const [isMigrating, setIsMigrating] = useState(false);
  const [completedJob, setCompletedJob] = useState<MigrationJob | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetchDatabases();
  }, [fetchDatabases]);

  useEffect(() => {
    if (!databaseId && activeId) setDatabaseId(activeId);
  }, [activeId, databaseId]);

  const normalizedUrl = targetUrl.trim().replace(/\/$/, '');
  const connectedToRemote = isDesktopApp() && !isLocalServer();

  const handleTest = async () => {
    if (!normalizedUrl || !targetToken.trim()) return;
    setIsTesting(true);
    setTestResult(null);
    setTestError(null);
    try {
      const resp = await fetch(`${normalizedUrl}/health`);
      if (resp.ok) {
        setTestResult('success');
      } else {
        setTestResult('error');
        setTestError(`Server returned ${resp.status}`);
      }
    } catch (e) {
      setTestResult('error');
      setTestError(String(e));
    } finally {
      setIsTesting(false);
    }
  };

  const handleMigrate = async () => {
    if (!databaseId || !normalizedUrl || !targetToken.trim()) return;
    setIsMigrating(true);
    setCompletedJob(null);
    setError(null);
    try {
      const result = await pushDatabaseToCloud(
        {
          targetUrl: normalizedUrl,
          targetToken: targetToken.trim(),
          databaseId,
          pauseFeeds,
        },
        progress => setJob(progress),
      );
      setCompletedJob(result);
      toast.success('Migration complete', {
        description: `"${result.db_name}" now exists on ${normalizedUrl}`,
      });
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
      toast.error('Migration failed', { description: String(e) });
    } finally {
      setIsMigrating(false);
      setJob(null);
    }
  };

  const handleCancel = async () => {
    if (!job) return;
    try {
      await cancelMigrationJob(job.id);
    } catch (e) {
      toast.error('Failed to cancel migration', { description: String(e) });
    }
  };

  const handleSwitchToCloud = async () => {
    try {
      await switchTransport({ baseUrl: normalizedUrl, authToken: targetToken.trim() });
      fetchDatabases();
      fetchSettings();
      fetchAtoms();
      fetchTags();
      toast.success('Connected to cloud server');
    } catch (e) {
      toast.error('Failed to switch to the cloud server', { description: String(e) });
    }
  };

  const progressPercent =
    job && job.total_rows > 0
      ? Math.min(100, Math.round((job.processed_rows / job.total_rows) * 100))
      : null;

  const inputClass =
    'w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm';

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
          Migrate to Cloud
        </label>
        <p className="text-xs text-[var(--color-text-secondary)]">
          Copy a local database — notes, tags, wiki articles, chats, feeds, and reports — to a
          remote Atomic server. Embeddings are rebuilt on the server with its own AI provider,
          so search may take a few minutes to warm up after migrating. Your local database is
          left untouched.
        </p>
      </div>

      {connectedToRemote && (
        <p className="text-xs text-yellow-500">
          You are currently connected to a remote server. Switch back to the local server
          (Connection tab) to migrate a local database.
        </p>
      )}

      {!completedJob && (
        <div className="space-y-3">
          <div className="space-y-1">
            <label className="block text-xs font-medium text-[var(--color-text-secondary)]">
              Database
            </label>
            <select
              value={databaseId}
              onChange={e => setDatabaseId(e.target.value)}
              disabled={isMigrating}
              className={inputClass}
            >
              {databases.map(db => (
                <option key={db.id} value={db.id}>
                  {db.name}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1">
            <label className="block text-xs font-medium text-[var(--color-text-secondary)]">
              Cloud Server URL
            </label>
            <input
              type="text"
              value={targetUrl}
              onChange={e => {
                setTargetUrl(e.target.value);
                setTestResult(null);
              }}
              placeholder="https://atomic.example.com"
              disabled={isMigrating}
              className={inputClass}
            />
          </div>
          <div className="space-y-1">
            <label className="block text-xs font-medium text-[var(--color-text-secondary)]">
              API Token
            </label>
            <input
              type="password"
              value={targetToken}
              onChange={e => {
                setTargetToken(e.target.value);
                setTestResult(null);
              }}
              placeholder="Auth token for the cloud server"
              disabled={isMigrating}
              className={inputClass}
            />
          </div>
          <label className="flex items-center gap-2 text-xs text-[var(--color-text-secondary)]">
            <input
              type="checkbox"
              checked={pauseFeeds}
              onChange={e => setPauseFeeds(e.target.checked)}
              disabled={isMigrating}
              className="accent-[var(--color-accent)]"
            />
            Pause RSS feeds on the cloud server (recommended while this app keeps running)
          </label>

          {!isMigrating && (
            <div className="flex gap-2">
              <Button
                variant="secondary"
                onClick={handleTest}
                disabled={!normalizedUrl || !targetToken.trim() || isTesting}
              >
                {isTesting ? 'Testing...' : 'Test'}
              </Button>
              <Button
                onClick={handleMigrate}
                disabled={testResult !== 'success' || !databaseId || connectedToRemote}
              >
                Migrate
              </Button>
            </div>
          )}
          {testResult === 'success' && !isMigrating && (
            <div className="text-sm text-green-500">Server reachable</div>
          )}
          {testResult === 'error' && <div className="text-sm text-red-500">{testError}</div>}

          {isMigrating && (
            <div className="space-y-2">
              <div className="flex items-center justify-between text-xs text-[var(--color-text-secondary)]">
                <span>{job?.phase ?? 'starting'}</span>
                {progressPercent !== null && <span>{progressPercent}%</span>}
              </div>
              <div className="h-1.5 rounded-full bg-[var(--color-bg-main)] overflow-hidden">
                <div
                  className="h-full bg-[var(--color-accent)] transition-all duration-300"
                  style={{ width: `${progressPercent ?? 5}%` }}
                />
              </div>
              <Button variant="secondary" size="sm" onClick={handleCancel}>
                Cancel
              </Button>
            </div>
          )}
          {error && <div className="text-sm text-red-500">{error}</div>}
        </div>
      )}

      {completedJob && (
        <div className="space-y-3">
          <div className="text-sm text-green-500">
            Migrated {completedJob.processed_rows} rows to “{completedJob.db_name}”.
          </div>
          {(completedJob.report?.skipped_feed_urls.length ?? 0) > 0 && (
            <p className="text-xs text-[var(--color-text-secondary)]">
              Skipped feeds already subscribed on the server:{' '}
              {completedJob.report?.skipped_feed_urls.join(', ')}
            </p>
          )}
          <p className="text-xs text-[var(--color-text-secondary)]">
            The server is now rebuilding embeddings in the background. Keyword search works
            immediately; semantic search fills in as embedding completes.
          </p>
          <div className="flex gap-2">
            <Button onClick={handleSwitchToCloud}>Switch to Cloud Server</Button>
            <Button variant="secondary" onClick={() => setCompletedJob(null)}>
              Done
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
