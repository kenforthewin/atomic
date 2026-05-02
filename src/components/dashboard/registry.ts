import type { FC } from 'react';
import { BriefingWidget } from './widgets/BriefingWidget';
import { ActivityWidget } from './widgets/ActivityWidget';
import { NewWikisWidget } from './widgets/NewWikisWidget';
import { RecentWikisWidget } from './widgets/RecentWikisWidget';
import { RevisionsWidget } from './widgets/RevisionsWidget';
import { HealthSummaryCard } from './widgets/HealthSummaryCard';

export type WidgetSpan = 'full' | 'half';

export interface DashboardWidget {
  id: string;
  span: WidgetSpan;
  Component: FC;
}

export const dashboardWidgets: DashboardWidget[] = [
  { id: 'briefing',      span: 'full', Component: BriefingWidget },
  { id: 'activity',     span: 'half', Component: ActivityWidget },
  { id: 'new-wikis',   span: 'half', Component: NewWikisWidget },
  { id: 'recent-wikis', span: 'half', Component: RecentWikisWidget },
  { id: 'revisions',   span: 'half', Component: RevisionsWidget },
  { id: 'health',      span: 'half', Component: HealthSummaryCard },
];