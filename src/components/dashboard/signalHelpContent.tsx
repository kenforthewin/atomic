import { SignalHelp } from './SignalHelp';

export function NewWikisHelp() {
  return (
    <SignalHelp title="Ready to generate">
      <p>Tags appear here when they have enough captured atoms to support a useful wiki and no wiki exists yet.</p>
      <p>Ranking uses atom count, source variety, recent growth, existing wiki mentions, and semantic cohesion.</p>
    </SignalHelp>
  );
}

export function RevisionsHelp() {
  return (
    <SignalHelp title="Revision suggestions">
      <p>Existing wikis appear here when new tagged atoms have arrived since the article was last generated.</p>
      <p>The action creates an update proposal for review rather than rewriting the wiki immediately.</p>
    </SignalHelp>
  );
}

export function TagCleanupHelp() {
  return (
    <SignalHelp title="Tag cleanup">
      <p>These suggestions identify tags that are empty or overlap strongly with another tag.</p>
      <p>Overlap is based on shared atom membership, hierarchy context, tag names, and semantic tag similarity when available.</p>
    </SignalHelp>
  );
}

export function IdeasToConnectHelp() {
  return (
    <SignalHelp title="Ideas to connect">
      <p>These are atoms that sit near a tagged cluster but do not have that tag yet.</p>
      <p>Atomic looks at nearby semantic neighbors and suggests a tag only when several similar atoms already use it.</p>
    </SignalHelp>
  );
}

export function SimilarNotesHelp() {
  return (
    <SignalHelp title="Similar notes">
      <p>These pairs may substantially overlap. Some are based on meaning similarity, and some are exact source URL duplicates.</p>
      <p>Atomic weighs semantic similarity, source match, title similarity, shared tags, and content-length compatibility.</p>
    </SignalHelp>
  );
}

export function BrokenLinksHelp() {
  return (
    <SignalHelp title="Broken links">
      <p>These atoms contain internal links that do not currently resolve cleanly.</p>
      <p>Missing atom-id links are higher confidence. Text links are shown more conservatively because they may point to future notes.</p>
    </SignalHelp>
  );
}

export function UnderconnectedNotesHelp() {
  return (
    <SignalHelp title="Underconnected notes">
      <p>These atoms have few strong semantic connections and sparse tag context compared with the rest of the database.</p>
      <p>Atomic only checks atoms whose semantic edges are complete, so this is not a processing-status warning.</p>
    </SignalHelp>
  );
}
