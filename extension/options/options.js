import { getConfig, setConfig, authHeaders, normalizeServerUrl } from '../lib/config.js';

const urlInput = document.getElementById('server-url');
const tokenInput = document.getElementById('api-token');
const databaseSelect = document.getElementById('database');
const refreshDbsBtn = document.getElementById('refresh-dbs');
const dbHint = document.getElementById('db-hint');
const saveBtn = document.getElementById('save');
const testBtn = document.getElementById('test');
const messageEl = document.getElementById('message');

function readForm() {
  return {
    serverUrl: normalizeServerUrl(urlInput.value),
    apiToken: tokenInput.value.trim(),
    database: databaseSelect.value.trim(),
  };
}

function showMessage(text, type) {
  messageEl.textContent = text;
  messageEl.className = `message ${type}`;
  messageEl.style.display = 'block';
  setTimeout(() => { messageEl.style.display = 'none'; }, 3000);
}

function renderDatabases(databases, selectedId) {
  databaseSelect.innerHTML = '';
  databaseSelect.appendChild(new Option('(server default)', ''));
  for (const db of databases) {
    const label = `${db.name} — ${db.id}${db.is_default ? ' [default]' : ''}`;
    databaseSelect.appendChild(new Option(label, db.id));
  }
  if (selectedId && !databases.some((d) => d.id === selectedId)) {
    databaseSelect.appendChild(new Option(`${selectedId} (not found on server)`, selectedId));
  }
  databaseSelect.value = selectedId || '';
}

async function refreshDatabases({ silent = false } = {}) {
  const { serverUrl, apiToken, database } = readForm();
  if (!serverUrl) {
    if (!silent) showMessage('Server URL is required', 'error');
    return;
  }

  refreshDbsBtn.disabled = true;
  try {
    const res = await fetch(`${serverUrl}/api/databases`, { headers: authHeaders(apiToken) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const { databases = [] } = await res.json();
    renderDatabases(databases, database);
    dbHint.textContent = `Loaded ${databases.length} database(s).`;
  } catch (err) {
    if (!silent) showMessage(`Could not load databases: ${err.message}`, 'error');
    dbHint.textContent = `Could not reach server (${err.message}). Save to persist current selection anyway.`;
  } finally {
    refreshDbsBtn.disabled = false;
  }
}

async function loadConfig() {
  const config = await getConfig();
  urlInput.value = config.serverUrl;
  tokenInput.value = config.apiToken;
  renderDatabases([], config.database);
  refreshDatabases({ silent: true });
}

saveBtn.addEventListener('click', async () => {
  const form = readForm();
  if (!form.serverUrl) {
    showMessage('Server URL is required', 'error');
    return;
  }
  await setConfig(form);
  urlInput.value = form.serverUrl;
  showMessage('Settings saved', 'success');
});

testBtn.addEventListener('click', async () => {
  const { serverUrl, apiToken, database } = readForm();
  if (!serverUrl) {
    showMessage('Server URL is required', 'error');
    return;
  }

  testBtn.disabled = true;
  testBtn.textContent = 'Testing...';
  try {
    const res = await fetch(`${serverUrl}/api/atoms?limit=1`, {
      headers: authHeaders(apiToken, database)
    });
    if (res.ok) {
      showMessage(`Connection successful — using database: ${database || '(server default)'}`, 'success');
    } else if (res.status === 401) {
      showMessage('Connected but token is invalid', 'error');
    } else if (res.status === 400) {
      showMessage('Connected but database not found — pick one from the list', 'error');
    } else {
      showMessage(`Connection failed: HTTP ${res.status}`, 'error');
    }
  } catch (err) {
    showMessage(`Connection failed: ${err.message}`, 'error');
  } finally {
    testBtn.disabled = false;
    testBtn.textContent = 'Test Connection';
  }
});

refreshDbsBtn.addEventListener('click', () => refreshDatabases());

loadConfig();
