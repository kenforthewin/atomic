import { useReducer } from 'react';
import type { AvailableModel, OllamaModel } from '../../lib/api';
import { isCloudTenant } from '../../lib/transport';

export type StepId =
  | 'welcome'
  | 'ai-provider'
  | 'tag-categories'
  | 'integrations'
  | 'tutorial';

export const DEFAULT_TAG_CATEGORIES = [
  'Topics',
  'People',
  'Locations',
  'Organizations',
  'Events',
] as const;

export interface OnboardingState {
  currentStep: number;

  // Step 1: Welcome / Server connection (web mode)
  serverUrl: string;
  serverToken: string;
  serverTestResult: 'success' | 'error' | null;
  serverTestError: string | null;
  isTestingServer: boolean;

  // Step 2: AI Provider
  provider: 'openrouter' | 'ollama' | 'openai_compat';
  apiKey: string;
  embeddingModel: string;
  taggingModel: string;
  wikiModel: string;
  chatModel: string;
  autoTaggingEnabled: boolean;
  isTesting: boolean;
  testResult: 'success' | 'error' | null;
  testError: string | null;
  availableModels: AvailableModel[];
  isLoadingModels: boolean;
  // Ollama
  ollamaHost: string;
  ollamaStatus: 'checking' | 'connected' | 'disconnected';
  ollamaError: string | undefined;
  ollamaModels: OllamaModel[];
  isLoadingOllamaModels: boolean;
  ollamaContextLength: string;
  ollamaTimeoutSecs: string;
  // OpenAI Compatible
  openaiCompatBaseUrl: string;
  openaiCompatApiKey: string;
  openaiCompatEmbeddingModel: string;
  openaiCompatEmbeddingDimension: string;
  openaiCompatLlmModel: string;
  openaiCompatContextLength: string;
  openaiCompatTimeoutSecs: string;
  openaiCompatStatus: 'idle' | 'checking' | 'connected' | 'error';
  openaiCompatError: string | null;

  // Step 3: Tag categories (only shown if auto-tagging is enabled)
  selectedDefaultCategories: string[]; // subset of DEFAULT_TAG_CATEGORIES
  customCategories: string[];
  customCategoryInput: string;
  isSavingCategories: boolean;
  categoriesError: string | null;

  // Step 4: Mobile setup
  mobileToken: string | null;
  mobileTokenName: string;

  // Step 6: Data loading
  feedUrl: string;
  ingestUrl: string;

  // Step 7: Tutorial
  tutorialAtomId: string | null;
  tutorialEmbeddingDone: boolean;
  tutorialTaggingDone: boolean;
  tutorialTagsExtracted: string[];
  tutorialCreating: boolean;
}

export type OnboardingAction =
  | { type: 'SET_STEP'; step: number }
  | { type: 'NEXT_STEP' }
  | { type: 'PREV_STEP' }
  // Welcome / Server
  | { type: 'SET_SERVER_URL'; value: string }
  | { type: 'SET_SERVER_TOKEN'; value: string }
  | { type: 'SET_SERVER_TEST'; result: 'success' | 'error' | null; error?: string }
  | { type: 'SET_TESTING_SERVER'; value: boolean }
  // AI Provider
  | { type: 'SET_PROVIDER'; value: 'openrouter' | 'ollama' | 'openai_compat' }
  | { type: 'SET_API_KEY'; value: string }
  | { type: 'SET_EMBEDDING_MODEL'; value: string }
  | { type: 'SET_TAGGING_MODEL'; value: string }
  | { type: 'SET_WIKI_MODEL'; value: string }
  | { type: 'SET_CHAT_MODEL'; value: string }
  | { type: 'SET_AUTO_TAGGING'; value: boolean }
  | { type: 'SET_TESTING'; value: boolean }
  | { type: 'SET_TEST_RESULT'; result: 'success' | 'error' | null; error?: string }
  | { type: 'SET_AVAILABLE_MODELS'; models: AvailableModel[] }
  | { type: 'SET_LOADING_MODELS'; value: boolean }
  | { type: 'SET_OLLAMA_HOST'; value: string }
  | { type: 'SET_OLLAMA_STATUS'; status: 'checking' | 'connected' | 'disconnected'; error?: string }
  | { type: 'SET_OLLAMA_MODELS'; models: OllamaModel[] }
  | { type: 'SET_LOADING_OLLAMA_MODELS'; value: boolean }
  | { type: 'SET_OLLAMA_CONTEXT_LENGTH'; value: string }
  | { type: 'SET_OLLAMA_TIMEOUT_SECS'; value: string }
  // OpenAI Compatible
  | { type: 'SET_OPENAI_COMPAT_BASE_URL'; value: string }
  | { type: 'SET_OPENAI_COMPAT_API_KEY'; value: string }
  | { type: 'SET_OPENAI_COMPAT_EMBEDDING_MODEL'; value: string }
  | { type: 'SET_OPENAI_COMPAT_EMBEDDING_DIMENSION'; value: string }
  | { type: 'SET_OPENAI_COMPAT_LLM_MODEL'; value: string }
  | { type: 'SET_OPENAI_COMPAT_CONTEXT_LENGTH'; value: string }
  | { type: 'SET_OPENAI_COMPAT_TIMEOUT_SECS'; value: string }
  | { type: 'SET_OPENAI_COMPAT_STATUS'; status: 'idle' | 'checking' | 'connected' | 'error'; error?: string }
  // Tag categories
  | { type: 'TOGGLE_DEFAULT_CATEGORY'; name: string }
  | { type: 'SET_CUSTOM_CATEGORY_INPUT'; value: string }
  | { type: 'ADD_CUSTOM_CATEGORY' }
  | { type: 'REMOVE_CUSTOM_CATEGORY'; name: string }
  | { type: 'SET_SAVING_CATEGORIES'; value: boolean }
  | { type: 'SET_CATEGORIES_ERROR'; error: string | null }
  // Mobile
  | { type: 'SET_MOBILE_TOKEN'; token: string | null }
  // Data loading
  | { type: 'SET_FEED_URL'; value: string }
  | { type: 'SET_INGEST_URL'; value: string }
  // Tutorial
  | { type: 'SET_TUTORIAL_ATOM_ID'; id: string | null }
  | { type: 'SET_TUTORIAL_EMBEDDING_DONE'; value: boolean }
  | { type: 'SET_TUTORIAL_TAGGING_DONE'; value: boolean; tags?: string[] }
  | { type: 'SET_TUTORIAL_CREATING'; value: boolean };

const initialState: OnboardingState = {
  currentStep: 0,
  serverUrl: '',
  serverToken: '',
  serverTestResult: null,
  serverTestError: null,
  isTestingServer: false,
  provider: 'openrouter',
  apiKey: '',
  embeddingModel: 'openai/text-embedding-3-small',
  taggingModel: 'openai/gpt-4o-mini',
  wikiModel: 'anthropic/claude-sonnet-4.6',
  chatModel: 'anthropic/claude-sonnet-4.6',
  autoTaggingEnabled: true,
  isTesting: false,
  testResult: null,
  testError: null,
  availableModels: [],
  isLoadingModels: false,
  ollamaHost: 'http://127.0.0.1:11434',
  ollamaStatus: 'disconnected',
  ollamaError: undefined,
  ollamaModels: [],
  isLoadingOllamaModels: false,
  ollamaContextLength: '65536',
  ollamaTimeoutSecs: '120',
  openaiCompatBaseUrl: '',
  openaiCompatApiKey: '',
  openaiCompatEmbeddingModel: '',
  openaiCompatEmbeddingDimension: '1536',
  openaiCompatLlmModel: '',
  openaiCompatContextLength: '65536',
  openaiCompatTimeoutSecs: '300',
  openaiCompatStatus: 'idle',
  openaiCompatError: null,
  selectedDefaultCategories: [...DEFAULT_TAG_CATEGORIES],
  customCategories: [],
  customCategoryInput: '',
  isSavingCategories: false,
  categoriesError: null,
  mobileToken: null,
  mobileTokenName: 'mobile-setup',
  feedUrl: '',
  ingestUrl: '',
  tutorialAtomId: null,
  tutorialEmbeddingDone: false,
  tutorialTaggingDone: false,
  tutorialTagsExtracted: [],
  tutorialCreating: false,
};

function reducer(state: OnboardingState, action: OnboardingAction): OnboardingState {
  switch (action.type) {
    case 'SET_STEP':
      return { ...state, currentStep: action.step };
    case 'NEXT_STEP':
      return { ...state, currentStep: state.currentStep + 1 };
    case 'PREV_STEP':
      return { ...state, currentStep: Math.max(0, state.currentStep - 1) };
    case 'SET_SERVER_URL':
      return { ...state, serverUrl: action.value, serverTestResult: null, serverTestError: null };
    case 'SET_SERVER_TOKEN':
      return { ...state, serverToken: action.value, serverTestResult: null, serverTestError: null };
    case 'SET_SERVER_TEST':
      return { ...state, serverTestResult: action.result, serverTestError: action.error || null };
    case 'SET_TESTING_SERVER':
      return { ...state, isTestingServer: action.value };
    case 'SET_PROVIDER': {
      const base = { ...state, provider: action.value, testResult: null, testError: null };
      if (action.value === 'ollama') {
        base.embeddingModel = '';
        base.taggingModel = '';
      } else if (action.value === 'openrouter') {
        base.embeddingModel = 'openai/text-embedding-3-small';
        base.taggingModel = 'openai/gpt-4o-mini';
        base.wikiModel = 'anthropic/claude-sonnet-4.6';
        base.chatModel = 'anthropic/claude-sonnet-4.6';
      }
      return base;
    }
    case 'SET_API_KEY':
      return { ...state, apiKey: action.value, testResult: null, testError: null };
    case 'SET_EMBEDDING_MODEL':
      return { ...state, embeddingModel: action.value };
    case 'SET_TAGGING_MODEL':
      return { ...state, taggingModel: action.value };
    case 'SET_WIKI_MODEL':
      return { ...state, wikiModel: action.value };
    case 'SET_CHAT_MODEL':
      return { ...state, chatModel: action.value };
    case 'SET_AUTO_TAGGING':
      return { ...state, autoTaggingEnabled: action.value };
    case 'SET_TESTING':
      return { ...state, isTesting: action.value };
    case 'SET_TEST_RESULT':
      return { ...state, testResult: action.result, testError: action.error || null };
    case 'SET_AVAILABLE_MODELS':
      return { ...state, availableModels: action.models };
    case 'SET_LOADING_MODELS':
      return { ...state, isLoadingModels: action.value };
    case 'SET_OLLAMA_HOST':
      return { ...state, ollamaHost: action.value };
    case 'SET_OLLAMA_STATUS':
      return { ...state, ollamaStatus: action.status, ollamaError: action.error };
    case 'SET_OLLAMA_MODELS':
      return { ...state, ollamaModels: action.models };
    case 'SET_LOADING_OLLAMA_MODELS':
      return { ...state, isLoadingOllamaModels: action.value };
    case 'SET_OLLAMA_CONTEXT_LENGTH':
      return { ...state, ollamaContextLength: action.value };
    case 'SET_OLLAMA_TIMEOUT_SECS':
      return { ...state, ollamaTimeoutSecs: action.value };
    case 'SET_OPENAI_COMPAT_BASE_URL':
      return { ...state, openaiCompatBaseUrl: action.value };
    case 'SET_OPENAI_COMPAT_API_KEY':
      return { ...state, openaiCompatApiKey: action.value };
    case 'SET_OPENAI_COMPAT_EMBEDDING_MODEL':
      return { ...state, openaiCompatEmbeddingModel: action.value };
    case 'SET_OPENAI_COMPAT_EMBEDDING_DIMENSION':
      return { ...state, openaiCompatEmbeddingDimension: action.value };
    case 'SET_OPENAI_COMPAT_LLM_MODEL':
      return { ...state, openaiCompatLlmModel: action.value };
    case 'SET_OPENAI_COMPAT_CONTEXT_LENGTH':
      return { ...state, openaiCompatContextLength: action.value };
    case 'SET_OPENAI_COMPAT_TIMEOUT_SECS':
      return { ...state, openaiCompatTimeoutSecs: action.value };
    case 'SET_OPENAI_COMPAT_STATUS':
      return { ...state, openaiCompatStatus: action.status, openaiCompatError: action.error || null };
    case 'TOGGLE_DEFAULT_CATEGORY': {
      const has = state.selectedDefaultCategories.includes(action.name);
      return {
        ...state,
        selectedDefaultCategories: has
          ? state.selectedDefaultCategories.filter(n => n !== action.name)
          : [...state.selectedDefaultCategories, action.name],
        categoriesError: null,
      };
    }
    case 'SET_CUSTOM_CATEGORY_INPUT':
      return { ...state, customCategoryInput: action.value, categoriesError: null };
    case 'ADD_CUSTOM_CATEGORY': {
      const trimmed = state.customCategoryInput.trim();
      if (!trimmed) return state;
      if (trimmed.includes('/')) {
        return { ...state, categoriesError: 'Category names cannot contain "/".' };
      }
      const existsInDefaults = DEFAULT_TAG_CATEGORIES.some(d => d.toLowerCase() === trimmed.toLowerCase());
      const existsInCustom = state.customCategories.some(c => c.toLowerCase() === trimmed.toLowerCase());
      if (existsInDefaults || existsInCustom) {
        return { ...state, categoriesError: `"${trimmed}" is already in the list.` };
      }
      return {
        ...state,
        customCategories: [...state.customCategories, trimmed],
        customCategoryInput: '',
        categoriesError: null,
      };
    }
    case 'REMOVE_CUSTOM_CATEGORY':
      return {
        ...state,
        customCategories: state.customCategories.filter(n => n !== action.name),
        categoriesError: null,
      };
    case 'SET_SAVING_CATEGORIES':
      return { ...state, isSavingCategories: action.value };
    case 'SET_CATEGORIES_ERROR':
      return { ...state, categoriesError: action.error };
    case 'SET_MOBILE_TOKEN':
      return { ...state, mobileToken: action.token };
    case 'SET_FEED_URL':
      return { ...state, feedUrl: action.value };
    case 'SET_INGEST_URL':
      return { ...state, ingestUrl: action.value };
    case 'SET_TUTORIAL_ATOM_ID':
      return { ...state, tutorialAtomId: action.id };
    case 'SET_TUTORIAL_EMBEDDING_DONE':
      return { ...state, tutorialEmbeddingDone: action.value };
    case 'SET_TUTORIAL_TAGGING_DONE':
      return { ...state, tutorialTaggingDone: action.value, tutorialTagsExtracted: action.tags || state.tutorialTagsExtracted };
    case 'SET_TUTORIAL_CREATING':
      return { ...state, tutorialCreating: action.value };
    default:
      return state;
  }
}

export function useOnboardingState() {
  return useReducer(reducer, initialState);
}

export const STEPS: { id: StepId; label: string; required: boolean }[] = [
  { id: 'welcome', label: 'Welcome', required: true },
  { id: 'ai-provider', label: 'AI Provider', required: true },
  { id: 'tag-categories', label: 'Tag Categories', required: true },
  { id: 'integrations', label: 'Integrations', required: true },
  { id: 'tutorial', label: 'Tutorial', required: true },
];

/**
 * The steps to show in the current environment. On Atomic Cloud the AI provider
 * is provisioned by the control plane (managed key, or BYOK set in the account
 * dashboard), so the provider step is dropped — but the tenant still needs the
 * rest of setup (tag categories for auto-tagging, integrations, the tutorial).
 * All wizard machinery indexes into this list, so it stays the single source of
 * truth for step order and count.
 */
export function getVisibleSteps(): { id: StepId; label: string; required: boolean }[] {
  if (isCloudTenant()) {
    return STEPS.filter(step => step.id !== 'ai-provider');
  }
  return STEPS;
}
