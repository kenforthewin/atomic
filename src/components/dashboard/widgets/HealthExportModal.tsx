import { createPortal } from 'react-dom';
import { X, Download } from 'lucide-react';
import { useEffect } from 'react';

// Minimal types needed for export
interface ExportHealthCheckResult {
  status: string;
  score: number;
  data: Record<string, unknown>;
}

interface ExportHealthReport {
  overall_score: number;
  overall_status: string;
  computed_at: string;
  atom_count: number;
  checks: Record<string, ExportHealthCheckResult>;
  auto_fixable: number;
  requires_review: number;
}

const CHECK_LABELS_EXPORT: Record<string, string> = {
  embedding_coverage: 'Embeddings',
  tagging_coverage: 'Tagging',
  source_uniqueness: 'Source duplicates',
  orphan_tags: 'Orphan tags',
  semantic_graph_freshness: 'Semantic graph freshness',
  wiki_coverage: 'Wiki coverage',
  content_quality: 'Content quality',
  tag_health: 'Tag health',
  content_overlap: 'Content overlap',
  contradiction_detection: 'Contradiction detection',
  broken_internal_links: 'Broken internal links',
  boilerplate_pollution: 'Boilerplate pollution',
};

const CHECK_ORDER_EXPORT = [
  'embedding_coverage', 'tagging_coverage', 'source_uniqueness', 'orphan_tags',
  'semantic_graph_freshness', 'wiki_coverage', 'content_quality', 'tag_health',
  'content_overlap', 'contradiction_detection', 'broken_internal_links',
];

function buildMarkdown(report: ExportHealthReport): string {
  const date = new Date(report.computed_at).toLocaleString();
  let md = `# Knowledge Base Health Report\n\n`;
  md += `**Overall Score:** ${report.overall_score}/100  \n`;
  md += `**Status:** ${report.overall_status.replace('_', ' ')}  \n`;
  md += `**Generated:** ${date}  \n`;
  md += `**Total atoms:** ${report.atom_count}  \n\n`;
  md += `---\n\n`;

  for (const key of CHECK_ORDER_EXPORT) {
    const check = report.checks[key];
    if (!check) continue;
    const label = CHECK_LABELS_EXPORT[key] ?? key;
    const statusIcon = check.score >= 90 ? '✅' : check.score >= 70 ? '⚠️' : check.score >= 50 ? '🟠' : '❌';
    md += `## ${statusIcon} ${label}\n\n`;
    md += `**Score:** ${check.score}/100  \n`;
    md += `**Status:** ${check.status}  \n\n`;
    // Include key data fields
    const dataEntries = Object.entries(check.data)
      .filter(([, v]) => typeof v === 'number' || typeof v === 'string')
      .slice(0, 5);
    if (dataEntries.length > 0) {
      for (const [k, v] of dataEntries) {
        md += `- **${k.replace(/_/g, ' ')}:** ${v}\n`;
      }
      md += '\n';
    }
  }

  return md;
}

async function downloadMarkdown(report: ExportHealthReport): Promise<void> {
  const md = buildMarkdown(report);
  const filename = `health-report-${new Date(report.computed_at).toISOString().split('T')[0]}.md`;

  // Tauri desktop: use plugin-dialog + plugin-fs if available
  const tauriWindow = window as typeof window & { __TAURI__?: { dialog?: unknown; fs?: unknown } };
  if (tauriWindow.__TAURI__) {
    try {
      const { save } = await import('@tauri-apps/plugin-dialog');
      const { writeTextFile } = await import('@tauri-apps/plugin-fs');
      const path = await save({
        defaultPath: filename,
        filters: [{ name: 'Markdown', extensions: ['md'] }],
      });
      if (path) {
        await writeTextFile(path, md);
      }
      return;
    } catch {
      // Fall through to web download if Tauri plugins aren't available
    }
  }

  // Web: data: URI download
  const blob = new Blob([md], { type: 'text/markdown;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

interface Props {
  report: ExportHealthReport;
  onClose: () => void;
}

export function HealthExportModal({ report, onClose }: Props) {
  const md = buildMarkdown(report);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onClose]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={e => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div className="bg-[#1e1e1e] border border-white/10 rounded-lg shadow-2xl w-full max-w-2xl mx-4 max-h-[80vh] flex flex-col">
        <div className="flex items-center justify-between px-5 py-4 border-b border-white/5 shrink-0">
          <h2 className="text-sm font-semibold text-white">Export Health Report</h2>
          <div className="flex items-center gap-2">
            <button
              onClick={() => downloadMarkdown(report)}
              className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 rounded text-xs text-white transition-colors"
            >
              <Download className="w-3.5 h-3.5" />
              Download .md
            </button>
            <button onClick={onClose} className="text-gray-500 hover:text-gray-300 transition-colors p-1">
              <X className="w-4 h-4" />
            </button>
          </div>
        </div>
        <div className="flex-1 overflow-y-auto p-5">
          <pre className="text-xs text-gray-300 whitespace-pre-wrap font-mono leading-relaxed">{md}</pre>
        </div>
      </div>
    </div>,
    document.body,
  );
}
