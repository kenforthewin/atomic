import { createPortal } from 'react-dom';
import { Button } from '../ui/Button';
import { StepIndicator } from './StepIndicator';
import { useOnboardingState, STEPS } from './useOnboardingState';
import { useSettingsStore } from '../../stores/settings';
import { isDesktopApp, isDesktopFreshInstall, getTransport } from '../../lib/transport';

import { WelcomeStep } from './steps/WelcomeStep';
import { AIProviderStep } from './steps/AIProviderStep';
import { IntegrationsStep } from './steps/IntegrationsStep';
import { TutorialStep } from './steps/TutorialStep';

interface OnboardingWizardProps {
  onComplete: () => void;
}

export function OnboardingWizard({ onComplete }: OnboardingWizardProps) {
  const [state, dispatch] = useOnboardingState();
  const setSetting = useSettingsStore(s => s.setSetting);
  const testOpenRouterConnection = useSettingsStore(s => s.testOpenRouterConnection);

  const currentStepDef = STEPS[state.currentStep];
  const isFirstStep = state.currentStep === 0;
  const isLastStep = state.currentStep === STEPS.length - 1;

  // Determine if current step can proceed
  const canProceed = (() => {
    switch (currentStepDef.id) {
      case 'welcome': {
        // Desktop existing install: always proceed (already connected)
        // Desktop fresh install: proceed after claim completes (calls onNext directly)
        // Web: proceed once transport is connected
        if (isDesktopApp() && !isDesktopFreshInstall()) return true;
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

  const isOptional = !currentStepDef.required;

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

    if (isLastStep) {
      onComplete();
    } else {
      dispatch({ type: 'NEXT_STEP' });
    }
  };

  const handleSkip = () => {
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
        return <WelcomeStep state={state} dispatch={dispatch} onNext={handleNext} />;
      case 'ai-provider':
        return <AIProviderStep state={state} dispatch={dispatch} />;
      case 'integrations':
        return <IntegrationsStep state={state} dispatch={dispatch} />;
      case 'tutorial':
        return <TutorialStep state={state} dispatch={dispatch} />;
    }
  };

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm">
      <div className="relative bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-full max-w-2xl mx-4 h-[80vh] flex flex-col animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="px-6 py-4 border-b border-[var(--color-border)] space-y-4">
          <div className="text-center">
            <h1 className="text-lg font-semibold text-[var(--color-text-primary)]">
              Set up Atomic
            </h1>
            <p className="text-xs text-[var(--color-text-secondary)] mt-0.5">
              Step {state.currentStep + 1} of {STEPS.length}: {currentStepDef.label}
              {isOptional && ' (optional)'}
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
            {isOptional && (
              <Button variant="secondary" onClick={handleSkip}>
                Skip
              </Button>
            )}
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
