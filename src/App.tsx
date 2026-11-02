import { lazy, Suspense } from 'react';
import { Route, Routes } from 'react-router-dom';
import { Toaster } from 'sonner';
import { Layout } from './components/layout';
import { EmbeddingProgressBanner } from './components/ui/EmbeddingProgressBanner';
import { useEmbeddingEvents } from './hooks';

// Standalone /editor-harness page used during the CodeMirror editor
// migration to exercise the new editor against large markdown samples.
// Lazy-loaded so the main app bundle is unaffected.
const EditorHarnessPage = lazy(async () => {
  const mod = await import('./components/editor-harness/EditorHarnessPage');
  return { default: mod.EditorHarnessPage };
});

// Standalone /toc-harness page used while building the contextual sidebar's
// table of contents — exercises heading parsing, click-to-scroll, and scroll
// tracking against both rendered markdown and CodeMirror before the readers
// depend on any of it. Lazy-loaded so the main app bundle is unaffected.
const TocHarnessPage = lazy(async () => {
  const mod = await import('./components/toc-harness/TocHarnessPage');
  return { default: mod.TocHarnessPage };
});

const AgentationToolbar = import.meta.env.DEV
  ? lazy(async () => {
      const mod = await import('agentation');
      return { default: mod.Agentation };
    })
  : null;

function App() {
  // Initialize embedding event listener
  useEmbeddingEvents();

  return (
    <>
      <Toaster
        position="bottom-right"
        theme="dark"
        // Lift toasts above the iOS home indicator on Capacitor; no-op on
        // desktop where env(safe-area-inset-bottom) is 0.
        offset={{ bottom: 'calc(16px + env(safe-area-inset-bottom))', right: 'calc(16px + env(safe-area-inset-right))' }}
        toastOptions={{
          className: 'atomic-toast',
          duration: 5000,
        }}
      />
      <Routes>
        <Route
          path="/editor-harness"
          element={
            <Suspense fallback={null}>
              <EditorHarnessPage />
            </Suspense>
          }
        />
        <Route
          path="/toc-harness"
          element={
            <Suspense fallback={null}>
              <TocHarnessPage />
            </Suspense>
          }
        />
        <Route path="*" element={<Layout />} />
      </Routes>
      <EmbeddingProgressBanner />
      {AgentationToolbar && (
        <Suspense fallback={null}>
          <AgentationToolbar />
        </Suspense>
      )}
    </>
  );
}

export default App;
