import { useState } from 'react';
import { HeartPulse } from 'lucide-react';
import { HealthPanel } from './HealthPanel';
import { TagStructureTab } from './TagStructureTab';

type HealthTab = 'overview' | 'tags';

export function HealthPage() {
  const [activeTab, setActiveTab] = useState<HealthTab>('overview');

  return (
    <div className="flex flex-col h-full overflow-hidden bg-[var(--color-bg-primary)]">
      {/* Page header */}
      <div className="flex items-center justify-between px-6 py-4 border-b border-[var(--color-border)] shrink-0">
        <div className="flex items-center gap-3">
          <HeartPulse className="w-5 h-5 text-[var(--color-accent-light)]" strokeWidth={2} />
          <div>
            <h1 className="text-base font-semibold text-white">Knowledge Health</h1>
            <p className="text-xs text-[var(--color-text-secondary)] mt-0.5">
              Monitor and fix quality issues in your knowledge base
            </p>
          </div>
        </div>
      </div>

      {/* Tabs */}
      <div className="flex border-b border-[var(--color-border)] shrink-0 px-6">
        {([
          ['overview', 'Overview & Review Queue'],
          ['tags', 'Tag Structure'],
        ] as const).map(([tab, label]) => (
          <button
            key={tab}
            onClick={() => setActiveTab(tab)}
            className={[
              'px-3 py-2.5 text-xs font-medium transition-colors border-b-2 -mb-px',
              activeTab === tab
                ? 'border-[var(--color-accent)] text-white'
                : 'border-transparent text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]',
            ].join(' ')}
          >
            {label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      <div className="flex-1 overflow-y-auto">
        {activeTab === 'overview' && (
          <div className="max-w-4xl mx-auto p-6">
            <HealthPanel hideTitle />
          </div>
        )}
        {activeTab === 'tags' && (
          <div className="max-w-4xl mx-auto p-6">
            <TagStructureTab />
          </div>
        )}
      </div>
    </div>
  );
}
