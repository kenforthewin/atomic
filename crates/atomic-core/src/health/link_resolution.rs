//! Generic internal-link extraction and resolution.
//!
//! Handles two link formats found in Obsidian-imported atoms:
//!
//! 1. **Markdown links** — `[text](relative/path.md)` or `[text](../other.md)`
//! 2. **Wikilinks** — `[[File Name]]` or `[[File Name|Display Text]]`
//!
//! A link is "internal" when its href contains no URI scheme (`://`) and is
//! not a bare fragment (`#anchor`).  Absolute paths starting with `/` are
//! also excluded — those are server-rooted URLs, not vault-relative paths.
//!
//! Resolution maps each link to a candidate `source_url` (or a set of LIKE
//! patterns for wikilinks) so callers can look the target up in the atom
//! table.

/// A single internal link found inside an atom's content.
#[derive(Debug, Clone)]
pub struct InternalLink {
    /// The original text in the content that needs to be replaced.
    /// For markdown: `[text](href)`.  For wikilinks: `[[target]]`.
    pub original: String,
    /// The raw href or wikilink target extracted from the original.
    pub href: String,
    /// Candidate absolute source URLs to try (exact lookup).
    /// Built from the current atom's source_url + relative path.
    pub candidate_source_urls: Vec<String>,
    /// For wikilinks: search the atoms table with
    /// `source_url LIKE '%/' || name || '.md'` across the vault.
    pub wikilink_name: Option<String>,
}

/// A resolved match: an `InternalLink` with its target atom identified.
#[derive(Debug, Clone)]
pub struct ResolvedLink {
    pub original: String,
    pub target_atom_id: String,
    /// Used as the replacement href: `atom://target_atom_id`
    pub replacement: String,
}

// ==================== Extraction ====================

/// Extract all internal links from `content`.
///
/// `source_url` is the current atom's source URL; it is used to resolve
/// relative paths.  Pass `None` for atoms without a known source.
pub fn extract_internal_links(
    content: &str,
    source_url: Option<&str>,
) -> Vec<InternalLink> {
    let mut links = Vec::new();
    links.extend(extract_markdown_links(content, source_url));
    links.extend(extract_wikilinks(content, source_url));
    links
}

fn extract_markdown_links(content: &str, source_url: Option<&str>) -> Vec<InternalLink> {
    let mut links = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;

    while i + 1 < bytes.len() {
        // Scan for `](`
        if bytes[i] != b']' || bytes[i + 1] != b'(' {
            i += 1;
            continue;
        }

        // Find the matching `)`
        let href_start = i + 2;
        let mut j = href_start;
        let mut depth = 1i32;

        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                j += 1;
            }
        }

        if depth != 0 {
            i += 1;
            continue;
        }

        let raw_href = match std::str::from_utf8(&bytes[href_start..j]) {
            Ok(s) => s,
            Err(_) => {
                i = j + 1;
                continue;
            }
        };

        // Strip optional inline title: `path.md "Title"` → `path.md`
        let href = raw_href
            .trim()
            .split('"')
            .next()
            .unwrap_or("")
            .split('\'')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if is_internal_href(&href) && looks_like_document(&href) {
            // Find the opening `[` to capture display text + full match
            let (original, display) = scan_back_for_display_text(content, i, j);

            let candidate_source_urls = match source_url {
                Some(su) => build_href_candidates(&href, su),
                None => vec![],
            };

            links.push(InternalLink {
                original,
                href: href.clone(),
                candidate_source_urls,
                wikilink_name: None,
            });

            // Skip `[display](href)` — display text already consumed above
            let _ = display;
        }

        i = j + 1;
    }

    links
}

fn extract_wikilinks(content: &str, source_url: Option<&str>) -> Vec<InternalLink> {
    let mut links = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;

    while i + 1 < bytes.len() {
        if bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }

        let start = i + 2;
        let mut j = start;

        while j + 1 < bytes.len() && !(bytes[j] == b']' && bytes[j + 1] == b']') {
            j += 1;
        }

        if j + 1 >= bytes.len() {
            i += 1;
            continue;
        }

        let inner = match std::str::from_utf8(&bytes[start..j]) {
            Ok(s) => s,
            Err(_) => {
                i = j + 2;
                continue;
            }
        };

        // `[[target|display text]]` — keep only the target
        let target = inner.split('|').next().unwrap_or("").trim().to_string();

        if !target.is_empty() {
            let original = format!("[[{}]]", inner);

            let candidate_source_urls = match source_url {
                Some(su) => build_wikilink_exact_candidates(&target, su),
                None => vec![],
            };

            links.push(InternalLink {
                original,
                href: target.clone(),
                candidate_source_urls,
                wikilink_name: Some(target),
            });
        }

        i = j + 2;
    }

    links
}

// ==================== Predicates ====================

/// A link is internal when it has no URI scheme, is not a bare fragment,
/// and does not start with `/` (server-root absolute path).
fn is_internal_href(href: &str) -> bool {
    let h = href.trim();
    !h.is_empty()
        && !h.starts_with('#')
        && !h.starts_with('/')
        && !h.contains("://")
        && !h.starts_with("mailto:")
        && !h.starts_with("tel:")
}

/// The link looks like a document reference (not an image, anchor, etc.).
fn looks_like_document(href: &str) -> bool {
    let h = href.trim().to_lowercase();
    // Explicit markdown/text extensions
    if h.ends_with(".md") || h.ends_with(".txt") || h.ends_with(".org") {
        return true;
    }
    // Relative path operators
    if h.starts_with("./") || h.starts_with("../") {
        return true;
    }
    // No extension + contains path separator → likely a document path
    if !h.contains('.') && h.contains('/') {
        return true;
    }
    false
}

// ==================== URL resolution ====================

/// Extract the vault root from a source URL.
///
/// `obsidian://ar-playbook/some/path.md` → `obsidian://ar-playbook/`
pub fn vault_root(source_url: &str) -> Option<&str> {
    let scheme_end = source_url.find("://")?;
    let after_scheme = &source_url[scheme_end + 3..];
    let vault_sep = after_scheme.find('/')?;
    Some(&source_url[..scheme_end + 3 + vault_sep + 1])
}

/// Directory portion of a source URL (everything up to and including the
/// last `/`).
fn source_dir(source_url: &str) -> &str {
    if let Some(pos) = source_url.rfind('/') {
        &source_url[..pos + 1]
    } else {
        source_url
    }
}

/// Resolve a relative href against the current atom's source URL, returning
/// candidate source URL strings (with and without `.md`) to try.
fn build_href_candidates(href: &str, current_source_url: &str) -> Vec<String> {
    let href = href.trim();
    let Some(root) = vault_root(current_source_url) else {
        return vec![];
    };
    let dir = source_dir(current_source_url);

    let resolved = if let Some(rest) = href.strip_prefix("./") {
        format!("{}{}", dir, rest)
    } else if let Some(rest) = href.strip_prefix("../") {
        resolve_parent(dir, rest, root)
    } else {
        // Relative to vault root (Obsidian default for bare paths)
        format!("{}{}", root, href)
    };

    candidates_with_and_without_extension(&resolved)
}

fn resolve_parent(current_dir: &str, rest: &str, vault_root: &str) -> String {
    let dir = current_dir.trim_end_matches('/');
    let parent = dir
        .rfind('/')
        .map(|p| &dir[..p + 1])
        .unwrap_or(vault_root);
    if let Some(rest) = rest.strip_prefix("../") {
        resolve_parent(parent, rest, vault_root)
    } else {
        format!("{}{}", parent, rest)
    }
}

/// For a wikilink `[[Name]]`, build exact-URL candidates to try first.
/// Wikilinks resolve by filename anywhere in the vault, so we generate:
/// - `vault_root/Name.md`
/// - `vault_root/name.md` (lower-case stem)
/// - `vault_root/name-with-dashes.md` (slug variant)
///
/// The `find_atoms_by_wikilink_name` SQL fallback handles subdirectory
/// resolution when none of these exact hits land.
fn build_wikilink_exact_candidates(name: &str, current_source_url: &str) -> Vec<String> {
    let Some(root) = vault_root(current_source_url) else {
        return vec![];
    };
    let slug = name.to_lowercase().replace(' ', "-");
    let mut candidates = vec![
        format!("{}{}.md", root, name),
        format!("{}{}.md", root, name.to_lowercase()),
        format!("{}{}.md", root, slug),
    ];
    candidates.dedup();
    candidates
}

/// Return the URL itself plus a variant without the `.md` extension (and
/// vice-versa), so callers can match atoms stored either way.
fn candidates_with_and_without_extension(url: &str) -> Vec<String> {
    if let Some(stem) = url.strip_suffix(".md") {
        vec![url.to_string(), stem.to_string()]
    } else {
        vec![url.to_string(), format!("{}.md", url)]
    }
}

// ==================== Display-text extraction ====================

/// Scan backwards from the `]` at byte index `bracket_pos` to find the
/// Walk backwards from `bracket_pos` (the `]` before `(`) to find the opening
/// `[`, returning `(full_original_text, display_text)`.
///
/// `end_pos` is the position of the closing `)` in `content`, so the full
/// original span `[display](href…)` can be reconstructed.
fn scan_back_for_display_text(content: &str, bracket_pos: usize, end_pos: usize) -> (String, String) {
    let bytes = content.as_bytes();
    if bracket_pos == 0 {
        // `]` is at position 0 — no room for `[display]`, reconstruct from end_pos.
        let original = std::str::from_utf8(&bytes[..end_pos + 1])
            .unwrap_or("")
            .to_string();
        return (original, String::new());
    }

    // Walk backwards through the content to find the opening `[`
    let mut depth = 1usize;
    let mut k = bracket_pos.saturating_sub(1);
    loop {
        match bytes[k] {
            b']' => depth += 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        if k == 0 {
            break;
        }
        k -= 1;
    }

    // Full match spans from `[` at position `k` to the `)` at `end_pos`.
    let display = std::str::from_utf8(&bytes[k + 1..bracket_pos])
        .unwrap_or("")
        .to_string();
    let original = std::str::from_utf8(&bytes[k..end_pos + 1])
        .unwrap_or("")
        .to_string();
    (original, display)
}

// ==================== Replacement ====================

/// Apply resolved link replacements to `content`, returning the updated string.
///
/// Each replacement: `(original_text, new_href)` — the display text is
/// preserved; only the href portion is changed.
pub fn apply_link_replacements(content: &str, replacements: &[ResolvedLink]) -> String {
    let mut result = content.to_string();

    for resolved in replacements {
        // For markdown links: [text](old_href) → [text](atom://id)
        // For wikilinks:      [[Name]]         → [Name](atom://id)
        let original = &resolved.original;
        let new_href = &resolved.replacement;

        if original.starts_with("[[") {
            // Wikilink → markdown link with atom:// href
            let inner = &original[2..original.len() - 2];
            let display = inner.split('|').next().unwrap_or(inner).trim();
            let replacement = format!("[{}]({})", display, new_href);
            result = result.replacen(original.as_str(), &replacement, 1);
        } else if let (Some(open), Some(_close)) = (original.find("]("), original.rfind(')')) {
            // Markdown link → update only the href part
            let display = &original[1..open];
            let replacement = format!("[{}]({})", display, new_href);
            result = result.replacen(original.as_str(), &replacement, 1);
        }
    }

    result
}

/// Reconstruct the full original markdown link text `[display](href)` for a
/// given href and its position in `content`, so we can build `InternalLink.original`.
///
/// Called after extraction to fill in the `original` field that
/// `scan_back_for_display_text` could not complete.
pub fn build_original_text(display: &str, href: &str) -> String {
    format!("[{}]({})", display, href)
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: &str = "obsidian://ar-playbook/";
    const SOURCE: &str = "obsidian://ar-playbook/processes/deployment.md";

    #[test]
    fn test_relative_href_resolves_to_vault_root() {
        let candidates = build_href_candidates("processes/work-tracking.md", SOURCE);
        assert!(candidates.contains(&"obsidian://ar-playbook/processes/work-tracking.md".to_string()));
    }

    #[test]
    fn test_dotslash_href_resolves_relative_to_current_dir() {
        let candidates = build_href_candidates("./capacity-planning.md", SOURCE);
        assert!(candidates.contains(
            &"obsidian://ar-playbook/processes/capacity-planning.md".to_string()
        ));
    }

    #[test]
    fn test_parent_href_resolves_correctly() {
        let candidates = build_href_candidates("../docs/overview.md", SOURCE);
        assert!(candidates.contains(&"obsidian://ar-playbook/docs/overview.md".to_string()));
    }

    #[test]
    fn test_absolute_url_not_internal() {
        assert!(!is_internal_href("https://example.com/file.md"));
        assert!(!is_internal_href("http://example.com"));
        assert!(!is_internal_href("obsidian://vault/path.md"));
        assert!(!is_internal_href("atom://some-id"));
    }

    #[test]
    fn test_relative_path_is_internal() {
        assert!(is_internal_href("processes/work-tracking.md"));
        assert!(is_internal_href("./capacity.md"));
        assert!(is_internal_href("../docs/overview.md"));
    }

    #[test]
    fn test_fragment_not_internal() {
        assert!(!is_internal_href("#section-heading"));
        assert!(!is_internal_href(""));
    }

    #[test]
    fn test_extract_markdown_links() {
        let content = "See [Work Tracking](processes/work-tracking.md) and [Metrics](../docs/metrics.md).";
        let links = extract_internal_links(content, Some(SOURCE));
        assert_eq!(links.len(), 2);
        let hrefs: Vec<&str> = links.iter().map(|l| l.href.as_str()).collect();
        assert!(hrefs.contains(&"processes/work-tracking.md"));
        assert!(hrefs.contains(&"../docs/metrics.md"));
    }

    #[test]
    fn test_extract_wikilinks() {
        let content = "See [[Work Tracking]] and [[Metrics|Metrics Docs]].";
        let links = extract_internal_links(content, Some(SOURCE));
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].href, "Work Tracking");
        assert_eq!(links[1].href, "Metrics");
    }

    #[test]
    fn test_no_links_in_plain_text() {
        let content = "No links here. Just text.";
        let links = extract_internal_links(content, Some(SOURCE));
        assert!(links.is_empty());
    }

    #[test]
    fn test_absolute_links_ignored() {
        let content = "See [Confluence](https://atlassian.net/wiki/page) and [Source](obsidian://vault/file.md).";
        let links = extract_internal_links(content, Some(SOURCE));
        assert!(links.is_empty());
    }

    #[test]
    fn test_vault_root_extraction() {
        assert_eq!(
            vault_root("obsidian://ar-playbook/processes/deployment.md"),
            Some("obsidian://ar-playbook/")
        );
    }

    #[test]
    fn test_apply_markdown_replacement() {
        let content = "See [Work Tracking](processes/work-tracking.md).";
        let resolved = vec![ResolvedLink {
            original: "[Work Tracking](processes/work-tracking.md)".to_string(),
            target_atom_id: "abc123".to_string(),
            replacement: "atom://abc123".to_string(),
        }];
        let result = apply_link_replacements(content, &resolved);
        assert_eq!(result, "See [Work Tracking](atom://abc123).");
    }

    #[test]
    fn test_apply_wikilink_replacement() {
        let content = "See [[Work Tracking]] for details.";
        let resolved = vec![ResolvedLink {
            original: "[[Work Tracking]]".to_string(),
            target_atom_id: "abc123".to_string(),
            replacement: "atom://abc123".to_string(),
        }];
        let result = apply_link_replacements(content, &resolved);
        assert_eq!(result, "See [Work Tracking](atom://abc123) for details.");
    }
    #[test]
    fn test_markdown_link_original_is_populated() {
        // `original` must be the full `[text](href)` span so callers can use it
        // for replace operations.  Previously scan_back_for_display_text always
        // returned String::new() for `original`, causing `content: ""` to be
        // sent to the server and failing the relink validation.
        let content = "See [broken link](./missing.md) for details.";
        let links = extract_internal_links(content, Some("obsidian://vault/index.md"));
        assert_eq!(links.len(), 1, "one broken link extracted");
        assert_eq!(links[0].original, "[broken link](./missing.md)");
    }

    #[test]
    fn test_markdown_link_at_position_zero_has_original() {
        // Edge case: link at the very start of content.
        let content = "[start link](./page.md) is here.";
        let links = extract_internal_links(content, Some("obsidian://vault/index.md"));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].original, "[start link](./page.md)");
    }

}
