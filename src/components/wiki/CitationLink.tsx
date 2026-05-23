interface CitationLinkProps {
  index: number;
  onClick: (event: React.MouseEvent<HTMLButtonElement>) => void;
}

/// `[N]` inline citation marker. Used across wiki articles, dashboard
/// briefings, finding reader, and chat — keep styling consistent
/// here rather than per call site.
///
/// Default color is `--color-text-primary` (white in dark theme, near-
/// black in light) so the marker reads as part of the prose, with the
/// `[N]` bracketing as the affordance for "this is clickable". Hover
/// shifts to the accent + underline as the click feedback.
export function CitationLink({ index, onClick }: CitationLinkProps) {
  return (
    <sup>
      <button
        onClick={onClick}
        className="text-[var(--color-text-primary)] hover:text-[var(--color-accent-light)] hover:underline transition-colors text-[10px] font-medium mx-px"
      >
        [{index}]
      </button>
    </sup>
  );
}

