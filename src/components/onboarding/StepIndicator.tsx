import { Check } from 'lucide-react';
import { getVisibleSteps } from './useOnboardingState';

interface StepIndicatorProps {
  currentStep: number;
  onStepClick: (step: number) => void;
}

export function StepIndicator({ currentStep, onStepClick }: StepIndicatorProps) {
  const steps = getVisibleSteps();
  return (
    <div className="flex items-center justify-center gap-0 px-4">
      {steps.map((step, index) => (
        <div key={step.id} className="flex items-center">
          {/* Step circle */}
          <button
            onClick={() => index < currentStep ? onStepClick(index) : undefined}
            className={`
              w-8 h-8 rounded-full flex items-center justify-center text-xs font-medium transition-all duration-200
              ${index < currentStep
                ? 'bg-[var(--color-accent)] text-white cursor-pointer hover:bg-[var(--color-accent-hover)]'
                : index === currentStep
                ? 'bg-[var(--color-accent)] text-white ring-2 ring-[var(--color-accent)] ring-offset-2 ring-offset-[var(--color-bg-panel)]'
                : 'bg-[var(--color-bg-card)] text-[var(--color-text-secondary)] border border-[var(--color-border)]'
              }
            `}
            title={step.label}
            disabled={index > currentStep}
          >
            {index < currentStep ? (
              <Check className="w-4 h-4" strokeWidth={2} />
            ) : (
              index + 1
            )}
          </button>

          {/* Connecting line */}
          {index < steps.length - 1 && (
            <div className={`w-6 h-0.5 mx-0.5 transition-colors duration-200 ${
              index < currentStep
                ? 'bg-[var(--color-accent)]'
                : 'bg-[var(--color-border)]'
            }`} />
          )}
        </div>
      ))}
    </div>
  );
}
