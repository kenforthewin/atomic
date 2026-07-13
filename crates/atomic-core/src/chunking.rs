//! Markdown-aware content chunking for embedding generation
//!
//! Chunking strategy:
//! - Uses pulldown-cmark to parse markdown structure
//! - Code blocks are kept intact for syntax integrity, up to the maximum
//!   chunk size; oversized ones are split at line boundaries and re-fenced
//! - Headers create natural chunk boundaries
//! - Target chunk size: ~3200 chars (~800 tokens)
//! - Minimum chunk size: ~240 chars (~60 tokens)
//! - Maximum chunk size: ~4000 chars (~1000 tokens)
//! - Overlap: ~320 chars (~80 tokens) from next chunk appended to each chunk
//!
//! Uses character counts for fast size estimation (~4 chars/token).
//! Exact token counts (via `count_tokens`) are available for callers that
//! need precision (e.g. LLM context budgets).

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::sync::LazyLock;
use tiktoken_rs::{cl100k_base, CoreBPE};

/// Approximate characters per token (~4 for English text).
const CHARS_PER_TOKEN: usize = 4;

/// Configuration constants for chunking (in characters)
const TARGET_CHUNK_CHARS: usize = 800 * CHARS_PER_TOKEN;
const OVERLAP_CHARS: usize = 80 * CHARS_PER_TOKEN;
const MIN_CHUNK_CHARS: usize = 60 * CHARS_PER_TOKEN;
const MAX_CHUNK_CHARS: usize = 1000 * CHARS_PER_TOKEN;

/// Lazily initialized tokenizer (loaded once, reused for all operations)
static BPE: LazyLock<CoreBPE> =
    LazyLock::new(|| cl100k_base().expect("Failed to load tiktoken encoding"));

/// Count tokens using tiktoken's cl100k_base encoding (used by OpenAI embedding models).
/// This is the precise, slower path — used for LLM context budgets, NOT for chunking.
pub fn count_tokens(text: &str) -> usize {
    BPE.encode_with_special_tokens(text).len()
}

/// Get the first N characters of text (char-boundary safe)
fn get_first_n_chars(text: &str, n: usize) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    // Find a char boundary at or before n
    let mut end = n;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Types of markdown blocks
#[derive(Debug, Clone, PartialEq)]
enum BlockType {
    CodeBlock,
    Header,
    List,
    Paragraph,
}

/// A parsed markdown block
#[derive(Debug, Clone)]
struct MarkdownBlock {
    block_type: BlockType,
    content: String,
}

/// Parse content into markdown blocks using pulldown-cmark
fn parse_markdown_blocks(content: &str) -> Vec<MarkdownBlock> {
    let parser = Parser::new(content).into_offset_iter();
    let mut blocks = Vec::new();

    // Track block-level ranges: (block_type, start_offset, end_offset)
    let mut current_block: Option<(BlockType, usize, usize)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                // If inside a container block (e.g. list), just extend it
                // rather than splitting the container.
                if matches!(current_block, Some((BlockType::List, ..))) {
                    if let Some((_, _, ref mut end)) = current_block {
                        *end = range.end;
                    }
                } else {
                    // Flush any pending block
                    if let Some((bt, start, end)) = current_block.take() {
                        let text = content[start..end].trim().to_string();
                        if !text.is_empty() {
                            blocks.push(MarkdownBlock {
                                block_type: bt,
                                content: text,
                            });
                        }
                    }
                    current_block = Some((BlockType::CodeBlock, range.start, range.end));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                match current_block {
                    Some((BlockType::List, _, ref mut end)) => {
                        // Code block ended inside list — just extend the list range
                        *end = range.end;
                    }
                    Some((BlockType::CodeBlock, start, _)) => {
                        current_block = None;
                        let text = content[start..range.end].trim().to_string();
                        if !text.is_empty() {
                            blocks.push(MarkdownBlock {
                                block_type: BlockType::CodeBlock,
                                content: text,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::Start(Tag::Heading { .. }) => {
                // Flush any pending block
                if let Some((bt, start, end)) = current_block.take() {
                    let text = content[start..end].trim().to_string();
                    if !text.is_empty() {
                        blocks.push(MarkdownBlock {
                            block_type: bt,
                            content: text,
                        });
                    }
                }
                current_block = Some((BlockType::Header, range.start, range.end));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((BlockType::Header, start, _)) = current_block.take() {
                    let text = content[start..range.end].trim().to_string();
                    if !text.is_empty() {
                        blocks.push(MarkdownBlock {
                            block_type: BlockType::Header,
                            content: text,
                        });
                    }
                }
            }
            Event::Start(Tag::List(_)) => {
                // Flush any pending block
                if let Some((bt, start, end)) = current_block.take() {
                    let text = content[start..end].trim().to_string();
                    if !text.is_empty() {
                        blocks.push(MarkdownBlock {
                            block_type: bt,
                            content: text,
                        });
                    }
                }
                current_block = Some((BlockType::List, range.start, range.end));
            }
            Event::End(TagEnd::List(_)) => {
                if let Some((BlockType::List, start, _)) = current_block.take() {
                    let text = content[start..range.end].trim().to_string();
                    if !text.is_empty() {
                        blocks.push(MarkdownBlock {
                            block_type: BlockType::List,
                            content: text,
                        });
                    }
                }
            }
            Event::Start(Tag::Paragraph) => {
                // Only start a new paragraph if we're not inside another block
                if current_block.is_none() {
                    current_block = Some((BlockType::Paragraph, range.start, range.end));
                } else {
                    // Extend current block (e.g. paragraph inside list item)
                    if let Some((_, _, ref mut end)) = current_block {
                        *end = range.end;
                    }
                }
            }
            Event::End(TagEnd::Paragraph) => {
                match current_block {
                    Some((BlockType::Paragraph, start, _)) => {
                        let text = content[start..range.end].trim().to_string();
                        if !text.is_empty() {
                            blocks.push(MarkdownBlock {
                                block_type: BlockType::Paragraph,
                                content: text,
                            });
                        }
                        current_block = None;
                    }
                    _ => {
                        // Paragraph inside list/blockquote — extend parent
                        if let Some((_, _, ref mut end)) = current_block {
                            *end = range.end;
                        }
                    }
                }
            }
            _ => {
                // Extend current block's end offset for any content events
                if let Some((_, _, ref mut end)) = current_block {
                    if range.end > *end {
                        *end = range.end;
                    }
                }
            }
        }
    }

    // Flush remaining block
    if let Some((bt, start, end)) = current_block {
        let text = content[start..end].trim().to_string();
        if !text.is_empty() {
            blocks.push(MarkdownBlock {
                block_type: bt,
                content: text,
            });
        }
    }

    blocks
}

/// Split a block by sentences if it exceeds the character limit
fn split_block_by_sentences(content: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();

    let sentence_endings = [". ", "! ", "? ", ".\n", "!\n", "?\n"];
    let mut remaining = content;

    while !remaining.is_empty() {
        let mut best_pos = None;
        for ending in &sentence_endings {
            if let Some(pos) = remaining.find(ending) {
                let end_pos = pos + ending.len();
                match best_pos {
                    None => best_pos = Some(end_pos),
                    Some(current) if end_pos < current => best_pos = Some(end_pos),
                    _ => {}
                }
            }
        }

        let (sentence, rest) = match best_pos {
            Some(pos) => (&remaining[..pos], &remaining[pos..]),
            None => (remaining, ""),
        };

        // If adding this sentence exceeds limit, start new chunk
        if current_chunk.len() + sentence.len() > max_chars && !current_chunk.is_empty() {
            chunks.push(current_chunk.clone());
            current_chunk = String::new();
        }

        // If single sentence is too large, hard split it
        if sentence.len() > max_chars {
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                current_chunk = String::new();
            }
            let hard_splits = hard_split_by_chars(sentence, max_chars);
            chunks.extend(hard_splits);
        } else {
            current_chunk.push_str(sentence);
        }

        remaining = rest;
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

/// Hard split text by character count (last resort, char-boundary safe)
fn hard_split_by_chars(text: &str, max_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + max_chars).min(text.len());
        // Ensure we're at a char boundary
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // Shouldn't happen with valid UTF-8, but avoid infinite loop
            break;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }

    chunks
}

/// Split an oversized code block at line boundaries, re-wrapping each piece
/// in the original fence so every piece still reads (and embeds) as code.
///
/// Code blocks normally stay intact for syntax integrity, but a block larger
/// than `max_chars` exceeds the embedding model's context window, which fails
/// embedding for the whole atom — a re-fenced split is strictly better than
/// no embedding at all.
fn split_code_block(content: &str, max_chars: usize) -> Vec<String> {
    let mut lines = content.lines();
    let first_line = lines.next().unwrap_or("");
    let opener = first_line.trim_start();

    // Fenced blocks get their fence lines peeled off so only the body is
    // split, then each piece is re-wrapped. Indented blocks have no fence.
    let (open, close, body_lines) = if opener.starts_with("```") || opener.starts_with("~~~") {
        let fence_char = opener.chars().next().unwrap();
        let fence_len = opener.chars().take_while(|&c| c == fence_char).count();
        let mut body: Vec<&str> = lines.collect();
        // Unterminated blocks (EOF inside the fence) have no closing line.
        if body.last().is_some_and(|line| {
            let trimmed = line.trim();
            trimmed.len() >= 3 && trimmed.chars().all(|c| c == fence_char)
        }) {
            body.pop();
        }
        (first_line, fence_char.to_string().repeat(fence_len), body)
    } else {
        ("", String::new(), content.lines().collect())
    };

    let overhead = if open.is_empty() {
        0
    } else {
        open.len() + close.len() + 2 // the newlines joining fences to body
    };
    let budget = max_chars.saturating_sub(overhead).max(CHARS_PER_TOKEN);

    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in body_lines {
        if !current.is_empty() && current.len() + 1 + line.len() > budget {
            pieces.push(std::mem::take(&mut current));
        }
        if line.len() > budget {
            // A single line past the budget (minified JS, base64 blobs, …)
            // offers no boundary to honor — hard-split it.
            pieces.extend(hard_split_by_chars(line, budget));
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    if pieces.is_empty() {
        return vec![content.to_string()];
    }

    pieces
        .into_iter()
        .map(|body| {
            if open.is_empty() {
                body
            } else {
                format!("{open}\n{body}\n{close}")
            }
        })
        .collect()
}

/// Merge adjacent small blocks into chunks respecting character limits
fn merge_blocks_into_chunks(blocks: Vec<MarkdownBlock>) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();

    for block in blocks {
        let block_len = block.content.len();

        // Code blocks stay intact for syntax integrity — but only up to
        // MAX_CHUNK_CHARS. Beyond that the chunk exceeds the embedding
        // model's context window and the whole atom fails to embed, so
        // oversized blocks are split at line boundaries and re-fenced.
        if block.block_type == BlockType::CodeBlock {
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                current_chunk = String::new();
            }

            if block_len > MAX_CHUNK_CHARS {
                chunks.extend(split_code_block(&block.content, MAX_CHUNK_CHARS));
            } else if block_len > TARGET_CHUNK_CHARS {
                chunks.push(block.content);
            } else {
                // Small code block - can be combined
                current_chunk = block.content;
            }
            continue;
        }

        // Headers start new chunks (natural boundaries)
        if block.block_type == BlockType::Header {
            if !current_chunk.is_empty() && current_chunk.len() >= MIN_CHUNK_CHARS {
                chunks.push(current_chunk.clone());
                current_chunk = String::new();
            }
        }

        // Check if block fits in current chunk
        if current_chunk.len() + block_len <= TARGET_CHUNK_CHARS {
            if !current_chunk.is_empty() {
                current_chunk.push_str("\n\n");
            }
            current_chunk.push_str(&block.content);
        } else if block_len > TARGET_CHUNK_CHARS {
            // Block is too large - need to split it
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                current_chunk = String::new();
            }

            let sub_chunks = split_block_by_sentences(&block.content, TARGET_CHUNK_CHARS);

            for (i, sub_chunk) in sub_chunks.into_iter().enumerate() {
                if i == 0 || current_chunk.len() + sub_chunk.len() > TARGET_CHUNK_CHARS {
                    if !current_chunk.is_empty() {
                        chunks.push(current_chunk.clone());
                    }
                    current_chunk = sub_chunk;
                } else {
                    current_chunk.push_str(&sub_chunk);
                }
            }
        } else {
            // Start new chunk with this block
            if !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
            }
            current_chunk = block.content;
        }
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

/// Merge small chunks with adjacent chunks
fn merge_small_chunks(chunks: Vec<String>) -> Vec<String> {
    if chunks.is_empty() {
        return chunks;
    }

    let mut result: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;

    for chunk in chunks {
        if let Some(prev) = pending.take() {
            let merged = format!("{}\n\n{}", prev, chunk);

            if merged.len() <= TARGET_CHUNK_CHARS {
                if merged.len() < MIN_CHUNK_CHARS {
                    pending = Some(merged);
                } else {
                    result.push(merged);
                }
            } else {
                if prev.len() >= MIN_CHUNK_CHARS {
                    result.push(prev);
                } else if !result.is_empty() {
                    let last = result.pop().unwrap();
                    result.push(format!("{}\n\n{}", last, prev));
                } else {
                    result.push(prev);
                }

                if chunk.len() < MIN_CHUNK_CHARS {
                    pending = Some(chunk);
                } else {
                    result.push(chunk);
                }
            }
        } else if chunk.len() < MIN_CHUNK_CHARS {
            pending = Some(chunk);
        } else {
            result.push(chunk);
        }
    }

    if let Some(remaining) = pending {
        if !result.is_empty() {
            let last = result.pop().unwrap();
            result.push(format!("{}\n\n{}", last, remaining));
        } else {
            result.push(remaining);
        }
    }

    result
}

/// Apply overlap between consecutive chunks
fn apply_overlap(chunks: Vec<String>) -> Vec<String> {
    if chunks.len() <= 1 || OVERLAP_CHARS == 0 {
        return chunks;
    }

    let mut result = Vec::with_capacity(chunks.len());

    for (i, chunk) in chunks.iter().enumerate() {
        if i < chunks.len() - 1 {
            let next_overlap = get_first_n_chars(&chunks[i + 1], OVERLAP_CHARS);
            result.push(format!("{}\n\n{}", chunk, next_overlap));
        } else {
            result.push(chunk.clone());
        }
    }

    result
}

/// Chunks content into smaller pieces for embedding generation.
///
/// Uses pulldown-cmark for markdown parsing and character-based size estimates
/// for fast chunking without tokenization overhead.
pub fn chunk_content(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }

    // 1. Parse markdown structure
    let blocks = parse_markdown_blocks(content);

    if blocks.is_empty() {
        return Vec::new();
    }

    // 2. Merge blocks into chunks respecting size limits
    let chunks = merge_blocks_into_chunks(blocks);

    // 3. Merge any remaining small chunks
    let chunks = merge_small_chunks(chunks);

    // 4. Apply overlap
    let chunks = apply_overlap(chunks);

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_counting() {
        let text = "Hello, world!";
        let tokens = count_tokens(text);
        assert!(tokens > 0);
        assert!(tokens < 10); // Should be ~4 tokens
    }

    #[test]
    fn test_empty_content() {
        let chunks = chunk_content("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_simple_paragraphs() {
        let content = "First paragraph with enough content to stand alone.\n\nSecond paragraph also with content.";
        let chunks = chunk_content(content);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_code_block_not_split() {
        let content = r#"Some intro text.

```rust
fn main() {
    println!("Hello, world!");
    // This is a code block
    // It should not be split
    for i in 0..100 {
        println!("{}", i);
    }
}
```

Some outro text."#;
        let chunks = chunk_content(content);

        let code_chunk = chunks.iter().find(|c| c.contains("fn main()"));
        assert!(code_chunk.is_some(), "Code block should be in output");

        let code = code_chunk.unwrap();
        assert!(
            code.contains("```rust"),
            "Code block should have opening fence"
        );
        assert!(
            code.contains("for i in 0..100"),
            "Code block should be complete"
        );
    }

    #[test]
    fn test_header_creates_boundary() {
        let content = r#"# First Section

This is content under the first section with enough text to be meaningful.

# Second Section

This is content under the second section with different information."#;

        let blocks = parse_markdown_blocks(content);

        let header_count = blocks
            .iter()
            .filter(|b| b.block_type == BlockType::Header)
            .count();
        assert_eq!(header_count, 2, "Should identify 2 headers");
    }

    #[test]
    fn test_list_kept_together() {
        let content = r#"Here are some items:

- First item
- Second item
- Third item

After the list."#;

        let blocks = parse_markdown_blocks(content);

        let list_block = blocks.iter().find(|b| b.block_type == BlockType::List);
        assert!(list_block.is_some(), "Should identify list block");

        let list = list_block.unwrap();
        assert!(list.content.contains("First item"));
        assert!(list.content.contains("Third item"));
    }

    #[test]
    fn test_overlap_applied() {
        let long_para = "This is a test sentence with enough content. ".repeat(500);
        let content = format!("{}\n\nFinal paragraph with unique content.", long_para);

        let chunks = chunk_content(&content);

        if chunks.len() > 1 {
            let first = &chunks[0];
            assert!(!first.is_empty());
        }
    }

    #[test]
    fn test_numbered_list() {
        let content = r#"Steps to follow:

1. First step
2. Second step
3. Third step

Done!"#;

        let blocks = parse_markdown_blocks(content);
        let list_block = blocks.iter().find(|b| b.block_type == BlockType::List);
        assert!(list_block.is_some(), "Should identify numbered list");
    }

    #[test]
    fn test_small_chunks_merged() {
        let content = "Title\n\nSubtitle\n\nA longer paragraph with actual meaningful content that should stand alone.";
        let chunks = chunk_content(content);

        assert!(!chunks.is_empty());
        assert!(chunks[0].contains("Title"));
    }

    #[test]
    fn test_get_first_n_chars() {
        let text = "Hello world, this is a test sentence.";
        let first = get_first_n_chars(text, 10);
        assert_eq!(first.len(), 10);
        assert_eq!(first, "Hello worl");
    }

    #[test]
    fn test_preserves_whitespace_in_code() {
        let content = r#"```python
def foo():
    if True:
        print("indented")
```"#;

        let chunks = chunk_content(content);
        assert!(!chunks.is_empty());

        let chunk = &chunks[0];
        assert!(chunk.contains("    if True:"));
        assert!(chunk.contains("        print"));
    }

    #[test]
    fn test_multiple_code_blocks() {
        let content = r#"First code:

```js
console.log("first");
```

Second code:

```python
print("second")
```"#;

        let blocks = parse_markdown_blocks(content);
        let code_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.block_type == BlockType::CodeBlock)
            .collect();

        assert_eq!(code_blocks.len(), 2, "Should find 2 code blocks");
    }

    #[test]
    fn test_sentence_splitting() {
        let long_text =
            "This is sentence one. This is sentence two! Is this sentence three? Yes it is. "
                .repeat(50);
        let splits = split_block_by_sentences(&long_text, 2000);

        assert!(splits.len() > 1);

        for (i, split) in splits.iter().enumerate() {
            if i < splits.len() - 1 {
                let trimmed = split.trim();
                assert!(
                    trimmed.ends_with('.') || trimmed.ends_with('!') || trimmed.ends_with('?'),
                    "Split {} should end with sentence punctuation: '{}'",
                    i,
                    &trimmed[trimmed.len().saturating_sub(20)..]
                );
            }
        }
    }

    #[test]
    fn test_hard_split_multibyte() {
        // Ensure hard split handles multi-byte chars
        let text = "ñ".repeat(100);
        let splits = hard_split_by_chars(&text, 10);
        for split in &splits {
            assert!(split.len() <= 10 || split.len() <= "ñ".len()); // May slightly exceed due to char boundary
        }
    }

    #[test]
    fn test_split_code_block_refences_pieces() {
        let body: Vec<String> = (0..2000).map(|i| format!("println!(\"line {i}\");")).collect();
        let content = format!("```rust\n{}\n```", body.join("\n"));
        assert!(content.len() > MAX_CHUNK_CHARS);

        let pieces = split_code_block(&content, MAX_CHUNK_CHARS);

        assert!(pieces.len() > 1, "oversized block should split");
        for piece in &pieces {
            assert!(piece.len() <= MAX_CHUNK_CHARS, "piece exceeds max: {}", piece.len());
            assert!(piece.starts_with("```rust\n"), "piece keeps the language fence");
            assert!(piece.ends_with("\n```"), "piece is re-terminated");
        }

        // No line lost or reordered by the split.
        let mut reassembled: Vec<&str> = Vec::new();
        for piece in &pieces {
            let inner = piece
                .strip_prefix("```rust\n")
                .unwrap()
                .strip_suffix("\n```")
                .unwrap();
            reassembled.extend(inner.lines());
        }
        let original: Vec<&str> = body.iter().map(String::as_str).collect();
        assert_eq!(reassembled, original);
    }

    #[test]
    fn test_split_code_block_hard_splits_giant_line() {
        // A single line with no boundaries (minified JS, base64 blob)
        let line = "x".repeat(3 * MAX_CHUNK_CHARS);
        let content = format!("```\n{line}\n```");

        let pieces = split_code_block(&content, MAX_CHUNK_CHARS);

        assert!(pieces.len() >= 3);
        let mut total_body = 0;
        for piece in &pieces {
            assert!(piece.len() <= MAX_CHUNK_CHARS);
            total_body += piece
                .strip_prefix("```\n")
                .unwrap()
                .strip_suffix("\n```")
                .unwrap()
                .len();
        }
        assert_eq!(total_body, line.len(), "hard split must not drop characters");
    }

    #[test]
    fn test_code_block_between_target_and_max_stays_intact() {
        let body = "let x = 0;\n".repeat(320); // ~3.5KB: above TARGET, below MAX
        let content = format!("```rust\n{body}```");
        assert!(content.len() > TARGET_CHUNK_CHARS && content.len() <= MAX_CHUNK_CHARS);

        let chunks = chunk_content(&content);

        assert_eq!(chunks.len(), 1, "code blocks under MAX stay whole");
    }

    #[test]
    fn test_issue_197_oversized_code_block_stays_embeddable() {
        // Regression for issue #197: an RSS-ingested mailing-list post whose
        // body is one giant fenced code block (~72KB) became a single ~68KB
        // chunk, exceeding the embedding model's context window and failing
        // embedding for the whole atom.
        let body_line = "static const struct option long_options[] = { /* ... */ };";
        let lines = vec![body_line; 1200].join("\n");
        let content = format!("Advisory intro text.\n\n```c\n{lines}\n```\n\nPowered by blists.");
        assert!(content.len() > 60_000);

        let chunks = chunk_content(&content);

        for chunk in &chunks {
            // +2 accounts for the "\n\n" joining a chunk to its overlap.
            assert!(
                chunk.len() <= MAX_CHUNK_CHARS + OVERLAP_CHARS + 2,
                "chunk of {} chars would exceed the embedding context window",
                chunk.len()
            );
        }
        assert!(
            chunks.iter().filter(|c| c.contains(body_line)).count() > 1,
            "the code should survive, spread across multiple chunks"
        );
    }

    #[test]
    fn test_nested_code_in_list() {
        let content = r#"- Item with code:
  ```
  code here
  ```
- Next item"#;

        let blocks = parse_markdown_blocks(content);

        // The list should remain a single List block, not be split by the code block
        let list_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.block_type == BlockType::List)
            .collect();
        assert_eq!(list_blocks.len(), 1, "Should be a single list block");

        let list = &list_blocks[0];
        assert!(
            list.content.contains("Item with code"),
            "List should contain first item"
        );
        assert!(
            list.content.contains("code here"),
            "List should contain code block"
        );
        assert!(
            list.content.contains("Next item"),
            "List should contain item after code"
        );

        // No standalone code blocks should be produced
        let code_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.block_type == BlockType::CodeBlock)
            .collect();
        assert_eq!(
            code_blocks.len(),
            0,
            "Code inside list should not produce standalone CodeBlock"
        );
    }
}
