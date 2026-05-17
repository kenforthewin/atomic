import { Section } from '../Section';
import { useWikiStore } from '../../../stores/wiki';
import { X } from 'lucide-react';

const MAX_ITEMS = 5;

export function NewWikisWidget() {
  const suggestedArticles = useWikiStore(s => s.suggestedArticles);
  const openAndGenerate = useWikiStore(s => s.openAndGenerate);
  const dismissSuggestedArticle = useWikiStore(s => s.dismissSuggestedArticle);

  const items = suggestedArticles.slice(0, MAX_ITEMS);

  return (
    <Section label="Ready to generate">
      {items.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          No wiki suggestions yet. Tag more atoms to build up candidates.
        </div>
      ) : (
        <ul className="-mx-2">
          {items.map(s => (
            <li key={s.tag_id} className="group flex items-start gap-1 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60">
              <button
                onClick={() => openAndGenerate(s.tag_id, s.tag_name)}
                className="min-w-0 flex-1 text-left"
              >
                <span className="flex items-baseline gap-3">
                  <span className="flex-1 min-w-0 truncate text-sm text-[var(--color-text-secondary)] group-hover:text-[var(--color-text-primary)]">
                    {s.tag_name}
                  </span>
                </span>
                <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                  {(s.reasons ?? [`${s.atom_count} atoms`]).slice(0, 2).join(' / ')}
                </span>
              </button>
              {s.signal_id && (
                <button
                  onClick={() => dismissSuggestedArticle(s.signal_id!)}
                  title="Dismiss suggestion"
                  className="mt-0.5 shrink-0 text-[var(--color-text-tertiary)] opacity-0 transition-opacity hover:text-[var(--color-text-primary)] group-hover:opacity-100 focus:opacity-100"
                >
                  <X className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}
