import { describe, expect, it } from 'vitest';
import { ACCOUNT_NETWORK_CATEGORIES, accountNetworkCategory } from './accountNetworkCategories';

const prefix = 'atomic://starterstack/account-network/peachtree-group/';

describe('accountNetworkCategory', () => {
  it('maps the account, executives, and shared entities to legend categories', () => {
    expect(accountNetworkCategory(prefix + 'account')?.key).toBe('account');
    expect(accountNetworkCategory(prefix + 'person/greg-friedman')?.key).toBe('executive');
    expect(accountNetworkCategory(prefix + 'school/tulane-university')?.key).toBe('alumni');
    expect(accountNetworkCategory(prefix + 'community/american-hotel-lodging-association')?.key).toBe('communities');
  });

  it('covers every research door and ignores unrelated atoms', () => {
    for (const category of ACCOUNT_NETWORK_CATEGORIES.slice(2)) {
      expect(accountNetworkCategory(prefix + 'door/' + category.key)?.key).toBe(category.key);
    }
    expect(accountNetworkCategory('https://example.com/story')).toBeNull();
    expect(accountNetworkCategory(prefix + 'door/unknown')).toBeNull();
  });
});
