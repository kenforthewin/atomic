export function sourceTrust(source?: string): { label: string; tone: 'official' | 'personal' | 'manual' } {
  if (!source) return { label: 'manual', tone: 'manual' };
  try {
    const host = new URL(source).hostname.replace(/^www\./, '');
    const official = ['docs.', 'support.', 'developer.'];
    if (official.some(p => host.startsWith(p))) return { label: host, tone: 'official' };
    return { label: host, tone: 'personal' };
  } catch {
    return { label: source.slice(0, 24), tone: 'personal' };
  }
}

export function relativeAge(iso?: string): string | null {
  if (!iso) return null;
  const delta = Date.now() - new Date(iso).getTime();
  const days = delta / 86_400_000;
  if (days < 1) return 'today';
  if (days < 7) return `${Math.floor(days)}d ago`;
  if (days < 30) return `${Math.floor(days / 7)}w ago`;
  if (days < 365) return `${Math.floor(days / 30)}mo ago`;
  return `${Math.floor(days / 365)}y ago`;
}
