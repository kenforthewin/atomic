import { Section } from '../Section';
import { useWikiStore } from '../../../stores/wiki';
import { useUIStore } from '../../../stores/ui';
import { formatShortRelativeDate } from '../../../lib/date';

const MAX_ITEMS = 5;

export function RecentWikisWidget() {
  const articles = useWikiStore(s => s.articles);
  const openWikiReader = useUIStore(s => s.openWikiReader);

  // Sort by updated_at descending so freshly generated/updated articles
  // always appear first, regardless of the server's importance ordering.
  const items = articles
    .slice()
    .sort((a, b) => b.updated_at.localeCompare(a.updated_at))
    .slice(0, MAX_ITEMS);

  return (
    <Section label="Recent wikis">
      {items.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          No wiki articles yet. Generate one to get started.
        </div>
      ) : (
        <ul className="-mx-2">
          {items.map(article => (
            <li key={article.id}>
              <button
                onClick={() => openWikiReader(article.tag_id, article.tag_name)}
                className="w-full flex items-baseline gap-3 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60 text-left group"
              >
                <span className="flex-1 min-w-0 truncate text-sm text-[var(--color-text-secondary)] group-hover:text-[var(--color-text-primary)]">
                  {article.tag_name}
                </span>
                <span className="text-[11px] text-[var(--color-text-tertiary)] tabular-nums shrink-0">
                  {formatShortRelativeDate(article.updated_at)}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}
