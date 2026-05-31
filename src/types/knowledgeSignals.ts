export interface KnowledgeSignalTarget {
  kind: string;
  id: string;
  label: string;
}

export interface KnowledgeSignalReason {
  kind: string;
  label: string;
  value: unknown;
  contribution: number;
}

export interface KnowledgeSignalAction {
  id: string;
  label: string;
  kind: string;
}

export interface KnowledgeSignal<Evidence = Record<string, unknown>> {
  id: string;
  provider_id: string;
  target: KnowledgeSignalTarget;
  score: number;
  confidence: number;
  severity?: string;
  title: string;
  summary: string;
  reasons: KnowledgeSignalReason[];
  evidence?: Evidence;
  suggested_actions?: KnowledgeSignalAction[];
  created_at?: string;
  expires_at?: string | null;
}

export interface KnowledgeSignalActionResult<Result = Record<string, unknown>> {
  action_log_id: string;
  signal_key: string;
  provider_id: string;
  action: string;
  status: string;
  undo_supported: boolean;
  result?: Result;
}

export interface KnowledgeSignalProviderConfig {
  provider_id: string;
  enabled: boolean;
  weight: number;
  min_score: number;
  min_confidence: number;
  show_on_dashboard: boolean;
  include_in_briefing: boolean;
  config_json?: Record<string, unknown>;
}

export interface KnowledgeSignalProviderSettings {
  provider_id: string;
  name: string;
  config: KnowledgeSignalProviderConfig;
}

export interface DashboardKnowledgeSignalGroup {
  provider_id: string;
  name: string;
  evaluation_ms: number;
  signals: KnowledgeSignal[];
}

export interface DashboardKnowledgeSignalError {
  provider_id: string;
  name: string;
  evaluation_ms: number;
  message: string;
}

export interface DashboardKnowledgeSignals {
  generated_at: string;
  provider_settings: KnowledgeSignalProviderSettings[];
  groups: DashboardKnowledgeSignalGroup[];
  errors: DashboardKnowledgeSignalError[];
}

export interface WikiCandidateEvidence {
  schema?: string;
  schema_version?: number;
  tag_id?: string;
  tag_name?: string;
  atom_count?: number;
  mention_count?: number;
  source_count?: number;
  recent_count?: number;
}

export interface WikiUpdateEvidence {
  schema?: string;
  schema_version?: number;
  article_id?: string;
  tag_id?: string;
  tag_name?: string;
  article_atom_count?: number;
  current_atom_count?: number;
  new_atom_count?: number;
  new_source_count?: number;
  new_substantive_count?: number;
  new_recent_count?: number;
  inbound_link_count?: number;
  updated_at?: string;
}

export interface TagCleanupTagEvidence {
  id: string;
  name: string;
  parent_id?: string | null;
  path: string[];
  atom_count: number;
  child_count: number;
  has_wiki: boolean;
  is_autotag_target: boolean;
}

export interface TagRedundancyEvidence {
  schema?: string;
  schema_version?: number;
  primary_tag: TagCleanupTagEvidence;
  secondary_tag: TagCleanupTagEvidence;
  shared_atom_count: number;
  primary_unique_atom_count: number;
  secondary_unique_atom_count: number;
  jaccard_overlap: number;
  containment_overlap: number;
  centroid_similarity?: number | null;
  name_similarity: number;
  hierarchy_relationship: string;
  review_posture: string;
}

export interface EmptyTagEvidence {
  schema?: string;
  schema_version?: number;
  tag: TagCleanupTagEvidence;
}

export interface MissingTagOverlapEvidence {
  schema?: string;
  schema_version?: number;
  atom_id: string;
  atom_title: string;
  current_tag_count: number;
  suggested_tag: TagCleanupTagEvidence;
  nearby_tagged_atom_count: number;
  strongest_similarity: number;
  average_similarity: number;
}

export interface NearDuplicateAtomEvidenceAtom {
  id: string;
  title: string;
  source_url?: string | null;
  content_length: number;
  created_at: string;
  updated_at: string;
}

export interface NearDuplicateTagEvidence {
  id: string;
  name: string;
}

export interface NearDuplicateAtomEvidence {
  schema?: string;
  schema_version?: number;
  primary_atom: NearDuplicateAtomEvidenceAtom;
  secondary_atom: NearDuplicateAtomEvidenceAtom;
  semantic_similarity: number;
  source_match: string;
  title_similarity: number;
  shared_tags: NearDuplicateTagEvidence[];
  shared_tag_count: number;
  content_length_ratio: number;
}

export interface SourceDuplicateEvidence {
  schema?: string;
  schema_version?: number;
  primary_atom: NearDuplicateAtomEvidenceAtom;
  secondary_atom: NearDuplicateAtomEvidenceAtom;
  source_url: string;
  normalized_source_url: string;
  duplicate_count: number;
  title_similarity: number;
  content_length_ratio: number;
}

export interface UnderconnectedAtomEvidence {
  schema?: string;
  schema_version?: number;
  atom_id: string;
  atom_title: string;
  source_url?: string | null;
  content_length: number;
  tag_count: number;
  total_edge_count: number;
  strong_edge_count: number;
  strongest_similarity?: number | null;
  average_similarity?: number | null;
  captured_atom_count: number;
  edges_status: string;
}

export interface BrokenInternalLinkEvidence {
  schema?: string;
  schema_version?: number;
  link_id: string;
  source_atom_id: string;
  source_atom_title: string;
  raw_target: string;
  label?: string | null;
  target_kind: string;
  status: string;
  start_offset?: number | null;
  end_offset?: number | null;
}

export type TagCleanupEvidence = TagRedundancyEvidence | EmptyTagEvidence;
