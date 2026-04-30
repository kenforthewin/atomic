import { useState, useRef, useMemo, useEffect, MouseEvent } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Pencil, Plus, Trash2, Inbox, Search } from 'lucide-react';
import { TagNode } from './TagNode';
import { ContextMenu } from '../ui/ContextMenu';
import { Modal } from '../ui/Modal';
import { Input } from '../ui/Input';
import { useTagsStore, TagWithCount } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { useAtomsStore } from '../../stores/atoms';

interface FlattenedTag {
  tag: TagWithCount;
  level: number;
  /** If set, this row is a "load more" sentinel for the given parent tag id */
  loadMoreParentId?: string;
  loadMoreRemaining?: number;
}

function flattenVisibleTags(
  tags: TagWithCount[],
  expandedTagIds: Record<string, boolean>,
  level: number = 0
): FlattenedTag[] {
  const result: FlattenedTag[] = [];
  for (const tag of tags) {
    result.push({ tag, level });
    if (tag.children.length > 0 && expandedTagIds[tag.id]) {
      const children = flattenVisibleTags(tag.children, expandedTagIds, level + 1);
      for (let i = 0; i < children.length; i++) {
        result.push(children[i]);
      }
      // Add "load more" sentinel if there are more children on the server
      if (tag.children.length < tag.children_total) {
        result.push({
          tag,
          level: level + 1,
          loadMoreParentId: tag.id,
          loadMoreRemaining: tag.children_total - tag.children.length,
        });
      }
    }
  }
  return result;
}

interface TagTreeProps {
  /** Called when the user clicks "Configure" in the empty-state notice. */
  onOpenTagSettings?: () => void;
}

export function TagTree({ onOpenTagSettings }: TagTreeProps = {}) {
  const tags = useTagsStore(s => s.tags);
  const isLoading = useTagsStore(s => s.isLoading);
  const createTag = useTagsStore(s => s.createTag);
  const updateTag = useTagsStore(s => s.updateTag);
  const deleteTag = useTagsStore(s => s.deleteTag);
  const fetchMoreTagChildren = useTagsStore(s => s.fetchMoreTagChildren);
  const selectedTagId = useUIStore(s => s.selectedTagId);
  const setSelectedTag = useUIStore(s => s.setSelectedTag);
  const openSearchPalette = useUIStore(s => s.openSearchPalette);
  const expandedTagIds = useUIStore(s => s.expandedTagIds);
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchAtomsByTag = useAtomsStore(s => s.fetchAtomsByTag);

  const scrollContainerRef = useRef<HTMLDivElement>(null);

  const flatTags = useMemo(
    () => flattenVisibleTags(tags, expandedTagIds),
    [tags, expandedTagIds]
  );

  const tagIndexMap = useMemo(() => {
    const map = new Map<string, number>();
    for (let i = 0; i < flatTags.length; i++) {
      map.set(flatTags[i].tag.id, i);
    }
    return map;
  }, [flatTags]);

  // Always keep a ref to the latest tagIndexMap so the scroll effect can read
  // current indices without subscribing to map changes. The ref is updated on
  // every render (outside any effect), so it is always fresh when the effect fires.
  const tagIndexMapRef = useRef(tagIndexMap);
  tagIndexMapRef.current = tagIndexMap;
  const virtualizer = useVirtualizer({
    count: flatTags.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: () => 32,
    overscan: 20,
  });

  // Remeasure virtualizer when tree structure changes (expansion/collapse)
  useEffect(() => {
    virtualizer.measure();
  }, [flatTags, virtualizer]);

  // Scroll to selected tag only when the selection changes. We intentionally
  // read tagIndexMapRef.current (not tagIndexMap directly) so that accordion
  // expand/collapse — which rebuilds tagIndexMap but does not change
  // selectedTagId — does NOT trigger a scroll back to the active tag.
  useEffect(() => {
    if (selectedTagId) {
      const index = tagIndexMapRef.current.get(selectedTagId);
      if (index !== undefined) {
        virtualizer.scrollToIndex(index, { align: 'center', behavior: 'smooth' });
      }
    }
  }, [selectedTagId, virtualizer]);

  const [contextMenu, setContextMenu] = useState<{
    position: { x: number; y: number } | null;
    tag: TagWithCount | null;
  }>({ position: null, tag: null });

  const [renameModal, setRenameModal] = useState<{
    isOpen: boolean;
    tag: TagWithCount | null;
    name: string;
  }>({ isOpen: false, tag: null, name: '' });

  const [deleteModal, setDeleteModal] = useState<{
    isOpen: boolean;
    tag: TagWithCount | null;
    recursive: boolean;
  }>({ isOpen: false, tag: null, recursive: false });

  const [newTagModal, setNewTagModal] = useState<{
    isOpen: boolean;
    parentId: string | null;
    name: string;
  }>({ isOpen: false, parentId: null, name: '' });

  const handleSelectTag = async (tagId: string | null) => {
    setSelectedTag(tagId);
    if (tagId) {
      await fetchAtomsByTag(tagId);
    } else {
      await fetchAtoms();
    }
  };

  const handleContextMenu = (e: MouseEvent, tag: TagWithCount) => {
    setContextMenu({
      position: { x: e.clientX, y: e.clientY },
      tag,
    });
  };

  const handleRename = async () => {
    if (renameModal.tag && renameModal.name.trim()) {
      await updateTag(renameModal.tag.id, renameModal.name.trim(), renameModal.tag.parent_id || undefined);
      setRenameModal({ isOpen: false, tag: null, name: '' });
    }
  };

  const handleDelete = async () => {
    if (deleteModal.tag) {
      await deleteTag(deleteModal.tag.id, deleteModal.recursive);
      if (selectedTagId === deleteModal.tag.id) {
        handleSelectTag(null);
      }
      setDeleteModal({ isOpen: false, tag: null, recursive: false });
    }
  };

  const handleCreateTag = async () => {
    if (newTagModal.name.trim()) {
      await createTag(newTagModal.name.trim(), newTagModal.parentId || undefined);
      setNewTagModal({ isOpen: false, parentId: null, name: '' });
    }
  };

  const contextMenuItems = contextMenu.tag
    ? [
        {
          label: 'Rename',
          onClick: () => {
            setRenameModal({
              isOpen: true,
              tag: contextMenu.tag,
              name: contextMenu.tag?.name || '',
            });
          },
          icon: (
            <Pencil className="w-4 h-4" strokeWidth={2} />
          ),
        },
        {
          label: 'Add Child Tag',
          onClick: () => {
            setNewTagModal({
              isOpen: true,
              parentId: contextMenu.tag?.id || null,
              name: '',
            });
          },
          icon: (
            <Plus className="w-4 h-4" strokeWidth={2} />
          ),
        },
        {
          label: 'Delete',
          onClick: () => {
            setDeleteModal({
              isOpen: true,
              tag: contextMenu.tag,
              recursive: false,
            });
          },
          danger: true,
          icon: (
            <Trash2 className="w-4 h-4" strokeWidth={2} />
          ),
        },
      ]
    : [];

  return (
    <div className="flex flex-col h-full">
      {/* All Atoms option */}
      <div
        className={`flex items-center gap-2 px-3 py-2 cursor-pointer transition-colors shrink-0 ${
          selectedTagId === null
            ? 'bg-[var(--color-accent)]/20 text-[var(--color-text-primary)]'
            : 'text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-card)] hover:text-[var(--color-text-primary)]'
        }`}
        onClick={() => handleSelectTag(null)}
      >
        <Inbox className="w-4 h-4" strokeWidth={2} />
        <span className="flex-1 text-sm font-medium">All Atoms</span>
      </div>

      {/* Tags header with search button */}
      <div className="flex items-center justify-between px-3 py-2 shrink-0">
        <span className="text-xs font-semibold text-[var(--color-text-tertiary)] uppercase tracking-wider">
          Tags
        </span>
        <button
          onClick={(e) => {
            e.stopPropagation();
            openSearchPalette('#');
          }}
          className="p-1 rounded hover:bg-[var(--color-bg-hover)] text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors"
          title="Search tags"
        >
          <Search className="w-4 h-4" strokeWidth={2} />
        </button>
      </div>

      {/* Virtualized tag list */}
      <div ref={scrollContainerRef} className="flex-1 overflow-y-auto scrollbar-hidden">
        {tags.length === 0 && isLoading ? (
          <div className="flex flex-col gap-1 px-2 py-1">
            {Array.from({ length: 5 }, (_, i) => (
              <div key={i} className="flex items-center gap-2 px-2 py-1.5">
                <div className="w-4 h-4 rounded bg-[var(--color-bg-card)] animate-pulse" />
                <div className="h-3.5 rounded bg-[var(--color-bg-card)] animate-pulse" style={{ width: `${60 + i * 15}px` }} />
              </div>
            ))}
          </div>
        ) : tags.length === 0 ? (
          <div className="px-3 py-4 space-y-3">
            <p className="text-xs text-[var(--color-text-secondary)] leading-relaxed">
              No tag categories are configured for this database, so auto-tagging is off. Set up categories to let Atomic tag your atoms automatically.
            </p>
            {onOpenTagSettings && (
              <button
                onClick={onOpenTagSettings}
                className="w-full px-3 py-1.5 text-xs font-medium rounded bg-[var(--color-accent)] text-white hover:bg-[var(--color-accent-hover)] transition-colors"
              >
                Configure categories
              </button>
            )}
          </div>
        ) : (
          <div
            style={{ height: `${virtualizer.getTotalSize()}px`, position: 'relative' }}
          >
            {virtualizer.getVirtualItems().map((virtualItem) => {
              const item = flatTags[virtualItem.index];
              const { tag, level, loadMoreParentId, loadMoreRemaining } = item;
              return (
                <div
                  key={loadMoreParentId ? `load-more-${loadMoreParentId}` : tag.id}
                  style={{
                    position: 'absolute',
                    top: 0,
                    left: 0,
                    right: 0,
                    height: `${virtualItem.size}px`,
                    transform: `translateY(${virtualItem.start}px)`,
                  }}
                >
                  {loadMoreParentId ? (
                    <div
                      className="flex items-center gap-1 px-2 py-1.5 cursor-pointer text-[var(--color-text-tertiary)] hover:text-[var(--color-accent-light)] transition-colors text-xs"
                      style={{ paddingLeft: `${8 + level * 16}px` }}
                      onClick={() => fetchMoreTagChildren(loadMoreParentId)}
                    >
                      <span className="w-4" />
                      <span>{loadMoreRemaining?.toLocaleString()} more tags...</span>
                    </div>
                  ) : (
                    <TagNode
                      tag={tag}
                      level={level}
                      selectedTagId={selectedTagId}
                      onSelect={handleSelectTag}
                      onContextMenu={handleContextMenu}
                    />
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>

      {/* New Tag button */}
      <div className="px-3 py-2 border-t border-[var(--color-border)] shrink-0">
        <button
          className="w-full flex items-center justify-start gap-1.5 px-2 py-1.5 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded-md transition-colors"
          onClick={() => setNewTagModal({ isOpen: true, parentId: null, name: '' })}
        >
          <Plus className="w-3.5 h-3.5" strokeWidth={2} />
          New Tag
        </button>
      </div>

      {/* Context Menu */}
      <ContextMenu
        items={contextMenuItems}
        position={contextMenu.position}
        onClose={() => setContextMenu({ position: null, tag: null })}
      />

      {/* Rename Modal */}
      <Modal
        isOpen={renameModal.isOpen}
        onClose={() => setRenameModal({ isOpen: false, tag: null, name: '' })}
        title="Rename Tag"
        confirmLabel="Rename"
        onConfirm={handleRename}
      >
        <Input
          label="Tag Name"
          value={renameModal.name}
          onChange={(e) => setRenameModal((prev) => ({ ...prev, name: e.target.value }))}
          placeholder="Enter tag name"
          autoFocus
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              handleRename();
            }
          }}
        />
      </Modal>

      {/* Delete Confirmation Modal */}
      <Modal
        isOpen={deleteModal.isOpen}
        onClose={() => setDeleteModal({ isOpen: false, tag: null, recursive: false })}
        title="Delete Tag"
        confirmLabel="Delete"
        confirmVariant="danger"
        onConfirm={handleDelete}
      >
        <p>
          Are you sure you want to delete the tag "{deleteModal.tag?.name}"?
        </p>
        {deleteModal.tag && deleteModal.tag.children.length > 0 && (
          <div className="mt-3 space-y-2">
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="radio"
                name="deleteMode"
                checked={!deleteModal.recursive}
                onChange={() => setDeleteModal(s => ({ ...s, recursive: false }))}
                className="accent-[var(--color-accent)]"
              />
              <span className="text-sm text-[var(--color-text-primary)]">
                Delete only this tag (children move to root)
              </span>
            </label>
            <label className="flex items-center gap-2 cursor-pointer">
              <input
                type="radio"
                name="deleteMode"
                checked={deleteModal.recursive}
                onChange={() => setDeleteModal(s => ({ ...s, recursive: true }))}
                className="accent-[var(--color-accent)]"
              />
              <span className="text-sm text-[var(--color-text-primary)]">
                Delete this tag and all {deleteModal.tag.children_total} descendant{deleteModal.tag.children_total !== 1 ? 's' : ''}
              </span>
            </label>
          </div>
        )}
      </Modal>

      {/* New Tag Modal */}
      <Modal
        isOpen={newTagModal.isOpen}
        onClose={() => setNewTagModal({ isOpen: false, parentId: null, name: '' })}
        title={newTagModal.parentId ? 'New Child Tag' : 'New Tag'}
        confirmLabel="Create"
        onConfirm={handleCreateTag}
      >
        <Input
          label="Tag Name"
          value={newTagModal.name}
          onChange={(e) => setNewTagModal((prev) => ({ ...prev, name: e.target.value }))}
          placeholder="Enter tag name"
          autoFocus
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              handleCreateTag();
            }
          }}
        />
      </Modal>
    </div>
  );
}
