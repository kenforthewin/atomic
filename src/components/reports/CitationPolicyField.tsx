import { memo } from 'react';
import { CitationPolicy } from '../../stores/reports';

interface CitationPolicyFieldProps {
  value: CitationPolicy;
  onChange: (next: CitationPolicy) => void;
}

const OPTIONS: { value: CitationPolicy; label: string; helper: string }[] = [
  {
    value: 'source_only',
    label: 'Cite source atoms only',
    helper: 'Citations resolve to the atoms in this run’s source scope.',
  },
  {
    value: 'context_citable',
    label: 'Allow citing context atoms',
    helper: 'The agent can cite anything in the context scope, not just the source.',
  },
];

/// Two-option radio for the report's citation policy. The default
/// (source-only) is the conservative answer for most reports — the agent
/// can only point at the atoms that triggered this run. Switching to
/// context-citable opens the citation pool to whatever shows up in
/// semantic_search results, which is the right answer for
/// contradiction-detection or open-question reports where the *point*
/// is to compare new evidence against the prior corpus.
export const CitationPolicyField = memo(function CitationPolicyField({
  value, onChange,
}: CitationPolicyFieldProps) {
  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)] mb-1">
        Citation policy
      </legend>
      {OPTIONS.map(opt => {
        const checked = opt.value === value;
        return (
          <label
            key={opt.value}
            className={`
              flex items-start gap-3 px-3 py-2 rounded-md cursor-pointer transition-colors
              border
              ${checked
                ? 'border-[var(--color-accent)]/60 bg-[var(--color-accent)]/5'
                : 'border-[var(--color-border)] hover:bg-[var(--color-bg-hover)]'
              }
            `}
          >
            <input
              type="radio"
              name="citation-policy"
              checked={checked}
              onChange={() => onChange(opt.value)}
              className="mt-1 accent-[var(--color-accent)]"
            />
            <div className="flex flex-col">
              <span className="text-sm text-[var(--color-text-primary)]">{opt.label}</span>
              <span className="text-[11px] text-[var(--color-text-tertiary)]">{opt.helper}</span>
            </div>
          </label>
        );
      })}
    </fieldset>
  );
});
