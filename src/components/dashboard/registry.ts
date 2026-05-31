import type { FC } from 'react';
import { BriefingWidget } from './widgets/BriefingWidget';
import { ActivityWidget } from './widgets/ActivityWidget';
import { NewWikisWidget } from './widgets/NewWikisWidget';
import { RevisionsWidget } from './widgets/RevisionsWidget';
import { TagCleanupWidget } from './widgets/TagCleanupWidget';
import { IdeasToConnectWidget } from './widgets/IdeasToConnectWidget';
import { SimilarNotesWidget } from './widgets/SimilarNotesWidget';
import { UnderconnectedNotesWidget } from './widgets/UnderconnectedNotesWidget';
import { BrokenLinksWidget } from './widgets/BrokenLinksWidget';

export type WidgetSpan = 'full' | 'half';

export interface DashboardWidget {
  id: string;
  span: WidgetSpan;
  Component: FC;
  providerIds?: string[];
}

export const dashboardWidgets: DashboardWidget[] = [
  { id: 'briefing', span: 'full', Component: BriefingWidget },
  { id: 'activity', span: 'half', Component: ActivityWidget },
  { id: 'new-wikis', span: 'half', Component: NewWikisWidget, providerIds: ['wiki_candidate'] },
  { id: 'tag-cleanup', span: 'half', Component: TagCleanupWidget, providerIds: ['tag_redundancy', 'empty_tag'] },
  { id: 'ideas-to-connect', span: 'half', Component: IdeasToConnectWidget, providerIds: ['missing_tag_overlap'] },
  { id: 'similar-notes', span: 'half', Component: SimilarNotesWidget, providerIds: ['near_duplicate_atom', 'source_duplicate'] },
  { id: 'broken-links', span: 'half', Component: BrokenLinksWidget, providerIds: ['broken_internal_link'] },
  { id: 'underconnected-notes', span: 'half', Component: UnderconnectedNotesWidget, providerIds: ['underconnected_atom'] },
  { id: 'revisions', span: 'half', Component: RevisionsWidget, providerIds: ['wiki_update'] },
];
