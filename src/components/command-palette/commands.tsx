import { getTransport } from '../../lib/transport';
import { Command, CommandCategory } from './types';
import { useUIStore } from '../../stores/ui';
import { useTagsStore } from '../../stores/tags';

// Icon components as simple SVG functions
const PlusIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 4v16m8-8H4" />
  </svg>
);

const SearchIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
  </svg>
);

const TagIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A1.994 1.994 0 013 12V7a4 4 0 014-4z" />
  </svg>
);

const BookOpenIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 6.253v13m0-13C10.832 5.477 9.246 5 7.5 5S4.168 5.477 3 6.253v13C4.168 18.477 5.754 18 7.5 18s3.332.477 4.5 1.253m0-13C13.168 5.477 14.754 5 16.5 5c1.747 0 3.332.477 4.5 1.253v13C19.832 18.477 18.247 18 16.5 18c-1.746 0-3.332.477-4.5 1.253" />
  </svg>
);

const MessageCircleIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
  </svg>
);

const LayoutGridIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2V6zM14 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2V6zM4 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2v-2zM14 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2v-2z" />
  </svg>
);

const ListIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 6h16M4 10h16M4 14h16M4 18h16" />
  </svg>
);

const SettingsIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
  </svg>
);

const RefreshIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
  </svg>
);

const MergeIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 7h12m0 0l-4-4m4 4l-4 4m0 6H4m0 0l4 4m-4-4l4-4" />
  </svg>
);

const XIcon = () => (
  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
  </svg>
);

// Command definitions
export const commands: Command[] = [
  // Navigation commands
  {
    id: 'open-wiki-list',
    label: 'Open wiki list',
    category: 'navigation',
    keywords: ['wiki', 'articles', 'list', 'browse', 'knowledge'],
    icon: BookOpenIcon,
    action: () => useUIStore.getState().openWikiListDrawer(),
  },
  {
    id: 'open-chat-list',
    label: 'Open chat list',
    category: 'navigation',
    keywords: ['chat', 'conversations', 'messages', 'talk'],
    icon: MessageCircleIcon,
    action: () => useUIStore.getState().openChatDrawer(),
  },
  {
    id: 'create-new-chat',
    label: 'Create new chat',
    category: 'navigation',
    keywords: ['chat', 'conversation', 'new', 'start'],
    icon: MessageCircleIcon,
    action: () => useUIStore.getState().openChatDrawer(),
  },
  {
    id: 'switch-to-grid',
    label: 'Switch to grid view',
    category: 'navigation',
    keywords: ['view', 'grid', 'cards', 'tiles'],
    icon: LayoutGridIcon,
    action: () => useUIStore.getState().setViewMode('grid'),
    isEnabled: () => useUIStore.getState().viewMode !== 'grid',
  },
  {
    id: 'switch-to-list',
    label: 'Switch to list view',
    category: 'navigation',
    keywords: ['view', 'list', 'rows', 'compact'],
    icon: ListIcon,
    action: () => useUIStore.getState().setViewMode('list'),
    isEnabled: () => useUIStore.getState().viewMode !== 'list',
  },
  {
    id: 'open-settings',
    label: 'Open settings',
    category: 'navigation',
    keywords: ['settings', 'preferences', 'config', 'options', 'setup'],
    icon: SettingsIcon,
    action: () => useUIStore.getState().setViewMode('settings'),
  },

  // Atom commands
  {
    id: 'create-atom',
    label: 'Create new atom',
    category: 'atoms',
    keywords: ['new', 'add', 'write', 'note', 'create', 'atom'],
    shortcut: '⌘N',
    icon: PlusIcon,
    action: () => useUIStore.getState().openDrawer('editor'),
  },
  {
    id: 'search-atoms',
    label: 'Search atoms...',
    category: 'atoms',
    keywords: ['search', 'find', 'query', 'semantic', 'lookup'],
    shortcut: '/',
    icon: SearchIcon,
    action: () => {
      // This is handled specially - switches to search mode
      // The CommandPalette will intercept this
    },
  },

  // Tag commands
  {
    id: 'filter-by-tag',
    label: 'Filter by tag...',
    category: 'tags',
    keywords: ['tag', 'filter', 'category', 'label'],
    shortcut: '#',
    icon: TagIcon,
    action: () => {
      // This is handled specially - switches to tag filter mode
      // The CommandPalette will intercept this
    },
  },
  {
    id: 'create-tag',
    label: 'Create new tag',
    category: 'tags',
    keywords: ['tag', 'new', 'add', 'create', 'category'],
    icon: PlusIcon,
    action: async () => {
      const name = window.prompt('Enter tag name:');
      if (name && name.trim()) {
        await useTagsStore.getState().createTag(name.trim());
      }
    },
  },
  {
    id: 'compact-tags',
    label: 'Compact tags (AI-assisted)',
    category: 'tags',
    keywords: ['compact', 'merge', 'clean', 'organize', 'ai', 'llm'],
    icon: MergeIcon,
    action: async () => {
      await useTagsStore.getState().compactTags();
    },
  },
  {
    id: 'clear-tag-filter',
    label: 'Clear tag filter',
    category: 'tags',
    keywords: ['clear', 'reset', 'remove', 'filter'],
    icon: XIcon,
    action: () => useUIStore.getState().setSelectedTag(null),
    isEnabled: () => useUIStore.getState().selectedTagId !== null,
  },

  // Utility commands
  {
    id: 'open-event-log',
    label: 'Open event log',
    category: 'utility',
    keywords: ['event', 'log', 'console', 'debug', 'events', 'stream', 'monitor'],
    icon: SettingsIcon,
    action: () => {
      window.dispatchEvent(new CustomEvent('open-event-log'));
    },
  },
  {
    id: 'open-pipeline-status',
    label: 'Pipeline Status',
    category: 'utility',
    keywords: ['pipeline', 'embedding', 'status', 'queue', 'failed', 'processing', 'stuck'],
    icon: SettingsIcon,
    action: () => {
      window.dispatchEvent(new CustomEvent('open-pipeline-status'));
    },
  },
  {
    id: 'retry-failed-embeddings',
    label: 'Retry failed embeddings',
    category: 'utility',
    keywords: ['retry', 'failed', 'embedding', 'process', 'fix'],
    icon: RefreshIcon,
    action: async () => {
      try {
        const count = await getTransport().invoke<number>('process_pending_embeddings');
        if (count > 0) {
          console.log(`Retrying ${count} pending embeddings...`);
        }
      } catch (error) {
        console.error('Failed to retry embeddings:', error);
      }
    },
  },
];

// Category labels for display
export const categoryLabels: Record<CommandCategory, string> = {
  navigation: 'Navigation',
  atoms: 'Atoms',
  tags: 'Tags',
  wiki: 'Wiki',
  utility: 'Utility',
};

// Category order for display
export const categoryOrder: CommandCategory[] = [
  'navigation',
  'atoms',
  'tags',
  'wiki',
  'utility',
];

// Get commands grouped by category
export function getGroupedCommands(): Map<CommandCategory, Command[]> {
  const grouped = new Map<CommandCategory, Command[]>();

  for (const category of categoryOrder) {
    const categoryCommands = commands.filter(
      (cmd) => cmd.category === category && (cmd.isEnabled?.() ?? true)
    );
    if (categoryCommands.length > 0) {
      grouped.set(category, categoryCommands);
    }
  }

  return grouped;
}
