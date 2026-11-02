const SOURCE_PREFIX = 'atomic://starterstack/account-network/';

export function accountNetworkSlug(sourceUrl: string | null | undefined): string | null {
  if (!sourceUrl?.startsWith(SOURCE_PREFIX)) return null;
  const slug = sourceUrl.slice(SOURCE_PREFIX.length).split('/')[0];
  return slug || null;
}

export const ACCOUNT_NETWORK_CATEGORIES = [
  { key: 'account', label: 'Account', color: 'rgb(244, 244, 245)' },
  { key: 'executive', label: 'Executives', color: 'rgb(251, 146, 60)' },
  { key: 'network', label: 'Mutual connections', color: 'rgb(244, 114, 182)' },
  { key: 'board', label: 'Board', color: 'rgb(192, 132, 252)' },
  { key: 'alumni', label: 'Schools & alumni', color: 'rgb(96, 165, 250)' },
  { key: 'new_hires', label: 'VP new hires', color: 'rgb(250, 204, 21)' },
  { key: 'investors', label: 'Investors', color: 'rgb(74, 222, 128)' },
  { key: 'agencies', label: 'Service providers', color: 'rgb(251, 113, 133)' },
  { key: 'software_stack', label: 'Software stack', color: 'rgb(45, 212, 191)' },
  { key: 'borrowers', label: 'Borrowers', color: 'rgb(251, 191, 36)' },
  { key: 'competitors', label: 'Competitors', color: 'rgb(248, 113, 113)' },
  { key: 'communities', label: 'Communities', color: 'rgb(163, 230, 53)' },
] as const;

export type AccountNetworkCategory = (typeof ACCOUNT_NETWORK_CATEGORIES)[number];

const CATEGORY_BY_KEY = new Map<string, AccountNetworkCategory>(
  ACCOUNT_NETWORK_CATEGORIES.map(category => [category.key, category]),
);

export function accountNetworkCategory(sourceUrl: string | null | undefined): AccountNetworkCategory | null {
  if (!sourceUrl?.startsWith(SOURCE_PREFIX)) return null;
  const [, kind, detail] = sourceUrl.slice(SOURCE_PREFIX.length).split('/');
  if (kind === 'account') return CATEGORY_BY_KEY.get('account')!;
  if (kind === 'person') return CATEGORY_BY_KEY.get('executive')!;
  if (kind === 'school') return CATEGORY_BY_KEY.get('alumni')!;
  if (kind === 'community') return CATEGORY_BY_KEY.get('communities')!;
  if (kind === 'door') return CATEGORY_BY_KEY.get(detail) ?? null;
  return null;
}
