import { useEffect, useId, useRef, useState, useCallback, useMemo, type KeyboardEvent } from 'react';
import { Check, Loader2 } from 'lucide-react';
import { toast } from 'sonner';
import { useUIStore } from '../../stores/ui';
import { useDatabasesStore } from '../../stores/databases';
import { getAtomLinks, getAtomPositions, getGlobalCanvas, saveAtomPositions, type AtomLink, type GlobalCanvasData } from '../../lib/api';
import { getTransport } from '../../lib/transport';
import Graph from 'graphology';
import Sigma from 'sigma';
import { EdgeRectangleProgram, NodeCircleProgram } from 'sigma/rendering';
import type { CameraState } from 'sigma/types';
import EdgeCurveProgram from '@sigma/edge-curve';
import {
  CANVAS_THEMES,
  DEFAULT_THEME,
  nodeColor,
  edgeColor,
  type CanvasTheme,
} from './sigma/themes';
import { AtomPreviewPopover } from './AtomPreviewPopover';
import { useCanvasStore } from '../../stores/canvas';
import { ACCOUNT_NETWORK_CATEGORIES, accountNetworkCategory, accountNetworkSlug } from './accountNetworkCategories';
import type { AccountNetworkCategory } from './accountNetworkCategories';
import { accountNetworkEdges, type AccountNetworkEdge } from './accountNetworkEdges';
import {
  computeAccountChartLayout,
  emptyAccountLayoutSettings,
  readAccountLayoutSettings,
  writeAccountLayoutSettings,
  type AccountLayoutMode,
  type AccountLayoutSettings,
} from './accountNetworkLayout';
import { categoryCameraTarget, toggleCategory } from './categoryFocus';
import StarNodeProgram from './sigma/StarNodeProgram';
import { parseCanvasImage } from './accountNetworkProfile';
import type { AtomWithTags } from '../../stores/atoms';
import { accountResearchChecks, enrichedCategoryKeys, verifiedEntryPoints, type AccountResearchNote } from './accountNetworkEnrichment';
import { AccountResearchPanel } from './AccountResearchPanel';

type CategoryKey = AccountNetworkCategory['key'];
type AccountControlTab = 'categories' | 'research';
const EMPTY_RESEARCH_NOTES: AccountResearchNote[] = [];
const EMPTY_ATOM_LINKS: AtomLink[] = [];

function truncLabel(str: string, max: number): string {
  return str.length > max ? str.substring(0, max - 1) + '\u2026' : str;
}

function parseRgbColor(s: string): [number, number, number] | null {
  const m = s.match(/^rgb\((\d+)\s*,\s*(\d+)\s*,\s*(\d+)\)$/);
  if (!m) return null;
  return [+m[1], +m[2], +m[3]];
}

export type SigmaCanvasMode = 'main' | 'preview';

interface SigmaCanvasProps {
  /** 'main' runs the full interactive canvas; 'preview' renders a thumbnail
   *  with no chrome and no chat controller. Static by default — pass
   *  filterAtomIds to make it interactive (pan/zoom + clickable nodes). */
  mode?: SigmaCanvasMode;
  /** Click handler for static preview mode — fires on any click inside the
   *  container. Ignored when filterAtomIds is set (the canvas is interactive
   *  and clicks are handled per-node via onPreviewNodeClick). */
  onPreviewClick?: () => void;
  /** Subset the rendered graph to these atoms + their 1-hop neighbors from
   *  the existing edge set. When provided in preview mode, the canvas
   *  becomes interactive (pan/zoom + clickable nodes). */
  filterAtomIds?: string[];
  /** Click handler for nodes in interactive preview mode. */
  onPreviewNodeClick?: (atomId: string) => void;
}

export function SigmaCanvas({
  mode = 'main',
  onPreviewClick,
  filterAtomIds,
  onPreviewNodeClick,
}: SigmaCanvasProps = {}) {
  const isPreview = mode === 'preview';
  const isInteractivePreview = isPreview && filterAtomIds !== undefined;
  // Stable key for filterAtomIds — array identity changes shouldn't rebuild.
  const filterKey = filterAtomIds ? [...filterAtomIds].sort().join(',') : null;
  // Refs let event handlers see the latest callback without re-running the
  // graph-build effect (which would tear down and rebuild Sigma).
  const onPreviewNodeClickRef = useRef(onPreviewNodeClick);
  onPreviewNodeClickRef.current = onPreviewNodeClick;
  const openReader = useUIStore(s => s.openReader);
  const selectedTagId = useUIStore(s => s.selectedTagId);
  const activeDbId = useDatabasesStore(s => s.activeId);
  const containerRef = useRef<HTMLDivElement>(null);
  // The hover pill renders into this div, which lives outside the
  // overflow-hidden Sigma container. That lets long titles spill past the
  // canvas edges instead of getting clipped (the canvas's drawing surface
  // is exactly the panel size — flipping sides only helps when the label
  // is narrower than the longer-side gap).
  const hoverPillRef = useRef<HTMLDivElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const graphRef = useRef<Graph | null>(null);
  const [data, setData] = useState<GlobalCanvasData | null>(null);
  const [recordedEdges, setRecordedEdges] = useState<{ rootId: string; edges: AccountNetworkEdge[]; links: AtomLink[] } | null>(null);
  const [linksLoading, setLinksLoading] = useState(false);
  const [linksError, setLinksError] = useState(false);
  const [linkRetry, setLinkRetry] = useState(0);
  const [researchNotes, setResearchNotes] = useState<{ rootId: string; notes: AccountResearchNote[] } | null>(null);
  const [researchLoading, setResearchLoading] = useState(false);
  const [researchError, setResearchError] = useState(false);
  const [researchRetry, setResearchRetry] = useState(0);
  const [canvasReload, setCanvasReload] = useState(0);
  const [animateEntryPaths, setAnimateEntryPaths] = useState(() => !window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  const [reduceMotion, setReduceMotion] = useState(() => window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  useEffect(() => {
    const preference = window.matchMedia('(prefers-reduced-motion: reduce)');
    const update = () => setReduceMotion(preference.matches);
    preference.addEventListener('change', update);
    return () => preference.removeEventListener('change', update);
  }, []);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [theme, setTheme] = useState<CanvasTheme>(DEFAULT_THEME);
  const [themePickerOpen, setThemePickerOpen] = useState(false);
  const [controlTab, setControlTab] = useState<AccountControlTab>('research');
  const controlTabsId = useId();
  const controlsRef = useRef<HTMLElement>(null);
  const categoryTabRef = useRef<HTMLButtonElement>(null);
  const researchTabRef = useRef<HTMLButtonElement>(null);
  const [selectedCategories, setSelectedCategories] = useState<Set<CategoryKey>>(new Set());
  const selectedCategoriesRef = useRef<ReadonlySet<CategoryKey>>(selectedCategories);
  const savedCategoryCameraRef = useRef<CameraState | null>(null);
  const focusCategoriesRef = useRef<(selected: ReadonlySet<CategoryKey>) => void>(() => {});
  const [edgeThreshold, setEdgeThreshold] = useState(0);
  const edgeThresholdRef = useRef(0);
  const edgeAnimProgress = useRef(0); // 0 = invisible, 1 = fully visible
  const themeRef = useRef(theme);
  themeRef.current = theme;
  const savedPositionsRef = useRef<Record<string, { x: number; y: number }>>({});
  const [layoutSettings, setLayoutSettings] = useState<AccountLayoutSettings>(emptyAccountLayoutSettings);
  const layoutSettingsRef = useRef(layoutSettings);
  const applyLayoutRef = useRef<(mode: AccountLayoutMode, edges: AccountNetworkEdge[], force?: boolean) => void>(() => {});
  const refitLayoutRef = useRef<() => void>(() => {});

  const accountRoots = !isPreview && selectedTagId
    ? (data?.atoms ?? []).filter(atom => atom.tag_ids.includes(selectedTagId)
      && accountNetworkCategory(atom.source_url)?.key === 'account')
    : [];
  const accountRoot = accountRoots.length === 1 ? accountRoots[0] : null;
  const accountRootId = accountRoot?.atom_id ?? null;
  const accountSlug = accountNetworkSlug(accountRoot?.source_url);
  const accountTagFilterId = accountRoot && accountSlug ? selectedTagId : null;
  useEffect(() => setControlTab('research'), [accountTagFilterId, activeDbId]);
  const selectControlTab = (tab: AccountControlTab) => {
    setControlTab(tab);
    controlsRef.current?.scrollTo({ top: 0 });
  };
  const handleControlTabKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const next: AccountControlTab = event.key === 'Home' ? 'categories'
      : event.key === 'End' ? 'research'
        : controlTab === 'categories' ? 'research' : 'categories';
    selectControlTab(next);
    (next === 'categories' ? categoryTabRef : researchTabRef).current?.focus();
  };
  const accountAtoms = useMemo(() => !accountTagFilterId ? [] : (data?.atoms ?? []).filter(atom =>
    atom.tag_ids.includes(accountTagFilterId)
    && accountNetworkSlug(atom.source_url) === accountSlug
    && accountNetworkCategory(atom.source_url),
  ), [data, accountTagFilterId, accountSlug]);
  const activeNotes = researchNotes?.rootId === accountRootId ? researchNotes.notes : EMPTY_RESEARCH_NOTES;
  const activeLinks = recordedEdges?.rootId === accountRootId ? recordedEdges.links : EMPTY_ATOM_LINKS;
  const entryPoints = useMemo(() => verifiedEntryPoints(accountAtoms, activeNotes, activeLinks), [accountAtoms, activeNotes, activeLinks]);
  const researchChecks = useMemo(() => accountResearchChecks(accountAtoms, activeNotes, entryPoints), [accountAtoms, activeNotes, entryPoints]);
  const enrichedCategories = useMemo(() => enrichedCategoryKeys(accountAtoms, researchChecks), [accountAtoms, researchChecks]);
  const entryPointRows = useMemo(() => {
    const titles = new Map(accountAtoms.map(atom => [atom.atom_id, atom.title]));
    return entryPoints.map(entry => ({
      ...entry,
      sourceTitle: titles.get(entry.source) ?? 'Source note',
      targetTitle: titles.get(entry.target) ?? 'Target note',
    }));
  }, [accountAtoms, entryPoints]);
  const documentedEdges = useMemo(() => {
    if (recordedEdges?.rootId !== accountRootId) return [];
    const edges = recordedEdges.edges.map(edge => ({ ...edge }));
    for (const entry of entryPoints) {
      const existing = edges.find(edge => edge.source === entry.source && edge.target === entry.target
        || edge.source === entry.target && edge.target === entry.source);
      if (existing) { existing.relation = 'entry-point'; existing.weight = 1; }
      else edges.push({ source: entry.source, target: entry.target, weight: 1, relation: 'entry-point' });
    }
    return edges;
  }, [recordedEdges, accountRootId, entryPoints]);
  const effectiveLayoutMode: AccountLayoutMode = accountRootId && recordedEdges?.rootId === accountRootId
    ? layoutSettings.mode : 'freeform';
  const layoutModeRef = useRef<AccountLayoutMode>(effectiveLayoutMode);
  layoutModeRef.current = effectiveLayoutMode;
  const categoryCounts = new Map<CategoryKey, number>();
  if (accountTagFilterId) {
    for (const atom of data?.atoms ?? []) {
      if (!atom.tag_ids.includes(accountTagFilterId)
        || accountNetworkSlug(atom.source_url) !== accountSlug) continue;
      const category = accountNetworkCategory(atom.source_url);
      if (category) categoryCounts.set(category.key, (categoryCounts.get(category.key) ?? 0) + 1);
    }
  }

  useEffect(() => {
    const next = accountRootId
      ? readAccountLayoutSettings(localStorage, activeDbId ?? 'default', accountRootId)
      : emptyAccountLayoutSettings();
    layoutSettingsRef.current = next;
    setLayoutSettings(next);
  }, [accountRootId, activeDbId]);

  useEffect(() => {
    applyLayoutRef.current(effectiveLayoutMode, documentedEdges);
  }, [effectiveLayoutMode, documentedEdges]);

  useEffect(() => {
    if (!accountRootId || !containerRef.current) return;
    let timer: number | null = null;
    const observer = new ResizeObserver(() => {
      if (timer !== null) window.clearTimeout(timer);
      timer = window.setTimeout(() => refitLayoutRef.current(), 160);
    });
    observer.observe(containerRef.current);
    return () => {
      observer.disconnect();
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [accountRootId]);

  useEffect(() => {
    const empty = new Set<CategoryKey>();
    selectedCategoriesRef.current = empty;
    savedCategoryCameraRef.current = null;
    setSelectedCategories(empty);
    sigmaRef.current?.refresh();
  }, [accountTagFilterId, activeDbId]);

  useEffect(() => {
    if (!accountSlug) return;
    const transport = getTransport();
    const belongsToAccount = (atom: { source_url?: string | null }) => accountNetworkSlug(atom.source_url) === accountSlug;
    const updated = transport.subscribe<{ source_url?: string | null }>('atom-updated', atom => {
      if (!belongsToAccount(atom)) return;
      setResearchRetry(count => count + 1);
      setLinkRetry(count => count + 1);
    });
    const created = transport.subscribe<{ source_url?: string | null }>('atom-created', atom => {
      if (belongsToAccount(atom)) setCanvasReload(count => count + 1);
    });
    return () => { updated(); created(); };
  }, [accountSlug]);

  // Hover emphasis: when a node is hovered, dim everything outside its neighborhood.
  // neighborsRef lets the edge/node reducers answer "is X a neighbor of hovered?" in O(1).
  const hoveredNodeRef = useRef<string | null>(null);
  const neighborsRef = useRef<Map<string, Set<string>>>(new Map());
  // hoverAnim: 0 = no emphasis, 1 = fully emphasized. Reducers interpolate
  // against this so the enter/leave transitions fade instead of snapping.
  const hoverAnimRef = useRef(0);
  const hoverTargetRef = useRef(0);
  // Pinned node: the node whose popover is open. Its outline persists while
  // the popover is open so hovering other nodes still shows their titles.
  const pinnedNodeRef = useRef<string | null>(null);
  // Lifted out of the sigma useEffect so pinNode (defined before main-mode
  // branch) can kick the hover-fade animation loop.
  const startHoverAnimRef = useRef<() => void>(() => {});

  // Atom preview popover state
  const [previewAtomId, setPreviewAtomId] = useState<string | null>(null);
  const [previewAnchorRect, setPreviewAnchorRect] = useState<{ top: number; left: number; bottom: number; width: number } | null>(null);

  const closePreview = useCallback(() => {
    setPreviewAtomId(null);
    setPreviewAnchorRect(null);
    pinnedNodeRef.current = null;
    // Redraw so the pinned ring disappears. Hover state is already correct
    // because enterNode/leaveNode have been running normally all along.
    sigmaRef.current?.refresh();
  }, []);

  const persistLayoutSettings = (next: AccountLayoutSettings) => {
    if (!accountRootId) return;
    layoutSettingsRef.current = next;
    setLayoutSettings(next);
    if (!writeAccountLayoutSettings(localStorage, activeDbId ?? 'default', accountRootId, next)) {
      toast.error('Could not save this layout on this device');
    }
  };

  const changeLayout = (mode: AccountLayoutMode) => {
    if (mode !== 'freeform' && recordedEdges?.rootId !== accountRootId) return;
    if (layoutSettingsRef.current.mode === mode) return;
    closePreview();
    persistLayoutSettings({ ...layoutSettingsRef.current, mode });
  };

  const resetChartLayout = () => {
    const mode = layoutSettingsRef.current.mode;
    if (mode === 'freeform' || !recordedEdges) return;
    const next: AccountLayoutSettings = {
      ...layoutSettingsRef.current,
      positions: { ...layoutSettingsRef.current.positions, [mode]: {} },
    };
    persistLayoutSettings(next);
    applyLayoutRef.current(mode, documentedEdges, true);
  };

  const toggleAccountCategory = (key: CategoryKey) => {
    if (!categoryCounts.get(key)) return;
    const sigma = sigmaRef.current;
    if (!sigma) return;
    const next = toggleCategory(selectedCategoriesRef.current, key);
    if (selectedCategoriesRef.current.size === 0 && next.size > 0) {
      savedCategoryCameraRef.current = sigma.getCamera().getState();
    }
    selectedCategoriesRef.current = next;
    setSelectedCategories(next);
    closePreview();
    sigma.refresh();
    if (next.size) {
      focusCategoriesRef.current(next);
    } else if (savedCategoryCameraRef.current) {
      void sigma.getCamera().animate(savedCategoryCameraRef.current, { duration: 600 });
      savedCategoryCameraRef.current = null;
    }
  };

  // Build a set of atom IDs that match the selected tag
  const selectedTagRef = useRef(selectedTagId);
  selectedTagRef.current = selectedTagId;

  // Fetch global canvas data
  useEffect(() => {
    let cancelled = false;
    setIsLoading(true);
    setError(null);

    Promise.all([getGlobalCanvas(), getAtomPositions()])
      .then(([result, positions]) => {
        if (!cancelled) {
          savedPositionsRef.current = Object.fromEntries(
            positions.filter(pos => Number.isFinite(pos.x) && Number.isFinite(pos.y))
              .map(pos => [pos.atom_id, { x: pos.x, y: pos.y }]),
          );
          setData(result);
          setIsLoading(false);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err.message || 'Failed to load canvas');
          setIsLoading(false);
        }
      });

    return () => { cancelled = true; };
  }, [activeDbId, canvasReload]);

  // Load documented links for the chart views. Freeform keeps semantic edges.
  useEffect(() => {
    if (!data || !accountTagFilterId || !accountRootId || !accountSlug) {
      setRecordedEdges(null);
      setLinksLoading(false);
      setLinksError(false);
      return;
    }
    let cancelled = false;
    setRecordedEdges(current => current?.rootId === accountRootId ? current : null);
    setLinksLoading(true);
    setLinksError(false);
    const atoms = accountAtoms;
    const linkSources = atoms;
    let nextIndex = 0;
    const allLinks: Awaited<ReturnType<typeof getAtomLinks>> = [];
    const worker = async () => {
      while (nextIndex < linkSources.length) {
        const atom = linkSources[nextIndex++];
        allLinks.push(...await getAtomLinks(atom.atom_id));
      }
    };
    void Promise.all(Array.from({ length: Math.min(6, linkSources.length) }, worker))
      .then(() => {
        if (cancelled) return;
        setRecordedEdges({ rootId: accountRootId, edges: accountNetworkEdges(atoms, allLinks), links: allLinks });
        setLinksLoading(false);
      })
      .catch(() => {
        if (cancelled) return;
        setRecordedEdges(null);
        setLinksLoading(false);
        setLinksError(true);
      });
    return () => { cancelled = true; };
  }, [data, accountTagFilterId, accountRootId, accountSlug, accountAtoms, activeDbId, linkRetry]);

  // Read the recorded enrichment and research outcomes from account notes.
  useEffect(() => {
    if (!accountTagFilterId || !accountRootId) {
      setResearchNotes(null);
      setResearchLoading(false);
      setResearchError(false);
      return;
    }
    let cancelled = false;
    setResearchNotes(current => current?.rootId === accountRootId ? current : null);
    setResearchLoading(true);
    setResearchError(false);
    const sources = accountAtoms;
    let nextIndex = 0;
    const notes: AccountResearchNote[] = [];
    const worker = async () => {
      while (nextIndex < sources.length) {
        const atom = sources[nextIndex++];
        const result = await getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id: atom.atom_id });
        if (result) notes.push({ atomId: atom.atom_id, content: result.content });
      }
    };
    void Promise.all(Array.from({ length: Math.min(6, sources.length) }, worker))
      .then(() => {
        if (cancelled) return;
        setResearchNotes({ rootId: accountRootId, notes });
        setResearchLoading(false);
      })
      .catch(() => {
        if (cancelled) return;
        setResearchNotes(null);
        setResearchLoading(false);
        setResearchError(true);
      });
    return () => { cancelled = true; };
  }, [accountTagFilterId, accountRootId, accountAtoms, activeDbId, researchRetry]);

  // Precomputed data for the graph
  const graphDataRef = useRef<{
    edgeCounts: Map<string, number>;
    maxEdges: number;
  } | null>(null);

  // Create Sigma graph when data is loaded
  useEffect(() => {
    const container = containerRef.current;
    if (!container || !data || data.atoms.length === 0) return;
    pinnedNodeRef.current = null;
    hoveredNodeRef.current = null;
    hoverAnimRef.current = 0;
    hoverTargetRef.current = 0;
    setPreviewAtomId(null);
    setPreviewAnchorRect(null);

    // For an interactive preview, narrow the graph to the seed atoms plus
    // their 1-hop neighbors from the existing edge set. We reuse the global
    // PCA positions so the subset lands in the same region of space the user
    // would see if they zoomed there in the main canvas — the camera then
    // fits-to-bbox over just those nodes.
    let atoms = data.atoms;
    let edges = data.edges;
    const accountNetworkMode = accountTagFilterId !== null;
    if (accountTagFilterId) {
      atoms = data.atoms.filter(atom =>
        atom.tag_ids.includes(accountTagFilterId)
        && accountNetworkSlug(atom.source_url) === accountSlug
        && accountNetworkCategory(atom.source_url),
      );
      const included = new Set(atoms.map(atom => atom.atom_id));
      edges = data.edges.filter(edge => included.has(edge.source) && included.has(edge.target));
    }
    if (filterAtomIds && filterAtomIds.length > 0) {
      const seeds = new Set(filterAtomIds);
      const included = new Set(seeds);
      for (const edge of data.edges) {
        if (seeds.has(edge.source)) included.add(edge.target);
        else if (seeds.has(edge.target)) included.add(edge.source);
      }
      atoms = data.atoms.filter(a => included.has(a.atom_id));
      edges = data.edges.filter(e => included.has(e.source) && included.has(e.target));
    }
    if (atoms.length === 0) return;

    if (sigmaRef.current) {
      sigmaRef.current.kill();
      sigmaRef.current = null;
    }

    const graph = new Graph();
    graphRef.current = graph;
    const scale = 500;

    // Compute per-atom edge count (over the rendered subset, if filtered)
    const edgeCounts = new Map<string, number>();
    for (const edge of edges) {
      edgeCounts.set(edge.source, (edgeCounts.get(edge.source) || 0) + 1);
      edgeCounts.set(edge.target, (edgeCounts.get(edge.target) || 0) + 1);
    }
    const maxEdges = Math.max(1, ...edgeCounts.values());
    graphDataRef.current = { edgeCounts, maxEdges };

    // Build atom → cluster index map
    const atomCluster = new Map<string, number>();
    for (let i = 0; i < data.clusters.length; i++) {
      for (const atomId of data.clusters[i].atom_ids) {
        atomCluster.set(atomId, i);
      }
    }

    // Add atom nodes at center — will animate to PCA positions
    const targetPositions: Record<string, { x: number; y: number }> = {};
    for (const atom of atoms) {
      const connectivity = (edgeCounts.get(atom.atom_id) || 0) / maxEdges;
      const clusterIdx = atomCluster.get(atom.atom_id);
      const category = accountNetworkMode ? accountNetworkCategory(atom.source_url) : null;
      targetPositions[atom.atom_id] = savedPositionsRef.current[atom.atom_id]
        ?? { x: atom.x * scale, y: atom.y * scale };
      graph.addNode(atom.atom_id, {
        x: 0,
        y: 0,
        size: category?.key === 'account' ? 20 : category?.key === 'executive' ? 17 : category?.key === 'alumni' ? 15 : 2.5 + connectivity * 5,
        color: category?.color ?? nodeColor(theme, connectivity, clusterIdx),
        label: truncLabel(atom.title || atom.atom_id.substring(0, 8), 30),
        fullLabel: atom.title || atom.atom_id.substring(0, 8),
        connectivity,
        clusterIndex: clusterIdx,
        tagIds: atom.tag_ids,
        sourceUrl: atom.source_url,
        categoryKey: category?.key,
        type: category?.key === 'account' ? 'star' : 'circle',
      });
    }

    const setGraphEdges = (nextEdges: typeof edges, documented: boolean) => {
      graph.clearEdges();
      let minW = 1, maxW = 0;
      for (const edge of nextEdges) {
        if (edge.weight < minW) minW = edge.weight;
        if (edge.weight > maxW) maxW = edge.weight;
      }
      const wRange = Math.max(maxW - minW, 0.001);
      const neighbors = new Map<string, Set<string>>();
      for (const edge of nextEdges) {
        if (!graph.hasNode(edge.source) || !graph.hasNode(edge.target)) continue;
        if (graph.hasEdge(edge.source, edge.target) || graph.hasEdge(edge.target, edge.source)) continue;
        graph.addEdge(edge.source, edge.target, {
          weight: documented ? edge.weight : (edge.weight - minW) / wRange,
          type: documented ? 'line' : 'curved',
          relation: documented ? (edge as AccountNetworkEdge).relation : undefined,
        });
        if (!neighbors.has(edge.source)) neighbors.set(edge.source, new Set());
        if (!neighbors.has(edge.target)) neighbors.set(edge.target, new Set());
        neighbors.get(edge.source)!.add(edge.target);
        neighbors.get(edge.target)!.add(edge.source);
      }
      neighborsRef.current = neighbors;
    };
    setGraphEdges(edges, false);

    const sigma = new Sigma(graph, container, {
      // Atom labels are drawn manually on the overlay canvas (drawLabels) with
      // real collision detection, so Sigma's built-in label pass is disabled.
      renderLabels: false,
      labelFont: 'system-ui, -apple-system, sans-serif',
      defaultEdgeColor: '#333',
      defaultNodeColor: '#555',
      defaultEdgeType: 'curved',
      // Sort by the reducer's zIndex so hover-incident edges paint above the
      // dimmed background edges.
      zIndex: true,
      edgeProgramClasses: {
        curved: EdgeCurveProgram,
        line: EdgeRectangleProgram,
      },
      nodeProgramClasses: {
        circle: NodeCircleProgram,
        star: StarNodeProgram,
      },
      minCameraRatio: 0.01,
      maxCameraRatio: 10,
      stagePadding: 40,
      // Hover pill + ring are drawn on our own labelCanvas (drawLabels) so
      // they stack above atom/cluster labels. Sigma's hover canvas sits below
      // labelCanvas, so drawing the hover pill here would render it behind.
      defaultDrawNodeHover: () => {},
      nodeReducer: (node, attrs) => {
        const selected = selectedCategoriesRef.current;
        const categoryKey = (attrs as { categoryKey?: CategoryKey }).categoryKey;
        const matchesCategory = categoryKey === 'account' || (!!categoryKey && selected.has(categoryKey));
        const focusedAttrs = selected.size
          ? matchesCategory
            ? { ...attrs, size: Math.max(12, (attrs.size || 4) * 1.8), zIndex: 2 }
            : { ...attrs, color: 'rgba(70, 70, 70, 0.22)', size: (attrs.size || 4) * 0.7, label: '' }
          : attrs;
        const hovered = hoveredNodeRef.current;
        const pinned = pinnedNodeRef.current;
        if (hovered || pinned) {
          if (hovered && node === hovered) return { ...focusedAttrs, color: attrs.color, size: Math.max(focusedAttrs.size || 4, attrs.size || 4), zIndex: 3 };
          if (pinned && node === pinned) return { ...focusedAttrs, color: attrs.color, size: Math.max(focusedAttrs.size || 4, attrs.size || 4), zIndex: 3 };
          const isNeighbor =
            (hovered && neighborsRef.current.get(hovered)?.has(node)) ||
            (pinned && neighborsRef.current.get(pinned)?.has(node));
          if (isNeighbor) return { ...focusedAttrs, zIndex: 1 };
          if (selected.size) return focusedAttrs;
          // Non-neighbors dim. Hover fades in/out via hoverAnim; pin holds
          // the dim at full strength so edges/nodes stay faded after the
          // cursor moves off the pinned node.
          const dim = pinned ? 1 : hoverAnimRef.current;
          const rgb = parseRgbColor(attrs.color as string);
          const color = rgb
            ? `rgb(${Math.round(rgb[0] + (60 - rgb[0]) * dim)},${Math.round(rgb[1] + (60 - rgb[1]) * dim)},${Math.round(rgb[2] + (60 - rgb[2]) * dim)})`
            : attrs.color;
          return {
            ...attrs,
            color,
            size: (attrs.size || 4) * (1 - 0.45 * dim),
            label: dim > 0.5 ? '' : (attrs.label as string),
          };
        }
        if (selected.size) return focusedAttrs;
        const tagId = selectedTagRef.current;
        if (!tagId) return attrs;
        const tagIds = (attrs as any).tagIds as string[] | undefined;
        const matches = tagIds?.includes(tagId);
        if (matches) return attrs;
        return {
          ...attrs,
          color: 'rgba(50, 50, 50, 0.3)',
          size: (attrs.size || 4) * 0.6,
          label: '',
        };
      },
      edgeReducer: (edge, attrs) => {
        const w = (attrs as any).weight ?? 0.5;
        const hovered = hoveredNodeRef.current;
        const pinned = pinnedNodeRef.current;
        const t = themeRef.current;
        const anim = edgeAnimProgress.current;
        const selected = selectedCategoriesRef.current;
        if (selected.size) {
          if (layoutModeRef.current === 'freeform' && w < edgeThresholdRef.current) return { ...attrs, hidden: true };
          const g = graphRef.current!;
          const sourceCategory = g.getNodeAttribute(g.source(edge), 'categoryKey') as CategoryKey | undefined;
          const targetCategory = g.getNodeAttribute(g.target(edge), 'categoryKey') as CategoryKey | undefined;
          const connectsSelected = !!sourceCategory && !!targetCategory
            && (selected.has(sourceCategory) || selected.has(targetCategory));
          return {
            ...attrs,
            color: edgeColor(t, w * anim * (connectsSelected ? 1 : 0.12)),
            size: (0.2 + w * 0.7) * anim * (connectsSelected ? 1 : 0.35),
            zIndex: connectsSelected ? 1 : 0,
          };
        }
        if (hovered || pinned) {
          const g = graphRef.current!;
          const src = g.source(edge);
          const dst = g.target(edge);
          const touchesHovered = hovered && (src === hovered || dst === hovered);
          const touchesPinned = pinned && (src === pinned || dst === pinned);
          const h = hoverAnimRef.current;
          if (touchesHovered) {
            // Hover boosts brightness + size on its incident edges.
            const bright = w * anim * (1 + 0.4 * h);
            const size = (0.2 + w * 0.7) * anim + ((0.5 + w * 1.2) * anim - (0.2 + w * 0.7) * anim) * h;
            return {
              ...attrs,
              color: edgeColor(t, Math.min(1, bright)),
              size,
              zIndex: 1,
            };
          }
          if (touchesPinned) {
            // Pinned edges stay at normal brightness — they don't pulse like hover.
            return {
              ...attrs,
              color: edgeColor(t, w * anim),
              size: (0.2 + w * 0.7) * anim,
              zIndex: 1,
            };
          }
          // Non-incident: fade. Pin holds the fade at full so edges stay dim
          // after the cursor leaves the pinned node.
          const dim = pinned ? 1 : h;
          return {
            ...attrs,
            color: edgeColor(t, w * anim * (1 - dim)),
            size: (0.2 + w * 0.7) * anim * (1 - dim),
          };
        }
        if (layoutModeRef.current === 'freeform' && w < edgeThresholdRef.current) {
          return { ...attrs, hidden: true };
        }
        return {
          ...attrs,
          color: edgeColor(t, w * anim),
          size: (0.2 + w * 0.7) * anim,
        };
      },
    });

    sigmaRef.current = sigma;

    // Cluster labels canvas
    const labelCanvas = document.createElement('canvas');
    labelCanvas.style.position = 'absolute';
    labelCanvas.style.inset = '0';
    labelCanvas.style.pointerEvents = 'none';
    labelCanvas.style.zIndex = '10';
    container.appendChild(labelCanvas);
    const nodeImages = new Map<string, HTMLImageElement>();
    let cancelledMedia = false;

    if (accountNetworkMode) {
      for (const atom of atoms) {
        const category = accountNetworkCategory(atom.source_url)?.key;
        if (category !== 'executive' && category !== 'alumni') continue;
        void getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id: atom.atom_id })
          .then((detail) => {
            if (cancelledMedia) return;
            const url = parseCanvasImage(detail?.content ?? '');
            if (!url) return;
            const image = new Image();
            image.onload = () => {
              if (cancelledMedia) return;
              nodeImages.set(atom.atom_id, image);
              drawLabels();
            };
            image.src = url;
          })
          .catch(() => { /* Keep the category-colored node when a portrait or logo cannot load. */ });
      }
    }


    function drawLabels() {
      const width = container!.clientWidth;
      const height = container!.clientHeight;
      const ratio = window.devicePixelRatio || 1;
      labelCanvas.width = width * ratio;
      labelCanvas.height = height * ratio;
      labelCanvas.style.width = `${width}px`;
      labelCanvas.style.height = `${height}px`;

      const ctx = labelCanvas.getContext('2d');
      if (!ctx) return;
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);

      const t = themeRef.current;

      // Shared collision list — atom labels avoid cluster pills and vice versa.
      const placed: { x: number; y: number; w: number; h: number }[] = [];
      function collides(rect: { x: number; y: number; w: number; h: number }, pad: number) {
        for (const p of placed) {
          if (
            rect.x - pad < p.x + p.w &&
            rect.x + rect.w + pad > p.x &&
            rect.y - pad < p.y + p.h &&
            rect.y + rect.h + pad > p.y
          ) return true;
        }
        return false;
      }

      // === Cluster labels (highest priority — placed first) ===
      // Skip in interactive preview: clusters are computed over the full graph,
      // so showing them on a filtered subset would mislabel sparse regions.
      const clusterFontSize = 13;
      ctx.font = `600 ${clusterFontSize}px system-ui, -apple-system, sans-serif`;
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';

      const sortedClusters = isInteractivePreview || accountNetworkMode
        ? []
        : [...data!.clusters].sort((a, b) => b.atom_count - a.atom_count);
      const maxClusterLabels = Math.max(4, Math.floor((width * height) / 40000));
      const clusterPad = 24;
      let clusterCount = 0;

      for (const cluster of sortedClusters) {
        if (clusterCount >= maxClusterLabels) break;

        // Compute centroid from actual current node positions
        let cx = 0, cy = 0, count = 0;
        for (const atomId of cluster.atom_ids) {
          if (!graph!.hasNode(atomId)) continue;
          cx += graph!.getNodeAttribute(atomId, 'x') as number;
          cy += graph!.getNodeAttribute(atomId, 'y') as number;
          count++;
        }
        if (count === 0) continue;
        cx /= count;
        cy /= count;
        const pos = sigma!.graphToViewport({ x: cx, y: cy });

        const labelY = pos.y - 20;
        const metrics = ctx.measureText(cluster.label);
        const pillW = metrics.width + 16;
        const pillH = clusterFontSize + 8;
        const rect = {
          x: pos.x - pillW / 2,
          y: labelY - pillH / 2,
          w: pillW,
          h: pillH,
        };

        if (collides(rect, clusterPad)) continue;
        placed.push(rect);
        clusterCount++;

        ctx.fillStyle = t.labelBg;
        ctx.beginPath();
        ctx.roundRect(rect.x, rect.y, pillW, pillH, pillH / 2);
        ctx.fill();
        ctx.strokeStyle = t.labelBorder;
        ctx.lineWidth = 1;
        ctx.stroke();

        ctx.fillStyle = t.labelColor;
        ctx.fillText(cluster.label, pos.x, labelY);
      }

      // Portraits and school marks sit above Sigma's colored circles. The
      // circles remain the pointer targets, and the image pass follows the
      // same category and hover emphasis as the node reducer.
      if (accountNetworkMode) {
        graph!.forEachNode((id, attrs) => {
          const image = nodeImages.get(id);
          if (!image) return;
          const category = (attrs as { categoryKey?: CategoryKey }).categoryKey;
          const display = sigma!.getNodeDisplayData(id);
          if (!display || (category !== 'executive' && category !== 'alumni')) return;
          const pos = sigma!.graphToViewport({ x: attrs.x as number, y: attrs.y as number });
          const radius = sigma!.scaleSize(display.size);
          if (pos.x < -radius || pos.x > width + radius || pos.y < -radius || pos.y > height + radius) return;
          const selected = selectedCategoriesRef.current;
          const isActive = id === hoveredNodeRef.current || id === pinnedNodeRef.current;
          const isNeighbor = !!hoveredNodeRef.current && !!neighborsRef.current.get(hoveredNodeRef.current)?.has(id)
            || !!pinnedNodeRef.current && !!neighborsRef.current.get(pinnedNodeRef.current)?.has(id);
          let opacity = 1;
          if (selected.size && !selected.has(category) && !isActive) {
            opacity = 0.22;
          } else if (!selected.size && (hoveredNodeRef.current || pinnedNodeRef.current) && !isActive && !isNeighbor) {
            opacity = 1 - 0.7 * (pinnedNodeRef.current ? 1 : hoverAnimRef.current);
          }
          ctx.save();
          ctx.globalAlpha = opacity;
          if (category === 'executive') {
            const innerRadius = Math.max(2, radius - 2);
            ctx.beginPath();
            ctx.arc(pos.x, pos.y, innerRadius, 0, Math.PI * 2);
            ctx.clip();
            const side = Math.min(image.naturalWidth, image.naturalHeight);
            ctx.drawImage(image, (image.naturalWidth - side) / 2, (image.naturalHeight - side) / 2, side, side,
              pos.x - innerRadius, pos.y - innerRadius, innerRadius * 2, innerRadius * 2);
          } else {
            const side = Math.max(4, radius * 1.65);
            ctx.fillStyle = '#fff';
            ctx.beginPath();
            ctx.roundRect(pos.x - side / 2, pos.y - side / 2, side, side, Math.min(5, side * 0.2));
            ctx.fill();
            const scale = Math.min(side / image.naturalWidth, side / image.naturalHeight);
            const imageWidth = image.naturalWidth * scale;
            const imageHeight = image.naturalHeight * scale;
            ctx.drawImage(image, pos.x - imageWidth / 2, pos.y - imageHeight / 2, imageWidth, imageHeight);
          }
          ctx.restore();
        });
      }

      // === Atom labels (collision-checked against everything already placed) ===
      // Interactive preview (briefing mini-canvas) keeps only a couple of
      // labels at rest — top-connected nodes win, the rest stay quiet until
      // the user hovers them. Pin state is unused here because preview mode
      // never pins.
      const atomFontSize = 12;
      ctx.font = `${atomFontSize}px system-ui, -apple-system, sans-serif`;
      ctx.textBaseline = 'middle';

      const tagFilter = selectedTagRef.current;
      // When a node is pinned, restrict labels to the pinned node + its
      // neighbors so the highlighted subgraph is the only thing named.
      const pinnedId = pinnedNodeRef.current;
      const pinnedNeighbors = pinnedId ? neighborsRef.current.get(pinnedId) : null;
      const selectedCategories = selectedCategoriesRef.current;
      const minRenderedSize = 4;
      const atomLabelPad = isInteractivePreview ? 40 : 20;
      const maxAtomLabels = isInteractivePreview ? 2 : Infinity;

      type Cand = { vx: number; vy: number; rsize: number; label: string };
      const candidates: Cand[] = [];
      graph!.forEachNode((id, attrs) => {
        if (!selectedCategories.size && pinnedId && id !== pinnedId && !pinnedNeighbors?.has(id)) return;
        if (selectedCategories.size) {
          const categoryKey = (attrs as { categoryKey?: CategoryKey }).categoryKey;
          if (!categoryKey || !selectedCategories.has(categoryKey)
            && categoryKey !== 'account' && id !== hoveredNodeRef.current && id !== pinnedId) return;
        }
        if (tagFilter) {
          const tagIds = (attrs as any).tagIds as string[] | undefined;
          if (!tagIds?.includes(tagFilter)) return;
        }
        const rsize = sigma!.scaleSize(sigma!.getNodeDisplayData(id)?.size ?? (attrs.size as number));
        if (rsize < minRenderedSize) return;
        const pos = sigma!.graphToViewport({ x: attrs.x as number, y: attrs.y as number });
        // Cull off-screen — generous horizontal margin so labels near the edge still render
        if (pos.x < -200 || pos.x > width + 50 || pos.y < -30 || pos.y > height + 30) return;
        const label = (attrs.label as string) || '';
        if (!label) return;
        candidates.push({ vx: pos.x, vy: pos.y, rsize, label });
      });
      // Largest (most-connected) nodes win label slots in dense regions
      candidates.sort((a, b) => b.rsize - a.rsize);

      // Labels render onto the bounded label canvas, so a label whose right
      // edge would land past `width` gets visually cut off. Flip to the left
      // side of the node when there's more room there — common on the small
      // briefing mini-canvas where nodes near the right edge would otherwise
      // lose their titles.
      ctx.fillStyle = t.nodeLabelColor;
      const edgePad = 2;
      let drawnAtomLabels = 0;
      for (const c of candidates) {
        if (drawnAtomLabels >= maxAtomLabels) break;
        const tw = ctx.measureText(c.label).width;
        const ly = c.vy;
        const rightX = c.vx + c.rsize + 4;
        const leftX = c.vx - c.rsize - 4;
        const rightFits = rightX + tw <= width - edgePad;
        const leftFits = leftX - tw >= edgePad;
        const onLeft = !rightFits && leftFits;
        const rect = onLeft
          ? { x: leftX - tw, y: ly - atomFontSize / 2, w: tw, h: atomFontSize }
          : { x: rightX, y: ly - atomFontSize / 2, w: tw, h: atomFontSize };
        if (collides(rect, atomLabelPad)) continue;
        placed.push(rect);
        if (onLeft) {
          ctx.textAlign = 'right';
          ctx.fillText(c.label, leftX, ly);
        } else {
          ctx.textAlign = 'left';
          ctx.fillText(c.label, rightX, ly);
        }
        drawnAtomLabels++;
      }

      // === Pinned-node ring (persists while a popover is open) ===
      if (pinnedId && graph!.hasNode(pinnedId)) {
        const pAttrs = graph!.getNodeAttributes(pinnedId);
        const pPos = sigma!.graphToViewport({ x: pAttrs.x as number, y: pAttrs.y as number });
        const pSize = sigma!.scaleSize(sigma!.getNodeDisplayData(pinnedId)?.size ?? (pAttrs.size as number));
        ctx.beginPath();
        ctx.arc(pPos.x, pPos.y, pSize + 3, 0, Math.PI * 2);
        ctx.strokeStyle = 'rgba(255, 255, 255, 0.75)';
        ctx.lineWidth = 2;
        ctx.stroke();

        // Keep the popover anchored to the node as the camera pans/zooms.
        // The popover ignores these updates after the user manually drags it,
        // so this doesn't fight with intentional repositioning.
        const cRect = container!.getBoundingClientRect();
        const aSize = pSize;
        const newAnchor = {
          top: cRect.top + pPos.y - aSize,
          left: cRect.left + pPos.x - aSize,
          bottom: cRect.top + pPos.y + aSize,
          width: aSize * 2,
        };
        setPreviewAnchorRect(prev => {
          if (
            prev &&
            Math.abs(prev.top - newAnchor.top) < 0.5 &&
            Math.abs(prev.left - newAnchor.left) < 0.5
          ) return prev;
          return newAnchor;
        });
      }

      // === Hover ring (drawn on canvas; the pill itself is a DOM element
      // so it can spill past the panel edges instead of getting clipped). ===
      // Suppress the pill/ring for the pinned node — its outline already marks it.
      const hoveredId = hoveredNodeRef.current;
      const hAnim = hoverAnimRef.current;
      const pill = hoverPillRef.current;
      if (hoveredId && hoveredId !== pinnedId && hAnim > 0.01 && graph!.hasNode(hoveredId)) {
        const hAttrs = graph!.getNodeAttributes(hoveredId);
        const hPos = sigma!.graphToViewport({ x: hAttrs.x as number, y: hAttrs.y as number });
        const hSize = sigma!.scaleSize(sigma!.getNodeDisplayData(hoveredId)?.size ?? (hAttrs.size as number));
        const hLabel = ((hAttrs as any).fullLabel as string) || (hAttrs.label as string) || '';

        ctx.globalAlpha = hAnim;
        ctx.beginPath();
        ctx.arc(hPos.x, hPos.y, hSize + 2, 0, Math.PI * 2);
        ctx.strokeStyle = 'rgba(255, 255, 255, 0.35)';
        ctx.lineWidth = 1.5;
        ctx.stroke();
        ctx.globalAlpha = 1;

        if (pill) {
          if (pill.textContent !== hLabel) pill.textContent = hLabel;
          // Measure the rendered DOM width so we can decide which side has
          // more room. The pill uses the full untruncated title, so it can
          // be wider than half the panel; we still pick whichever side has
          // more room, but the pill freely spills outside if needed.
          pill.style.visibility = 'hidden';
          pill.style.display = 'block';
          pill.style.left = '0px';
          pill.style.top = '0px';
          const pillW = pill.offsetWidth;
          const pillH = pill.offsetHeight;
          const rightRoom = width - (hPos.x + hSize + 4);
          const leftRoom = hPos.x - hSize - 4;
          const onLeft = leftRoom > rightRoom && pillW > rightRoom;
          const px = onLeft ? hPos.x - hSize - 4 - pillW : hPos.x + hSize + 4;
          const py = hPos.y - pillH / 2;
          pill.style.left = `${px}px`;
          pill.style.top = `${py}px`;
          pill.style.opacity = String(hAnim);
          pill.style.visibility = 'visible';
        }
      } else if (pill && pill.style.display !== 'none') {
        pill.style.display = 'none';
      }
    }

    sigma.on('afterRender', drawLabels);
    const initialLabelsRaf = requestAnimationFrame(drawLabels);

    // Lock the bounding box to the final layout so Sigma doesn't
    // recompute normalization as nodes move from center outward
    let xMin = Infinity, xMax = -Infinity, yMin = Infinity, yMax = -Infinity;
    for (const pos of Object.values(targetPositions)) {
      if (pos.x < xMin) xMin = pos.x;
      if (pos.x > xMax) xMax = pos.x;
      if (pos.y < yMin) yMin = pos.y;
      if (pos.y > yMax) yMax = pos.y;
    }
    sigma.setCustomBBox({ x: [xMin, xMax], y: [yMin, yMax] });

    // Animate nodes outward from center + fade edges in.
    // Preview mode skips the animation and snaps to final state so the thumbnail
    // shows the real layout immediately on mount. The main view also skips the
    // animation when a pendingCamera or pendingFocusAtomId is set — i.e. the
    // user came from clicking the dashboard preview, and we want to land on the
    // chosen framing/atom without the layout-builds-up flourish.
    let cancelledAnim = false;
    let layoutFinished = false;
    const manuallyPositionedNodes = new Set<string>();
    const pendingCamera = !isPreview ? useCanvasStore.getState().pendingCamera : null;
    const pendingFocus = !isPreview ? useCanvasStore.getState().pendingFocusAtomId : null;
    if (isPreview || pendingCamera || pendingFocus) {
      for (const [id, target] of Object.entries(targetPositions)) {
        if (!graph.hasNode(id)) continue;
        graph.setNodeAttribute(id, 'x', target.x);
        graph.setNodeAttribute(id, 'y', target.y);
      }
      edgeAnimProgress.current = 1;
      layoutFinished = true;
      if (pendingCamera) {
        sigma.getCamera().setState(pendingCamera);
        useCanvasStore.getState().setPendingCamera(null);
      }
      sigma.refresh();
    } else {
      const animStart = performance.now();
      function animateTick(now: number) {
        if (cancelledAnim) return;
        const elapsed = now - animStart;

        // Node positions: 0 → target over 2s, cubic ease-out
        const nt = Math.min(1, elapsed / 2000);
        const ne = 1 - (1 - nt) ** 3;
        for (const [id, target] of Object.entries(targetPositions)) {
          if (!graph.hasNode(id) || manuallyPositionedNodes.has(id)) continue;
          graph.setNodeAttribute(id, 'x', target.x * ne);
          graph.setNodeAttribute(id, 'y', target.y * ne);
        }

        // Edge fade: 0 → 1 over 2.5s, ease-in
        const et = Math.min(1, elapsed / 2500);
        edgeAnimProgress.current = et * et;

        // setNodeAttribute triggers a render but not a reducer re-run —
        // the edgeReducer reads edgeAnimProgress.current, so force a refresh
        // each tick so edge color/size track the animation.
        sigma.refresh();

        if (nt < 1 || et < 1) {
          requestAnimationFrame(animateTick);
        } else {
          layoutFinished = true;
        }
      }
      requestAnimationFrame(animateTick);
    }
    const cancelAnim = () => { cancelledAnim = true; };
    const cameraTargetForNodes = (ids: string[], focused = false) => {
      const points = ids.map(id => sigma.getNodeDisplayData(id))
        .filter((point): point is NonNullable<typeof point> => !!point)
        .map(point => ({ x: point.x, y: point.y }));
      const ratioOneCamera = { x: 0.5, y: 0.5, ratio: 1, angle: 0 };
      const origin = sigma.framedGraphToViewport({ x: 0, y: 0 }, { cameraState: ratioOneCamera });
      const unit = sigma.framedGraphToViewport({ x: 1, y: 0 }, { cameraState: ratioOneCamera });
      const pixelsPerUnit = Math.max(1, Math.abs(unit.x - origin.x));
      const dimensions = sigma.getDimensions();
      // Leave room for the controls and labels to the right of selected nodes.
      const reservedRight = accountNetworkMode && dimensions.width >= 640
        ? Math.min(focused ? 536 : 336, dimensions.width * 0.45) : 0;
      // On narrow screens, keep the chart's first row below the compact controls.
      const reservedTop = accountNetworkMode && dimensions.width < 640
        ? Math.min(180, dimensions.height * 0.26) : 0;
      const target = categoryCameraTarget(points, {
        width: dimensions.width - reservedRight,
        height: dimensions.height - reservedTop,
      }, pixelsPerUnit);
      if (!target) return null;
      return {
        ...target,
        x: target.x + reservedRight * target.ratio / (2 * pixelsPerUnit),
        y: target.y + reservedTop * target.ratio / (2 * pixelsPerUnit),
        angle: 0,
      };
    };
    let appliedMode: AccountLayoutMode = 'freeform';
    refitLayoutRef.current = () => {
      if (appliedMode === 'freeform') return;
      sigma.refresh();
      const fullView = cameraTargetForNodes(graph.nodes());
      const selected = selectedCategoriesRef.current;
      if (selected.size) {
        if (fullView) savedCategoryCameraRef.current = fullView;
        const ids = graph.nodes().filter(id => {
          const category = graph.getNodeAttribute(id, 'categoryKey') as CategoryKey | undefined;
          return !!category && selected.has(category);
        });
        const target = cameraTargetForNodes(ids, true);
        if (target) void sigma.getCamera().animate(target, { duration: 250 });
      } else if (fullView) {
        void sigma.getCamera().animate(fullView, { duration: 250 });
      }
    };
    applyLayoutRef.current = (mode, documentedEdges, force = false) => {
      if (!accountNetworkMode || !accountRootId || !force && mode === 'freeform' && appliedMode === 'freeform') return;
      cancelAnim();
      layoutFinished = true;
      edgeAnimProgress.current = 1;
      appliedMode = mode;
      const chartPositions = mode === 'freeform' ? null : computeAccountChartLayout(
        atoms.map(atom => ({
          id: atom.atom_id,
          title: atom.title,
          category: accountNetworkCategory(atom.source_url)!.key,
        })),
        documentedEdges,
        accountRootId,
        mode,
      );
      const overrides = mode === 'freeform' ? {} : layoutSettingsRef.current.positions[mode];
      const nextPositions = mode === 'freeform'
        ? Object.fromEntries(atoms.map(atom => [atom.atom_id,
          savedPositionsRef.current[atom.atom_id] ?? targetPositions[atom.atom_id]]))
        : { ...chartPositions, ...Object.fromEntries(Object.entries(overrides)
          .filter(([id]) => graph.hasNode(id))) };
      setGraphEdges(mode === 'freeform' ? edges : documentedEdges, mode !== 'freeform');
      xMin = Infinity; xMax = -Infinity; yMin = Infinity; yMax = -Infinity;
      for (const [id, point] of Object.entries(nextPositions)) {
        if (!graph.hasNode(id)) continue;
        graph.mergeNodeAttributes(id, point);
        xMin = Math.min(xMin, point.x); xMax = Math.max(xMax, point.x);
        yMin = Math.min(yMin, point.y); yMax = Math.max(yMax, point.y);
      }
      if (xMin === xMax) { xMin -= 1; xMax += 1; }
      if (yMin === yMax) { yMin -= 1; yMax += 1; }
      sigma.setCustomBBox({ x: [xMin, xMax], y: [yMin, yMax] });
      sigma.refresh();
      const fullView = cameraTargetForNodes(graph.nodes());
      if (!fullView) return;
      if (selectedCategoriesRef.current.size) {
        savedCategoryCameraRef.current = fullView;
        focusCategoriesRef.current(selectedCategoriesRef.current);
      } else {
        void sigma.getCamera().animate(fullView, { duration: 350 });
      }
    };
    focusCategoriesRef.current = (selected) => {
      if (!layoutFinished) {
        cancelAnim();
        for (const [id, target] of Object.entries(targetPositions)) {
          if (!graph.hasNode(id) || manuallyPositionedNodes.has(id)) continue;
          graph.mergeNodeAttributes(id, target);
        }
        edgeAnimProgress.current = 1;
        layoutFinished = true;
      }
      sigma.refresh();
      const ids: string[] = [];
      graph.forEachNode((id, attrs) => {
        const key = (attrs as { categoryKey?: CategoryKey }).categoryKey;
        if (!key || !selected.has(key)) return;
        ids.push(id);
      });
      const target = cameraTargetForNodes(ids, true);
      if (target) void sigma.getCamera().animate(target, { duration: 600 });
    };

    // Helper to show atom preview popover at a node's screen position.
    // Pinning is handled by clickNode / focusAtom so this stays purely positional.
    const showAtomPreview = (atomId: string) => {
      if (!graph.hasNode(atomId) || !sigma) return;
      const nodeAttrs = graph.getNodeAttributes(atomId);
      const viewportPos = sigma.graphToViewport({ x: nodeAttrs.x as number, y: nodeAttrs.y as number });
      const containerRect = container!.getBoundingClientRect();
      const nodeSize = sigma.scaleSize(sigma.getNodeDisplayData(atomId)?.size ?? ((nodeAttrs.size as number) || 4));
      const screenX = containerRect.left + viewportPos.x;
      const screenY = containerRect.top + viewportPos.y;
      setPreviewAnchorRect({
        top: screenY - nodeSize,
        left: screenX - nodeSize,
        bottom: screenY + nodeSize,
        width: nodeSize * 2,
      });
      setPreviewAtomId(atomId);
    };

    // Pin a node so hover emphasis stays on it while its popover is open.
    const pinNode = (atomId: string) => {
      if (!graph.hasNode(atomId)) return;
      pinnedNodeRef.current = atomId;
      hoveredNodeRef.current = atomId;
      hoverTargetRef.current = 1;
      startHoverAnimRef.current();
    };

    // Build a controller both modes use. Main mode registers it in the global
    // `controller` slot (driven by the chat agent). Preview mode registers it
    // in the separate `previewController` slot so dashboard widgets can drive
    // thumbnail focus without touching the chat agent's target.
    const graphToCamera = (gx: number, gy: number) => ({
      x: (gx - xMin) / (xMax - xMin || 1),
      y: (gy - yMin) / (yMax - yMin || 1),
    });

    const controller = {
      zoomToCluster: (clusterLabel: string) => {
        const cluster = data.clusters.find(
          (c) => c.label.toLowerCase() === clusterLabel.toLowerCase()
        );
        if (!cluster || !graph || !sigma) return;
        let cx = 0, cy = 0, count = 0;
        for (const atomId of cluster.atom_ids) {
          if (!graph.hasNode(atomId)) continue;
          cx += graph.getNodeAttribute(atomId, 'x') as number;
          cy += graph.getNodeAttribute(atomId, 'y') as number;
          count++;
        }
        if (count === 0) return;
        cx /= count;
        cy /= count;
        const cam = graphToCamera(cx, cy);
        sigma.getCamera().animate({ x: cam.x, y: cam.y, ratio: 0.3 }, { duration: 800 });
      },
      focusAtom: (atomId: string) => {
        if (!graph.hasNode(atomId) || !sigma) return;
        const gx = graph.getNodeAttribute(atomId, 'x') as number;
        const gy = graph.getNodeAttribute(atomId, 'y') as number;
        const cam = graphToCamera(gx, gy);
        // ratio 0.15 was visually too tight when arriving from the briefing
        // mini-canvas — the atom filled the screen with no surrounding
        // context. 0.35 keeps the focused atom prominent while showing the
        // neighborhood it sits in.
        sigma.getCamera().animate({ x: cam.x, y: cam.y, ratio: 0.35 }, { duration: 600 });
        // Main view shows the popover after the camera settles; preview stays quiet.
        if (!isPreview) setTimeout(() => { pinNode(atomId); showAtomPreview(atomId); }, 650);
      },
      getCameraState: () => {
        if (!sigma) return null;
        const s = sigma.getCamera().getState();
        return { x: s.x, y: s.y, ratio: s.ratio, angle: s.angle };
      },
    };

    // Hover + click handlers run in main mode AND in interactive preview.
    // In interactive preview, clicks bubble up to the parent (which opens the
    // main canvas focused on the chosen atom) instead of opening a popover.
    const isInteractive = !isPreview || isInteractivePreview;
    let cleanupDrag = () => {};
    let cleanupHover = () => {};
    if (isInteractive) {
      // Hover animation: exponential ease toward target (0 or 1).
      // Loop stops itself when target is reached, so idle cost is zero.
      let hoverRaf: number | null = null;
      const tickHover = () => {
        const diff = hoverTargetRef.current - hoverAnimRef.current;
        if (Math.abs(diff) < 0.005) {
          hoverAnimRef.current = hoverTargetRef.current;
          if (hoverTargetRef.current === 0) hoveredNodeRef.current = null;
          sigma.refresh();
          hoverRaf = null;
          return;
        }
        hoverAnimRef.current += diff * 0.22; // ~10–12 frames to close
        sigma.refresh();
        hoverRaf = requestAnimationFrame(tickHover);
      };
      const startHoverAnim = () => {
        if (hoverRaf !== null) return;
        hoverRaf = requestAnimationFrame(tickHover);
      };
      startHoverAnimRef.current = startHoverAnim;
      cleanupHover = () => {
        if (hoverRaf !== null) cancelAnimationFrame(hoverRaf);
        startHoverAnimRef.current = () => {};
      };
      let draggedNode: string | null = null;
      let dragMoved = false;
      let lastDragEnd = 0;
      let dragStart = { x: 0, y: 0 };
      let dragOffset = { x: 0, y: 0 };
      const finishDrag = () => {
        if (!draggedNode) return;
        if (dragMoved && graph.hasNode(draggedNode)) {
          const node = draggedNode;
          const point = {
            x: graph.getNodeAttribute(node, 'x') as number,
            y: graph.getNodeAttribute(node, 'y') as number,
          };
          lastDragEnd = Date.now();
          const mode = layoutModeRef.current;
          if (mode === 'freeform' || !accountRootId) {
            savedPositionsRef.current[node] = point;
            void saveAtomPositions([{ atom_id: node, ...point }]).catch(() => {
              toast.error('Could not save node position');
            });
          } else {
            const next: AccountLayoutSettings = {
              ...layoutSettingsRef.current,
              positions: {
                ...layoutSettingsRef.current.positions,
                [mode]: { ...layoutSettingsRef.current.positions[mode], [node]: point },
              },
            };
            layoutSettingsRef.current = next;
            setLayoutSettings(next);
            if (!writeAccountLayoutSettings(localStorage, activeDbId ?? 'default', accountRootId, next)) {
              toast.error('Could not save this layout on this device');
            }
          }
        }
        draggedNode = null;
        sigma.getCamera().enable();
        container.style.cursor = hoveredNodeRef.current ? 'grab' : '';
      };
      if (!isPreview) {
        sigma.on('downNode', ({ node, event }) => {
          if (event.original instanceof MouseEvent && event.original.button !== 0) return;
          draggedNode = node;
          dragMoved = false;
          dragStart = { x: event.x, y: event.y };
          sigma.getCamera().disable();
          container.style.cursor = 'grabbing';
        });
        sigma.on('moveBody', ({ event, preventSigmaDefault }) => {
          if (!draggedNode) return;
          preventSigmaDefault();
          if (!dragMoved && Math.hypot(event.x - dragStart.x, event.y - dragStart.y) < 4) return;
          const point = sigma.viewportToGraph({ x: event.x, y: event.y });
          if (!dragMoved) {
            dragMoved = true;
            manuallyPositionedNodes.add(draggedNode);
            dragOffset = {
              x: (graph.getNodeAttribute(draggedNode, 'x') as number) - point.x,
              y: (graph.getNodeAttribute(draggedNode, 'y') as number) - point.y,
            };
          }
          graph.mergeNodeAttributes(draggedNode, {
            x: point.x + dragOffset.x,
            y: point.y + dragOffset.y,
          });
          sigma.refresh();
        });
        sigma.on('upNode', finishDrag);
        sigma.on('upStage', finishDrag);
        window.addEventListener('mouseup', finishDrag);
        window.addEventListener('touchend', finishDrag);
        cleanupDrag = () => {
          window.removeEventListener('mouseup', finishDrag);
          window.removeEventListener('touchend', finishDrag);
          sigma.getCamera().enable();
        };
      }
      sigma.on('clickNode', ({ node }) => {
        if (Date.now() - lastDragEnd < 400) return;
        if (isInteractivePreview) {
          onPreviewNodeClickRef.current?.(node);
          return;
        }
        // Main view: pin the clicked node so emphasis persists while the popover is open.
        pinNode(node);
        showAtomPreview(node);
      });
      sigma.on('enterNode', ({ node }) => {
        hoveredNodeRef.current = node;
        hoverTargetRef.current = 1;
        startHoverAnim();
        if (!isPreview && !draggedNode) container.style.cursor = 'grab';
      });
      sigma.on('leaveNode', () => {
        hoverTargetRef.current = 0;
        startHoverAnim();
        if (!isPreview && !draggedNode) container.style.cursor = '';
      });
    }

    if (isPreview) {
      useCanvasStore.getState().registerPreviewController(controller);
    } else {
      const { registerController, setCanvasData } = useCanvasStore.getState();
      setCanvasData(data);
      registerController(controller);

      // If a node was clicked in the briefing mini-canvas, the main view
      // mounts here. Land directly on that atom (camera + popover) so the
      // user sees it selected without any post-mount panning.
      const pendingFocusId = useCanvasStore.getState().pendingFocusAtomId;
      if (pendingFocusId && graph.hasNode(pendingFocusId)) {
        useCanvasStore.getState().setPendingFocusAtomId(null);
        controller.focusAtom(pendingFocusId);
      } else if (pendingFocusId) {
        // The atom isn't in the graph (e.g. no embedding yet) — drop the
        // request so a stale focus doesn't fire later.
        useCanvasStore.getState().setPendingFocusAtomId(null);
      }
    }

    return () => {
      cancelledMedia = true;
      cancelAnim();
      focusCategoriesRef.current = () => {};
      applyLayoutRef.current = () => {};
      refitLayoutRef.current = () => {};
      cancelAnimationFrame(initialLabelsRaf);
      cleanupHover();
      cleanupDrag();
      const store = useCanvasStore.getState();
      if (isPreview) {
        store.unregisterPreviewController();
      } else {
        store.unregisterController();
      }
      sigma.kill();
      labelCanvas.remove();
      sigmaRef.current = null;
      graphRef.current = null;
    };
  }, [data, isPreview, filterKey, accountTagFilterId, accountSlug, activeDbId]); // theme is handled below

  // A verified entry path is the one deliberate motion on this view. It is
  // drawn over the current graph positions, so dragging and camera moves keep
  // the signal attached to the actual notes it connects.
  useEffect(() => {
    if (!accountTagFilterId || !entryPoints.length) return;
    const container = containerRef.current;
    const sigma = sigmaRef.current;
    const graph = graphRef.current;
    if (!container || !sigma || !graph) return;
    const canvas = document.createElement('canvas');
    canvas.setAttribute('aria-hidden', 'true');
    canvas.dataset.testid = 'entry-path-overlay';
    canvas.style.position = 'absolute';
    canvas.style.inset = '0';
    canvas.style.pointerEvents = 'none';
    canvas.style.zIndex = '9';
    container.appendChild(canvas);
    const ctx = canvas.getContext('2d');
    if (!ctx) { canvas.remove(); return; }
    const moving = animateEntryPaths && !reduceMotion;
    let frameId: number | null = null;
    let lastFrame = 0;

    const draw = (now: number) => {
      const width = container.clientWidth;
      const height = container.clientHeight;
      if (!width || !height) return;
      const ratio = window.devicePixelRatio || 1;
      if (canvas.width !== Math.round(width * ratio) || canvas.height !== Math.round(height * ratio)) {
        canvas.width = Math.round(width * ratio);
        canvas.height = Math.round(height * ratio);
        canvas.style.width = `${width}px`;
        canvas.style.height = `${height}px`;
      }
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, width, height);
      entryPoints.forEach((entry, index) => {
        if (!graph.hasNode(entry.source) || !graph.hasNode(entry.target)) return;
        const selected = selectedCategoriesRef.current;
        const sourceCategory = graph.getNodeAttribute(entry.source, 'categoryKey') as CategoryKey | undefined;
        const targetCategory = graph.getNodeAttribute(entry.target, 'categoryKey') as CategoryKey | undefined;
        const categoryRelevant = !selected.size
          || (!!sourceCategory && selected.has(sourceCategory))
          || (!!targetCategory && selected.has(targetCategory));
        const activeNode = pinnedNodeRef.current || hoveredNodeRef.current;
        const relevant = categoryRelevant && (!activeNode || activeNode === entry.source || activeNode === entry.target);
        const source = sigma.graphToViewport(graph.getNodeAttributes(entry.source) as { x: number; y: number });
        const target = sigma.graphToViewport(graph.getNodeAttributes(entry.target) as { x: number; y: number });
        const dx = target.x - source.x;
        const dy = target.y - source.y;
        const length = Math.hypot(dx, dy);
        if (length < 1) return;
        ctx.beginPath();
        ctx.moveTo(source.x, source.y);
        ctx.lineTo(target.x, target.y);
        ctx.strokeStyle = relevant ? 'rgba(250, 204, 21, 0.8)' : 'rgba(250, 204, 21, 0.14)';
        ctx.lineWidth = 1.6;
        ctx.stroke();
        if (!moving || !relevant) return;
        const phase = (now + index * 670) % 3400;
        if (phase > 1100) return;
        const progress = 1 - (1 - phase / 1100) ** 3;
        const steps = Math.max(4, Math.ceil(length / 26));
        const normalX = -dy / length;
        const normalY = dx / length;
        ctx.beginPath();
        for (let step = 0; step <= steps; step++) {
          const along = progress * step / steps;
          const offset = step === 0 || step === steps ? 0 : (step % 2 ? 1 : -1) * 4 * Math.sin(Math.PI * along);
          const x = source.x + dx * along + normalX * offset;
          const y = source.y + dy * along + normalY * offset;
          if (step === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.strokeStyle = '#fef08a';
        ctx.lineWidth = 2.4;
        ctx.stroke();
      });
    };
    const tick = (now: number) => {
      if (now - lastFrame >= 1000 / 30 && document.visibilityState === 'visible') {
        draw(now);
        lastFrame = now;
      }
      frameId = requestAnimationFrame(tick);
    };
    const drawStatic = () => draw(performance.now());
    if (moving) frameId = requestAnimationFrame(tick);
    else {
      sigma.on('afterRender', drawStatic);
      frameId = requestAnimationFrame(drawStatic);
    }
    return () => {
      if (frameId !== null) cancelAnimationFrame(frameId);
      if (!moving) sigma.off('afterRender', drawStatic);
      canvas.remove();
    };
  }, [accountTagFilterId, entryPoints, animateEntryPaths, reduceMotion, data]);

  // Update colors when theme changes (without recreating graph)
  useEffect(() => {
    const graph = graphRef.current;
    const sigma = sigmaRef.current;
    if (!graph || !sigma || !graphDataRef.current) return;

    const { edgeCounts, maxEdges } = graphDataRef.current;

    // Update node colors
    graph.forEachNode((node, attrs) => {
      const connectivity = (edgeCounts.get(node) || 0) / maxEdges;
      const category = accountTagFilterId
        ? accountNetworkCategory((attrs as { sourceUrl?: string }).sourceUrl)
        : null;
      graph.setNodeAttribute(node, 'color', category?.color ?? nodeColor(theme, connectivity, (attrs as any).clusterIndex));
    });

    // Atom label color comes from themeRef inside drawLabels — just trigger a refresh.
    // Edges update via edgeReducer (also reads themeRef.current).
    sigma.refresh();
  }, [theme, accountTagFilterId]);

  // Refresh when selected tag changes (nodeReducer reads selectedTagRef)
  useEffect(() => {
    sigmaRef.current?.refresh();
  }, [selectedTagId]);

  // Continuously refresh sigma during chat sidebar transition so the graph
  // resizes smoothly. Expanding the pane collapses this column to zero width
  // without unmounting it, so the same catch-up applies.
  const chatSidebarOpen = useUIStore(s => s.chatSidebarOpen);
  const chatSidebarExpanded = useUIStore(s => s.chatSidebarExpanded);
  useEffect(() => {
    const start = performance.now();
    let raf: number;
    function tick(now: number) {
      sigmaRef.current?.refresh();
      if (now - start < 350) {
        raf = requestAnimationFrame(tick);
      }
    }
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [chatSidebarOpen, chatSidebarExpanded]);

  // Subscribe to canvas action events from the chat agent.
  // Preview instances don't own the controller and shouldn't react to chat actions.
  useEffect(() => {
    if (isPreview) return;
    const transport = getTransport();
    const unsub = transport.subscribe<{ conversation_id: string; action: string; params: Record<string, string> }>(
      'chat-canvas-action',
      (payload) => {
        const ctrl = useCanvasStore.getState().controller;
        if (!ctrl) return;
        if (payload.action === 'zoom_to_cluster') {
          ctrl.zoomToCluster(payload.params.cluster_label);
        } else if (payload.action === 'focus_atom') {
          ctrl.focusAtom(payload.params.atom_id);
        }
      }
    );
    return () => unsub();
  }, [isPreview]);

  // Animate edge threshold changes
  const thresholdAnimRef = useRef<number | null>(null);
  useEffect(() => {
    const sigma = sigmaRef.current;
    if (!sigma) {
      edgeThresholdRef.current = edgeThreshold;
      return;
    }
    const from = edgeThresholdRef.current;
    const to = edgeThreshold;
    if (Math.abs(from - to) < 0.001) return;

    if (thresholdAnimRef.current) cancelAnimationFrame(thresholdAnimRef.current);
    const start = performance.now();
    const duration = 400;
    function tick(now: number) {
      const t = Math.min(1, (now - start) / duration);
      const eased = 1 - (1 - t) ** 2; // ease out quad
      edgeThresholdRef.current = from + (to - from) * eased;
      sigma!.refresh();
      if (t < 1) {
        thresholdAnimRef.current = requestAnimationFrame(tick);
      } else {
        thresholdAnimRef.current = null;
      }
    }
    thresholdAnimRef.current = requestAnimationFrame(tick);
  }, [edgeThreshold]);

  return (
    <div className="flex flex-col h-full w-full relative">
      <div
        className="flex-1 relative overflow-hidden"
        style={{ backgroundColor: isPreview ? 'var(--color-bg-main)' : theme.background }}
      >
        {isLoading && (
          <div className="absolute inset-0 flex items-center justify-center z-10">
            <div className="flex items-center gap-2 text-[var(--color-text-secondary)]">
              <Loader2 className={`animate-spin ${isPreview ? 'h-4 w-4' : 'h-5 w-5'}`} strokeWidth={2} />
              {!isPreview && <span className="text-sm">Computing layout...</span>}
            </div>
          </div>
        )}

        {error && (
          <div className="absolute inset-0 flex items-center justify-center">
            <div className="text-center text-[var(--color-text-secondary)]">
              {isPreview ? (
                <p className="text-xs">Canvas unavailable</p>
              ) : (
                <>
                  <p className="text-lg mb-2">Error loading canvas</p>
                  <p className="text-sm">{error}</p>
                </>
              )}
            </div>
          </div>
        )}

        {!isLoading && data && data.atoms.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center">
            <div className="text-center text-[var(--color-text-secondary)]">
              {isPreview ? (
                <p className="text-xs">No atoms with embeddings yet</p>
              ) : (
                <>
                  <p className="text-lg mb-2">No atoms with embeddings</p>
                  <p className="text-sm">Create some atoms and wait for embeddings to generate</p>
                </>
              )}
            </div>
          </div>
        )}

        <div
          ref={containerRef}
          data-testid={isPreview ? 'canvas-preview' : 'canvas-main'}
          className={`w-full h-full ${isPreview && !isInteractivePreview ? 'pointer-events-none' : ''}`}
          style={isPreview ? undefined : { minHeight: 200 }}
        />

        {/* Click-through overlay only for the static thumbnail preview.
            Interactive preview handles its own clicks via clickNode. */}
        {isPreview && !isInteractivePreview && (
          <button
            type="button"
            onClick={onPreviewClick}
            className="absolute inset-0 z-20 cursor-pointer bg-transparent hover:bg-white/[0.03] transition-colors"
            aria-label="Open canvas view"
          />
        )}

        {!isPreview && accountTagFilterId && !isLoading && (
          <aside
            ref={controlsRef}
            aria-label="Account network controls"
            className="absolute right-4 top-4 z-20 max-h-[calc(100%-32px)] w-[min(320px,calc(100%-32px))] overflow-y-auto rounded-lg border border-white/15 bg-[#242428] max-sm:right-2 max-sm:top-2 max-sm:w-[min(320px,calc(100%-16px))]"
          >
            <div className="px-3 pb-3 pt-3">
              <div className="mb-2 text-sm font-semibold text-white">Account network</div>
              <div role="group" aria-label="Account network layout" className="grid grid-cols-3 gap-1 rounded-md bg-black/20 p-1">
                {([
                  ['freeform', 'Freeform'],
                  ['top-down', 'Top-down'],
                  ['left-to-right', 'Left-to-right'],
                ] as const).map(([mode, label]) => (
                  <button
                    key={mode}
                    type="button"
                    data-testid={`account-layout-${mode}`}
                    aria-pressed={effectiveLayoutMode === mode}
                    disabled={mode !== 'freeform' && recordedEdges?.rootId !== accountRootId}
                    onClick={() => changeLayout(mode)}
                    className={`min-h-8 rounded px-1 text-[11px] font-medium transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-white/80 disabled:cursor-not-allowed disabled:opacity-35 ${
                      effectiveLayoutMode === mode ? 'bg-white/15 text-white' : 'text-white/70 hover:bg-white/10 hover:text-white'
                    }`}
                  >
                    {label}
                  </button>
                ))}
              </div>
              {effectiveLayoutMode !== 'freeform' && (
                <button type="button" onClick={resetChartLayout}
                  className="mt-2 text-xs text-white/70 underline decoration-white/25 underline-offset-2 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-white/80">
                  Reset layout
                </button>
              )}
              {linksLoading && <p className="mt-2 text-xs text-white/65" role="status">Loading recorded links…</p>}
              {linksError && (
                <div className="mt-2 flex items-center gap-2 text-xs text-white/75" role="alert">
                  <span>Recorded links unavailable</span>
                  <button type="button" onClick={() => setLinkRetry(count => count + 1)}
                    className="rounded border border-white/25 px-2 py-1 text-white hover:bg-white/10 focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
                    Retry
                  </button>
                </div>
              )}
            </div>
            <div role="tablist" aria-label="Account network panels" className="grid grid-cols-2 border-y border-white/10 px-2">
              <button ref={categoryTabRef} id={`${controlTabsId}-categories-tab`} type="button" role="tab"
                aria-selected={controlTab === 'categories'} aria-controls={`${controlTabsId}-categories-panel`}
                tabIndex={controlTab === 'categories' ? 0 : -1}
                onClick={() => selectControlTab('categories')} onKeyDown={handleControlTabKeyDown}
                className={`min-h-10 border-b-2 px-1 text-xs font-medium focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-white/80 ${controlTab === 'categories' ? 'border-white/80 text-white' : 'border-transparent text-white/60 hover:text-white'}`}>
                Categories
              </button>
              <button ref={researchTabRef} id={`${controlTabsId}-research-tab`} type="button" role="tab"
                aria-selected={controlTab === 'research'} aria-controls={`${controlTabsId}-research-panel`}
                tabIndex={controlTab === 'research' ? 0 : -1}
                onClick={() => selectControlTab('research')} onKeyDown={handleControlTabKeyDown}
                className={`min-h-10 border-b-2 px-1 text-xs font-medium focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-white/80 ${controlTab === 'research' ? 'border-white/80 text-white' : 'border-transparent text-white/60 hover:text-white'}`}>
                Research checks <span className="ml-1 font-normal tabular-nums text-white/50">{researchLoading ? '…' : researchChecks.length}</span>
              </button>
            </div>
            <div id={`${controlTabsId}-categories-panel`} role="tabpanel" aria-labelledby={`${controlTabsId}-categories-tab`}
              hidden={controlTab !== 'categories'} tabIndex={0}>
              <div className="border-t border-white/10 px-3 pb-3 pt-2">
                <div className="grid grid-cols-2 gap-x-2 gap-y-2">
                  {ACCOUNT_NETWORK_CATEGORIES.map(category => {
                    const selected = selectedCategories.has(category.key);
                    const available = (categoryCounts.get(category.key) ?? 0) > 0;
                    const enriched = available && !researchLoading && !researchError && enrichedCategories.has(category.key);
                    const enrichedDescriptionId = `${controlTabsId}-${category.key}-enriched`;
                    return (
                      <button
                        key={category.key}
                        type="button"
                        aria-pressed={selected}
                        aria-describedby={enriched ? enrichedDescriptionId : undefined}
                        disabled={!available}
                        onClick={() => toggleAccountCategory(category.key)}
                        title={available ? undefined : 'No nodes in this category'}
                        className={`flex min-h-8 items-center gap-1.5 rounded-md border px-1.5 py-1 text-left text-xs transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-white/80 ${
                          selected
                            ? 'border-white/30 bg-white/15 text-white'
                            : 'border-transparent text-white/75 hover:bg-white/10'
                        } ${available ? 'cursor-pointer' : 'cursor-not-allowed opacity-35'}`}
                      >
                        {category.key === 'account' ? (
                          <svg aria-hidden="true" viewBox="0 0 24 24" className="h-3.5 w-3.5 shrink-0">
                            <polygon points="12,1.5 14.9,8.4 22.4,9 16.7,13.8 18.5,21 12,17.1 5.5,21 7.3,13.8 1.6,9 9.1,8.4" fill={category.color} />
                          </svg>
                        ) : (
                          <span
                            aria-hidden="true"
                            className="h-2.5 w-2.5 shrink-0 rounded-full"
                            style={{ backgroundColor: category.color }}
                          />
                        )}
                        <span>{category.label}</span>
                        {enriched && <>
                          <Check aria-hidden="true" className="ml-auto h-4 w-4 shrink-0 text-emerald-400" strokeWidth={2.5} />
                          <span id={enrichedDescriptionId} className="sr-only">Enrichment recorded</span>
                        </>}
                      </button>
                    );
                  })}
                </div>
                <p className="mt-2 text-[11px] text-white/60">Green check = recorded enrichment or sourced findings</p>
                <p className="mt-3 border-t border-white/10 pt-2 text-xs text-white/60">
                  {effectiveLayoutMode === 'freeform'
                    ? 'Freeform lines show similar notes. Dragged positions save to Atomic.'
                    : 'Chart lines are links recorded in notes, not reporting relationships. Dragged positions save on this device.'}
                </p>
              </div>
            </div>
            <div id={`${controlTabsId}-research-panel`} role="tabpanel" aria-labelledby={`${controlTabsId}-research-tab`}
              hidden={controlTab !== 'research'} tabIndex={0}>
              <AccountResearchPanel
                atoms={accountAtoms}
                checks={researchChecks}
                entryPoints={entryPointRows}
                linkState={recordedEdges?.rootId === accountRootId ? 'ready' : linksError ? 'error' : 'loading'}
                loading={researchLoading || !researchError && researchNotes?.rootId !== accountRootId}
                error={researchError}
                onRetry={() => setResearchRetry(count => count + 1)}
                onRetryLinks={() => setLinkRetry(count => count + 1)}
                onRefresh={() => setCanvasReload(count => count + 1)}
                onFocus={atomId => useCanvasStore.getState().controller?.focusAtom(atomId)}
                animatePaths={animateEntryPaths && !reduceMotion}
                motionAvailable={!reduceMotion}
                onAnimatePathsChange={setAnimateEntryPaths}
              />
            </div>
          </aside>
        )}

        {/* Theme picker + edge slider — main view only */}
        {!isPreview && !isLoading && data && data.atoms.length > 0 && (
          <div className="absolute bottom-4 left-4 z-20 flex flex-col gap-2">
            <div className="flex items-center gap-1.5">
              <button
                onClick={() => setThemePickerOpen(!themePickerOpen)}
                title="Change theme"
                className="w-6 h-6 rounded-full border border-white/20 hover:border-white/40 transition-all flex-shrink-0"
                style={{
                  background: `linear-gradient(135deg, rgb(${theme.nodeMin.join(',')}), rgb(${theme.nodeMax.join(',')}))`,
                }}
              />
              <div
                className={`flex gap-1.5 overflow-hidden transition-all duration-200 ${
                  themePickerOpen ? 'max-w-[200px] opacity-100' : 'max-w-0 opacity-0'
                }`}
              >
                {CANVAS_THEMES.filter(t => t.id !== theme.id).map((t) => (
                  <button
                    key={t.id}
                    onClick={() => { setTheme(t); setThemePickerOpen(false); }}
                    title={t.name}
                    className="w-5 h-5 rounded-full border border-white/15 hover:border-white/40 transition-all flex-shrink-0"
                    style={{
                      background: `linear-gradient(135deg, rgb(${t.nodeMin.join(',')}), rgb(${t.nodeMax.join(',')}))`,
                    }}
                  />
                ))}
              </div>
            </div>
            {(!accountTagFilterId || effectiveLayoutMode === 'freeform') && <div className="flex items-center gap-1.5">
              <input
                type="range"
                min={0}
                max={100}
                value={(1 - edgeThreshold) * 100}
                onChange={(e) => setEdgeThreshold(1 - Number(e.target.value) / 100)}
                className="w-20 h-1 appearance-none bg-white/10 rounded-full cursor-pointer
                  [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:w-2.5 [&::-webkit-slider-thumb]:h-2.5
                  [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-white/60"
                title={`Edges: ${Math.round((1 - edgeThreshold) * 100)}%`}
              />
              <span className="text-[9px] text-white/30">
                {Math.round((1 - edgeThreshold) * 100)}%
              </span>
            </div>}
          </div>
        )}

        {/* Atom preview popover — main view only */}
        {!isPreview && previewAtomId && previewAnchorRect && (
          <AtomPreviewPopover
            atomId={previewAtomId}
            anchorRect={previewAnchorRect}
            onClose={closePreview}
            onViewAtom={(atomId, opts) => {
              closePreview();
              openReader(atomId, undefined, opts);
            }}
          />
        )}

      </div>

      {/* Hover pill — lives outside the overflow-hidden clipping div so a
          long title can extend past the panel edges instead of getting cut
          off by the canvas's drawing surface. drawLabels imperatively sets
          its position, content, and opacity each frame. */}
      <div
        ref={hoverPillRef}
        data-testid="canvas-hover-label"
        className="absolute pointer-events-none whitespace-nowrap z-30
                   px-1.5 py-1 rounded text-[13px] leading-none
                   bg-[rgba(20,20,20,0.92)] text-[#e8e8e8]
                   border border-white/10"
        style={{ display: 'none' }}
      />
    </div>
  );
}
