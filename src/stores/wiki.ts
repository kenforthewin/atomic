import { create } from 'zustand';
import { toast } from 'sonner';
import { getTransport } from '../lib/transport';
import type { KnowledgeSignal, WikiCandidateEvidence } from '../types/knowledgeSignals';

// Types matching the Rust structs
export interface WikiArticle {
  id: string;
  tag_id: string;
  content: string;
  created_at: string;
  updated_at: string;
  atom_count: number;
}

export interface WikiCitation {
  id: string;
  citation_index: number;
  atom_id: string;
  chunk_index: number | null;
  excerpt: string;
}

export interface WikiArticleWithCitations {
  article: WikiArticle;
  citations: WikiCitation[];
}

export interface WikiArticleStatus {
  has_article: boolean;
  article_atom_count: number;
  current_atom_count: number;
  new_atoms_available: number;
  updated_at: string | null;
}

export interface WikiArticleSummary {
  id: string;
  tag_id: string;
  tag_name: string;
  updated_at: string;
  atom_count: number;
  inbound_links: number;
}

export interface WikiLink {
  id: string;
  source_article_id: string;
  target_tag_name: string;
  target_tag_id: string | null;
  has_article: boolean;
}

export interface RelatedTag {
  tag_id: string;
  tag_name: string;
  score: number;
  shared_atoms: number;
  semantic_edges: number;
  has_article: boolean;
}

export interface SuggestedArticle {
  tag_id: string;
  tag_name: string;
  atom_count: number;
  mention_count: number;
  score: number;
  signal_id?: string;
  confidence?: number;
  reasons?: string[];
  source_count?: number;
  recent_count?: number;
}

export interface WikiVersionSummary {
  id: string;
  version_number: number;
  atom_count: number;
  created_at: string;
}

export interface WikiArticleVersion {
  id: string;
  tag_id: string;
  content: string;
  citations: WikiCitation[];
  atom_count: number;
  version_number: number;
  created_at: string;
}

export type WikiSectionOp =
  | { op: 'NoChange' }
  | { op: 'AppendToSection'; heading: string; content: string }
  | { op: 'ReplaceSection'; heading: string; content: string }
  | { op: 'InsertSection'; after_heading: string | null; heading: string; content: string };

export interface WikiProposal {
  id: string;
  tag_id: string;
  base_article_id: string;
  base_updated_at: string;
  content: string;
  citations: WikiCitation[];
  ops: WikiSectionOp[];
  new_atom_count: number;
  created_at: string;
}

type WikiView = 'list' | 'article';

interface WikiStore {
  // View state
  view: WikiView;
  currentTagId: string | null;
  currentTagName: string | null;

  // Articles list state
  articles: WikiArticleSummary[];
  isLoadingList: boolean;

  // Suggestions state
  suggestedArticles: SuggestedArticle[];
  isLoadingSuggestions: boolean;

  // Current article state
  currentArticle: WikiArticleWithCitations | null;
  articleStatus: WikiArticleStatus | null;
  relatedTags: RelatedTag[];
  wikiLinks: WikiLink[];

  // Version history
  versions: WikiVersionSummary[];
  selectedVersion: WikiArticleVersion | null;

  // Proposal state (human-in-the-loop update review)
  proposal: WikiProposal | null;
  isProposing: boolean;
  isAccepting: boolean;
  isDismissing: boolean;
  reviewingProposal: boolean;

  // Loading states
  isLoading: boolean;
  isGenerating: boolean;
  isUpdating: boolean;
  error: string | null;

  // List actions
  fetchAllArticles: (options?: { refreshSuggestions?: boolean }) => Promise<void>;
  fetchSuggestedArticles: () => Promise<void>;
  dismissSuggestedArticle: (signalKey: string) => Promise<void>;
  showList: () => void;
  openArticle: (tagId: string, tagName: string) => void;
  openAndGenerate: (tagId: string, tagName: string) => Promise<void>;
  goBack: () => void;

  // Article actions
  fetchArticle: (tagId: string) => Promise<void>;
  fetchArticleStatus: (tagId: string) => Promise<void>;
  fetchRelatedTags: (tagId: string) => Promise<void>;
  fetchWikiLinks: (tagId: string) => Promise<void>;
  generateArticle: (tagId: string, tagName: string) => Promise<void>;
  updateArticle: (tagId: string, tagName: string) => Promise<void>;
  deleteArticle: (tagId: string) => Promise<void>;
  fetchVersions: (tagId: string) => Promise<void>;
  selectVersion: (versionId: string) => Promise<void>;
  clearSelectedVersion: () => void;
  clearArticle: () => void;
  clearError: () => void;
  reset: () => void;

  // Proposal actions (human-in-the-loop update review)
  fetchProposal: (tagId: string) => Promise<void>;
  proposeArticle: (tagId: string, tagName: string) => Promise<void>;
  acceptProposal: (tagId: string) => Promise<void>;
  dismissProposal: (tagId: string) => Promise<void>;
  startReviewingProposal: () => void;
  stopReviewingProposal: () => void;
}

export const useWikiStore = create<WikiStore>((set, get) => ({
  // View state
  view: 'list',
  currentTagId: null,
  currentTagName: null,

  // Articles list state
  articles: [],
  isLoadingList: false,

  // Suggestions state
  suggestedArticles: [],
  isLoadingSuggestions: false,

  // Current article state
  currentArticle: null,
  articleStatus: null,
  relatedTags: [],
  wikiLinks: [],
  versions: [],
  selectedVersion: null,
  proposal: null,
  isProposing: false,
  isAccepting: false,
  isDismissing: false,
  reviewingProposal: false,
  isLoading: false,
  isGenerating: false,
  isUpdating: false,
  error: null,

  fetchAllArticles: async (options) => {
    set({ isLoadingList: true, error: null });
    try {
      const articles = await getTransport().invoke<WikiArticleSummary[]>('get_all_wiki_articles');
      set({ articles, isLoadingList: false });
      // Refresh suggestions after a brief yield so the list renders first
      if (options?.refreshSuggestions !== false) {
        setTimeout(() => get().fetchSuggestedArticles(), 50);
      }
    } catch (error) {
      set({ error: String(error), isLoadingList: false });
    }
  },

  fetchSuggestedArticles: async () => {
    set({ isLoadingSuggestions: true });
    try {
      const signals = await getTransport().invoke<KnowledgeSignal<WikiCandidateEvidence>[]>('list_knowledge_signals', {
        providerId: 'wiki_candidate',
        limit: 100,
      });
      const suggestions: SuggestedArticle[] = signals.map(signal => ({
        tag_id: signal.evidence?.tag_id ?? signal.target.id,
        tag_name: signal.evidence?.tag_name ?? signal.target.label,
        atom_count: signal.evidence?.atom_count ?? 0,
        mention_count: signal.evidence?.mention_count ?? 0,
        score: signal.score,
        signal_id: signal.id,
        confidence: signal.confidence,
        reasons: signal.reasons.map(reason => reason.label),
        source_count: signal.evidence?.source_count ?? 0,
        recent_count: signal.evidence?.recent_count ?? 0,
      }));
      set({ suggestedArticles: suggestions, isLoadingSuggestions: false });
    } catch (error) {
      console.error('Failed to fetch suggested articles:', error);
      toast.error('Failed to load suggested articles', { id: 'wiki-suggestions-error', description: String(error) });
      set({ isLoadingSuggestions: false });
    }
  },

  dismissSuggestedArticle: async (signalKey: string) => {
    const previous = get().suggestedArticles;
    set({
      suggestedArticles: previous.filter(s => s.signal_id !== signalKey),
    });
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (error) {
      console.error('Failed to dismiss wiki suggestion:', error);
      set({ suggestedArticles: previous });
      toast.error('Failed to dismiss suggestion', { description: String(error) });
    }
  },

  showList: () => {
    set({
      view: 'list',
      currentTagId: null,
      currentTagName: null,
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      error: null,
    });
  },

  openArticle: (tagId: string, tagName: string) => {
    set({
      view: 'article',
      currentTagId: tagId,
      currentTagName: tagName,
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      proposal: null,
      reviewingProposal: false,
      isLoading: true,
      error: null,
    });
    // Fetch article, status, related tags, wiki links, versions, and proposal
    get().fetchArticle(tagId);
    get().fetchArticleStatus(tagId);
    get().fetchRelatedTags(tagId);
    get().fetchWikiLinks(tagId);
    get().fetchVersions(tagId);
    get().fetchProposal(tagId);
  },

  // Open article view and immediately start generating (for new wikis)
  openAndGenerate: async (tagId: string, tagName: string) => {
    if (get().isGenerating) return;
    set({
      view: 'article',
      currentTagId: tagId,
      currentTagName: tagName,
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      isLoading: false,
      isGenerating: true,
      error: null,
    });
    // Fetch status for display during generation
    await get().fetchArticleStatus(tagId);
    // Start generation
    await get().generateArticle(tagId, tagName);
  },

  goBack: () => {
    set({
      view: 'list',
      currentTagId: null,
      currentTagName: null,
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      error: null,
    });
    // Refresh list in case changes were made
    get().fetchAllArticles();
  },

  fetchArticle: async (tagId: string) => {
    set({ isLoading: true, error: null });
    try {
      const article = await getTransport().invoke<WikiArticleWithCitations | null>('get_wiki_article', { tagId });
      set({ currentArticle: article, isLoading: false });
    } catch (error) {
      set({ error: String(error), isLoading: false });
    }
  },

  fetchArticleStatus: async (tagId: string) => {
    try {
      const status = await getTransport().invoke<WikiArticleStatus>('get_wiki_article_status', { tagId });
      set({ articleStatus: status });
    } catch (error) {
      console.error('Failed to fetch article status:', error);
      toast.error('Failed to load article status', { id: 'wiki-status-error', description: String(error) });
    }
  },

  fetchRelatedTags: async (tagId: string) => {
    try {
      const tags = await getTransport().invoke<RelatedTag[]>('get_related_tags', { tagId, limit: 10 });
      set({ relatedTags: tags });
    } catch (error) {
      console.error('Failed to fetch related tags:', error);
      toast.error('Failed to load related tags', { id: 'wiki-related-error', description: String(error) });
    }
  },

  fetchWikiLinks: async (tagId: string) => {
    try {
      const links = await getTransport().invoke<WikiLink[]>('get_wiki_links', { tagId });
      set({ wikiLinks: links });
    } catch (error) {
      console.error('Failed to fetch wiki links:', error);
      toast.error('Failed to load wiki links', { id: 'wiki-links-error', description: String(error) });
    }
  },

  generateArticle: async (tagId: string, tagName: string) => {
    set({
      isGenerating: true,
      error: null,
      selectedVersion: null,
      // Any pending proposal was computed against the article we're about
      // to replace, so it's meaningless now. Backend clears the row; clear
      // the in-memory mirror + exit review mode so the UI stays consistent.
      proposal: null,
      reviewingProposal: false,
    });
    try {
      const article = await getTransport().invoke<WikiArticleWithCitations>('generate_wiki_article', { tagId, tagName });
      set({ currentArticle: article, isGenerating: false });
      // Refresh status, related tags, wiki links, and versions after generation
      get().fetchArticleStatus(tagId);
      get().fetchRelatedTags(tagId);
      get().fetchWikiLinks(tagId);
      get().fetchVersions(tagId);
      // Also refresh the list to include the new article
      get().fetchAllArticles();
    } catch (error) {
      set({ error: String(error), isGenerating: false });
    }
  },

  updateArticle: async (tagId: string, tagName: string) => {
    set({ isUpdating: true, error: null, selectedVersion: null });
    try {
      const article = await getTransport().invoke<WikiArticleWithCitations>('update_wiki_article', { tagId, tagName });
      set({ currentArticle: article, isUpdating: false });
      // Refresh status, related tags, wiki links, and versions after update
      get().fetchArticleStatus(tagId);
      get().fetchRelatedTags(tagId);
      get().fetchWikiLinks(tagId);
      get().fetchVersions(tagId);
      // Also refresh the list
      get().fetchAllArticles();
    } catch (error) {
      set({ error: String(error), isUpdating: false });
    }
  },

  deleteArticle: async (tagId: string) => {
    try {
      await getTransport().invoke('delete_wiki_article', { tagId });
      set({
        currentArticle: null,
        articleStatus: null,
        relatedTags: [],
        wikiLinks: [],
        versions: [],
        selectedVersion: null,
        proposal: null,
        reviewingProposal: false,
      });
      // Refresh the list
      get().fetchAllArticles();
    } catch (error) {
      set({ error: String(error) });
    }
  },

  // ==================== Proposal actions ====================

  fetchProposal: async (tagId: string) => {
    try {
      const proposal = await getTransport().invoke<WikiProposal | null>('get_wiki_proposal', { tagId });
      set({ proposal });
    } catch (error) {
      // 404 is expected when there's no pending proposal — don't toast.
      const msg = String(error);
      if (!msg.includes('No pending proposal')) {
        console.error('Failed to fetch proposal:', error);
      }
      set({ proposal: null });
    }
  },

  proposeArticle: async (tagId: string, tagName: string) => {
    set({ isProposing: true, error: null });
    try {
      const result = await getTransport().invoke<WikiProposal | { status: string }>(
        'propose_wiki_article',
        { tagId, tagName }
      );
      if ('status' in result && result.status === 'no_update_needed') {
        set({ isProposing: false, proposal: null });
        toast.info('No update needed', {
          id: 'wiki-propose-noop',
          description: 'The new atoms don\'t warrant changes to the article.',
        });
        // Refresh status so the banner reflects the latest atom count.
        get().fetchArticleStatus(tagId);
      } else {
        set({ proposal: result as WikiProposal, isProposing: false });
        toast.success('Suggested update ready', {
          id: 'wiki-proposal-ready',
          description: 'Click Review to see what would change.',
        });
      }
    } catch (error) {
      set({ error: String(error), isProposing: false });
      toast.error('Failed to generate update', {
        id: 'wiki-propose-error',
        description: String(error),
      });
    }
  },

  acceptProposal: async (tagId: string) => {
    set({ isAccepting: true, error: null });
    try {
      const article = await getTransport().invoke<WikiArticleWithCitations>(
        'accept_wiki_proposal',
        { tagId }
      );
      set({
        currentArticle: article,
        proposal: null,
        reviewingProposal: false,
        isAccepting: false,
      });
      // Refresh dependent state after the live article changes.
      get().fetchArticleStatus(tagId);
      get().fetchRelatedTags(tagId);
      get().fetchWikiLinks(tagId);
      get().fetchVersions(tagId);
      get().fetchAllArticles();
      toast.success('Update applied');
    } catch (error) {
      const msg = String(error);
      set({ isAccepting: false });
      if (msg.includes('stale')) {
        // Proposal was superseded / live article updated out-of-band.
        set({ proposal: null, reviewingProposal: false });
        toast.error('Proposal out of date', {
          id: 'wiki-proposal-stale',
          description: 'The article was updated elsewhere. Regenerate the suggestion to review it.',
        });
        get().fetchArticle(tagId);
        get().fetchArticleStatus(tagId);
      } else {
        set({ error: msg });
        toast.error('Failed to apply update', {
          id: 'wiki-accept-error',
          description: msg,
        });
      }
    }
  },

  dismissProposal: async (tagId: string) => {
    set({ isDismissing: true });
    try {
      await getTransport().invoke('dismiss_wiki_proposal', { tagId });
      set({ proposal: null, reviewingProposal: false, isDismissing: false });
      get().fetchArticleStatus(tagId);
    } catch (error) {
      set({ isDismissing: false });
      toast.error('Failed to dismiss suggestion', {
        id: 'wiki-dismiss-error',
        description: String(error),
      });
    }
  },

  startReviewingProposal: () => {
    set({ reviewingProposal: true });
  },

  stopReviewingProposal: () => {
    set({ reviewingProposal: false });
  },

  fetchVersions: async (tagId: string) => {
    try {
      const versions = await getTransport().invoke<WikiVersionSummary[]>('get_wiki_versions', { tagId });
      set({ versions });
    } catch (error) {
      console.error('Failed to fetch wiki versions:', error);
      toast.error('Failed to load version history', { id: 'wiki-versions-error', description: String(error) });
    }
  },

  selectVersion: async (versionId: string) => {
    try {
      const version = await getTransport().invoke<WikiArticleVersion | null>('get_wiki_version', { versionId });
      set({ selectedVersion: version });
    } catch (error) {
      console.error('Failed to fetch wiki version:', error);
      toast.error('Failed to load version', { id: 'wiki-version-error', description: String(error) });
    }
  },

  clearSelectedVersion: () => {
    set({ selectedVersion: null });
  },

  clearArticle: () => {
    set({
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      proposal: null,
      reviewingProposal: false,
      error: null,
    });
  },

  clearError: () => {
    set({ error: null });
  },

  reset: () => {
    set({
      view: 'list',
      currentTagId: null,
      currentTagName: null,
      articles: [],
      isLoadingList: false,
      suggestedArticles: [],
      isLoadingSuggestions: false,
      currentArticle: null,
      articleStatus: null,
      relatedTags: [],
      wikiLinks: [],
      versions: [],
      selectedVersion: null,
      proposal: null,
      isProposing: false,
      isAccepting: false,
      isDismissing: false,
      reviewingProposal: false,
      isLoading: false,
      isGenerating: false,
      isUpdating: false,
      error: null,
    });
  },
}));
