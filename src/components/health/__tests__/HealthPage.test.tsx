import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

// Mock stores and transport before imports
const invoke = vi.fn();
const subscribe = vi.fn(() => () => {});
vi.mock('../../../lib/transport', () => ({
  getTransport: () => ({ invoke, subscribe }),
}));

vi.mock('../../../stores/ui', () => ({
  useUIStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ viewMode: 'health', setViewMode: vi.fn() }),
}));

vi.mock('../../../stores/toasts', () => ({
  toast: { error: vi.fn(), info: vi.fn(), success: vi.fn() },
}));

vi.mock('../../../stores/tags', () => ({
  useTagsStore: (selector: (s: Record<string, unknown>) => unknown) => selector({ fetchTags: vi.fn() }),
}));

vi.mock('../TagStructureTab', () => ({
  TagStructureTab: () => <div data-testid="tag-structure-tab">Tag Structure</div>,
}));

vi.mock('../../dashboard/widgets/HealthWidget', () => ({
  HealthPanel: () => <div data-testid="health-panel">Health Panel</div>,
}));

import { HealthPage } from '../HealthPage';

describe('HealthPage', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ overall_score: 85, overall_status: 'healthy', auto_fixable: 0, requires_review: 0 });
  });

  afterEach(() => { cleanup(); });

  it('renders page title', () => {
    render(<HealthPage />);
    expect(screen.getByText('Knowledge Health')).toBeTruthy();
  });

  it('renders Overview tab with HealthPanel', () => {
    render(<HealthPage />);
    expect(screen.getByTestId('health-panel')).toBeTruthy();
  });

  it('switches to Tag Structure tab on click', async () => {
    const user = userEvent.setup();
    render(<HealthPage />);
    const tagTab = screen.getByRole('button', { name: /Tag Structure/i });
    await user.click(tagTab);
    await waitFor(() => expect(screen.getByTestId('tag-structure-tab')).toBeTruthy());
  });

  it('refresh button is present', () => {
    render(<HealthPage />);
    expect(screen.getByRole('button', { name: /Refresh health checks/i })).toBeTruthy();
  });
});
