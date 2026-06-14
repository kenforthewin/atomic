import { createPortal } from 'react-dom';
import { Button } from '../ui/Button';
import { StepIndicator } from './StepIndicator';
import { useOnboardingState, getVisibleSteps } from './useOnboardingState';
import { useSettingsStore } from '../../stores/settings';
import { useTagsStore } from '../../stores/tags';
import { isDesktopApp, getTransport, isCloudTenant } from '../../lib/transport';

import { WelcomeStep } from './steps/WelcomeStep';
import { AIProviderStep } from './steps/AIProviderStep';
import { TagCategoriesStep } from './steps/TagCategoriesStep';
import { IntegrationsStep } from './steps/IntegrationsStep';
import { TutorialStep } from './steps/TutorialStep';

interface OnboardingWizardProps {
  onComplete: () => void;
}

export function OnboardingWizard({ onComplete }: OnboardingWizardProps) {
  const [state, dispatch] = useOnboardingState();
  const setSetting = useSettingsStore(s => s.setSetting);
  const testOpenRouterConnection = useSettingsStore(s => s.testOpenRouterConnection);
  const configureAutotagTargets = useTagsStore(s => s.configureAutotagTargets);

  const steps = getVisibleSteps();
  const currentStepDef = steps[state.currentStep];
  const isFirstStep = state.currentStep === 0;
  const isLastStep = state.currentStep === steps.length - 1;

  // Determine if current step can proceed
  const canProceed = (() => {
    switch (currentStepDef.id) {
      case 'welcome': {
        // Cloud is already connected via the session cookie, like the desktop
        // sidecar — no server form to satisfy before proceeding.
        if (isDesktopApp() || isCloudTenant()) return true;
        return getTransport().isConnected();
      }
      case 'ai-provider': {
        if (state.provider === 'openrouter') {
          return state.testResult === 'success';
        }
        if (state.provider === 'openai_compat') {
          return state.openaiCompatStatus === 'connected'
            && !!state.openaiCompatEmbeddingModel
            && !!state.openaiCompatLlmModel;
        }
        return state.ollamaStatus === 'connected'
          && !!state.embeddingModel
          && !!state.taggingModel;
      }
      default:
        return true; // Optional steps always allow proceeding
    }
  })();

  // Save AI provider settings before moving to step 3+
  const saveProviderSettings = async () => {
    try {
      await setSetting('provider', state.provider);

      if (state.provider === 'openrouter') {
        await setSetting('openrouter_api_key', state.apiKey);
        await setSetting('embedding_model', state.embeddingModel);
        await setSetting('tagging_model', state.taggingModel);
        await setSetting('wiki_model', state.wikiModel);
        await setSetting('chat_model', state.chatModel);
      } else if (state.provider === 'ollama') {
        await setSetting('ollama_host', state.ollamaHost);
        await setSetting('ollama_embedding_model', state.embeddingModel);
        await setSetting('ollama_llm_model', state.taggingModel);
        await setSetting('ollama_context_length', state.ollamaContextLength);
        await setSetting('ollama_timeout_secs', state.ollamaTimeoutSecs);
      } else if (state.provider === 'openai_compat') {
        await setSetting('openai_compat_base_url', state.openaiCompatBaseUrl);
        await setSetting('openai_compat_api_key', state.openaiCompatApiKey);
        await setSetting('openai_compat_embedding_model', state.openaiCompatEmbeddingModel);
        await setSetting('openai_compat_embedding_dimension', state.openaiCompatEmbeddingDimension);
        await setSetting('openai_compat_llm_model', state.openaiCompatLlmModel);
        await setSetting('openai_compat_context_length', state.openaiCompatContextLength);
        await setSetting('openai_compat_timeout_secs', state.openaiCompatTimeoutSecs);
      }

      await setSetting('auto_tagging_enabled', state.autoTaggingEnabled ? 'true' : 'false');
    } catch (e) {
      console.error('Failed to save provider settings:', e);
    }
  };

  const handleNext = async () => {
    // Save provider settings when leaving the AI provider step
    if (currentStepDef.id === 'ai-provider') {
      // Test connection first if not already tested (OpenRouter)
      if (state.provider === 'openrouter' && state.testResult !== 'success') {
        if (!state.apiKey.trim()) return;
        dispatch({ type: 'SET_TESTING', value: true });
        try {
          await testOpenRouterConnection(state.apiKey);
          dispatch({ type: 'SET_TEST_RESULT', result: 'success' });
        } catch (e) {
          dispatch({ type: 'SET_TEST_RESULT', result: 'error', error: String(e) });
          dispatch({ type: 'SET_TESTING', value: false });
          return;
        }
        dispatch({ type: 'SET_TESTING', value: false });
      }
      await saveProviderSettings();
    }

    // Save tag categories when leaving the tag-categories step (only if auto-tagging is enabled)
    if (currentStepDef.id === 'tag-categories' && state.autoTaggingEnabled) {
      dispatch({ type: 'SET_SAVING_CATEGORIES', value: true });
      try {
        await configureAutotagTargets(state.selectedDefaultCategories, state.customCategories);
        dispatch({ type: 'SET_CATEGORIES_ERROR', error: null });
      } catch (e) {
        dispatch({ type: 'SET_CATEGORIES_ERROR', error: String(e) });
        dispatch({ type: 'SET_SAVING_CATEGORIES', value: false });
        return;
      }
      dispatch({ type: 'SET_SAVING_CATEGORIES', value: false });
    }

    if (isLastStep) {
      onComplete();
    } else {
      dispatch({ type: 'NEXT_STEP' });
    }
  };

  const handleBack = () => {
    dispatch({ type: 'PREV_STEP' });
  };

  const handleStepClick = (step: number) => {
    if (step < state.currentStep) {
      dispatch({ type: 'SET_STEP', step });
    }
  };

  const renderStep = () => {
    switch (currentStepDef.id) {
      case 'welcome':
        return <WelcomeStep state={state} dispatch={dispatch} onNext={handleNext} onComplete={onComplete} />;
      case 'ai-provider':
        return <AIProviderStep state={state} dispatch={dispatch} />;
      case 'tag-categories':
        return <TagCategoriesStep state={state} dispatch={dispatch} />;
      case 'integrations':
        return <IntegrationsStep state={state} dispatch={dispatch} />;
      case 'tutorial':
        return <TutorialStep state={state} dispatch={dispatch} />;
    }
  };

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm safe-area-padding">
      <div className="relative bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-full max-w-2xl mx-4 h-[80vh] flex flex-col animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="px-6 py-4 border-b border-[var(--color-border)] space-y-4">
          <div className="text-center">
            <h1 className="text-lg font-semibold text-[var(--color-text-primary)]">
              Set up Atomic
            </h1>
            <p className="text-xs text-[var(--color-text-secondary)] mt-0.5">
              Step {state.currentStep + 1} of {steps.length}: {currentStepDef.label}
            </p>
          </div>
          <StepIndicator currentStep={state.currentStep} onStepClick={handleStepClick} />
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto px-6 py-6">
          {renderStep()}
        </div>

        {/* Footer */}
        <div className="px-6 py-4 border-t border-[var(--color-border)] flex items-center justify-between">
          <div>
            {!isFirstStep && (
              <Button variant="ghost" onClick={handleBack}>
                Back
              </Button>
            )}
          </div>
          <div className="flex gap-2">
            <Button onClick={handleNext} disabled={!canProceed}>
              {isLastStep ? 'Finish' : 'Next'}
            </Button>
          </div>
        </div>
      </div>
    </div>,
    document.body
  );
}
