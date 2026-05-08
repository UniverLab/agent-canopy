#![allow(dead_code)]
//! Structural chunker for personal RAG: language detection + per-type strategies.
//!
//! Personal RAG supports only:
//! `.md .mdx`  → markdown (by heading)
//! `.pdf`      → paragraph / default

use std::collections::HashMap;

const MAX_CHUNK_TOKENS: usize = 512;
const OVERLAP_TOKENS: usize = 64;
// Rough approximation: 1 token ≈ 4 chars
const CHARS_PER_TOKEN: usize = 4;
/// Minimum characters a chunk must have to be indexed.
/// Filters PDF artifacts like isolated numbers, axis labels, figure captions.
const MIN_CHUNK_CHARS: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticChunk {
    pub index: usize,
    pub content: String,
    pub similarity_to_prev: Option<f32>,
}

/// Detect language tag from file extension.
/// Returns `None` for unsupported types (personal RAG: md, mdx, pdf only).
pub fn detect_lang(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "md" | "mdx" => Some("markdown"),
        "pdf" => Some("text"),
        _ => None,
    }
}

/// Chunk `content` according to the language strategy.
/// Returns `(chunk_index, text)` pairs.
pub fn chunk(content: &str, lang: &str) -> Vec<(usize, String)> {
    match lang {
        "markdown" => to_indexed_chunks(chunk_markdown_texts(content)),
        _ => to_indexed_chunks(chunk_paragraph_texts(content)),
    }
}

/// Chunk `content` using structural chunking followed by similarity-aware merges.
pub fn chunk_semantic(content: &str, lang: &str, threshold: f32) -> Vec<SemanticChunk> {
    let chunks = match lang {
        "markdown" => annotate_similarity(chunk_markdown_texts(content)),
        _ => chunk_paragraphs(content),
    };
    merge_low_similarity_chunks(chunks, threshold)
}

/// Compute cosine similarity using normalized term frequencies.
pub fn cosine_similarity(text1: &str, text2: &str) -> f32 {
    let left = term_frequencies(text1);
    let right = term_frequencies(text2);

    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    let dot = left
        .iter()
        .filter_map(|(token, left_freq)| right.get(token).map(|right_freq| left_freq * right_freq))
        .sum::<f32>();
    let left_norm = left
        .values()
        .map(|weight| weight * weight)
        .sum::<f32>()
        .sqrt();
    let right_norm = right
        .values()
        .map(|weight| weight * weight)
        .sum::<f32>()
        .sqrt();

    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        (dot / (left_norm * right_norm)).clamp(0.0, 1.0)
    }
}

/// Merge adjacent chunks whose similarity metadata falls below `threshold`.
pub fn merge_low_similarity_chunks(
    chunks: Vec<SemanticChunk>,
    threshold: f32,
) -> Vec<SemanticChunk> {
    let mut iter = chunks.into_iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };

    let threshold = threshold.clamp(0.0, 1.0);
    let mut merged_texts = vec![first.content];

    for chunk in iter {
        let should_merge = chunk.similarity_to_prev.unwrap_or(1.0) < threshold;
        if should_merge {
            if let Some(current) = merged_texts.last_mut() {
                current.push_str("\n\n");
                current.push_str(&chunk.content);
            }
        } else {
            merged_texts.push(chunk.content);
        }
    }

    annotate_similarity(merged_texts)
}

/// Markdown: split on headings (`#`, `##`, `###`).
fn chunk_markdown_texts(content: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in content.lines() {
        if line.starts_with('#') && !current.is_empty() {
            let text = current.join("\n");
            push_chunk(&mut chunks, &text);
            current.clear();
        }
        current.push(line);
    }

    if !current.is_empty() {
        let text = current.join("\n");
        push_chunk(&mut chunks, &text);
    }

    if chunks.is_empty() {
        chunk_paragraph_texts(content)
    } else {
        chunks
    }
}

/// Default semantic paragraph chunking.
///
/// Each paragraph becomes its own chunk so similarity can be evaluated across
/// natural boundaries. Oversized paragraphs are still split with the same
/// window/overlap constraints used by structural chunking.
fn chunk_paragraphs(content: &str) -> Vec<SemanticChunk> {
    let chunks = content
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .flat_map(split_paragraph)
        .collect();

    annotate_similarity(chunks)
}

fn chunk_paragraph_texts(content: &str) -> Vec<String> {
    let window = MAX_CHUNK_TOKENS * CHARS_PER_TOKEN;
    let overlap = OVERLAP_TOKENS * CHARS_PER_TOKEN;

    let paragraphs: Vec<&str> = content
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .collect();

    let mut chunks = Vec::new();
    let mut buf = String::new();

    for paragraph in paragraphs {
        if paragraph.len() > window {
            if !buf.is_empty() {
                let flushed = std::mem::take(&mut buf);
                push_chunk(&mut chunks, &flushed);
            }
            chunks.extend(split_paragraph(paragraph));
            continue;
        }

        let separator_len = usize::from(!buf.is_empty()) * 2;
        if buf.len() + separator_len + paragraph.len() > window && !buf.is_empty() {
            let flushed = std::mem::take(&mut buf);
            push_chunk(&mut chunks, &flushed);
            let overlap_start = chunks
                .last()
                .map(|chunk: &String| {
                    let byte_target = chunk.len().saturating_sub(overlap);
                    chunk
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&b| b >= byte_target)
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            buf = chunks
                .last()
                .map(|chunk| chunk[overlap_start..].to_owned())
                .unwrap_or_default();
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(paragraph);
    }

    if !buf.trim().is_empty() {
        push_chunk(&mut chunks, &buf);
    }

    chunks
}

fn split_paragraph(paragraph: &str) -> Vec<String> {
    let window = MAX_CHUNK_TOKENS * CHARS_PER_TOKEN;
    let overlap = OVERLAP_TOKENS * CHARS_PER_TOKEN;

    if paragraph.len() <= window {
        return vec![paragraph.to_owned()];
    }

    let mut chunks = Vec::new();
    let char_boundaries: Vec<usize> = paragraph.char_indices().map(|(i, _)| i).collect();
    let total_len = paragraph.len();
    let mut start = 0usize;

    while start < total_len {
        let target = start.saturating_add(window).min(total_len);
        let end = char_boundaries
            .iter()
            .copied()
            .find(|&b| b >= target)
            .unwrap_or(total_len);

        push_chunk(&mut chunks, &paragraph[start..end]);
        if end >= total_len {
            break;
        }

        let overlap_target = end.saturating_sub(overlap);
        start = char_boundaries
            .iter()
            .copied()
            .find(|&b| b >= overlap_target)
            .unwrap_or(end);
    }

    chunks
}

fn push_chunk(chunks: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if trimmed.len() >= MIN_CHUNK_CHARS && has_meaningful_content(trimmed) {
        chunks.push(trimmed.to_owned());
    }
}

/// Returns `true` if `text` contains at least one word with 3+ alphabetic characters.
/// Rejects chunks that are purely numeric, symbolic, or axis/table labels
/// (e.g. "123", "0 20 40 60", "σ = 19.4").
fn has_meaningful_content(text: &str) -> bool {
    text.split_whitespace()
        .any(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 3)
}

fn annotate_similarity(chunks: Vec<String>) -> Vec<SemanticChunk> {
    let mut previous: Option<String> = None;

    chunks
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let similarity_to_prev = previous
                .as_deref()
                .map(|prev| cosine_similarity(prev, &content));
            previous = Some(content.clone());
            SemanticChunk {
                index,
                content,
                similarity_to_prev,
            }
        })
        .collect()
}

fn to_indexed_chunks(chunks: Vec<String>) -> Vec<(usize, String)> {
    chunks.into_iter().enumerate().collect()
}

fn term_frequencies(text: &str) -> HashMap<String, f32> {
    let mut frequencies = HashMap::new();

    for token in text
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
    {
        *frequencies.entry(token).or_insert(0.0) += 1.0;
    }

    frequencies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_lang_markdown() {
        assert_eq!(detect_lang("README.md"), Some("markdown"));
        assert_eq!(detect_lang("notes.mdx"), Some("markdown"));
    }

    #[test]
    fn detect_lang_pdf() {
        assert_eq!(detect_lang("doc.pdf"), Some("text"));
    }

    #[test]
    fn detect_lang_unsupported_returns_none() {
        assert_eq!(detect_lang("main.rs"), None);
        assert_eq!(detect_lang("file.xyz"), None);
        assert_eq!(detect_lang("config.toml"), None);
    }

    #[test]
    fn chunk_markdown_splits_on_headings() {
        let md =
            "# Title\n\nIntro text.\n\n## Section A\n\nContent A.\n\n## Section B\n\nContent B.";
        let chunks = chunk(md, "markdown");
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].1.contains("Title"));
        assert!(chunks[1].1.contains("Section A"));
        assert!(chunks[2].1.contains("Section B"));
    }

    #[test]
    fn chunk_markdown_indices_are_sequential() {
        let md = "# Alpha Section\n\nsome text here\n\n## Beta Section\n\nmore content here";
        let chunks = chunk(md, "markdown");
        let indices: Vec<usize> = chunks.iter().map(|(index, _)| *index).collect();
        assert_eq!(indices, (0..chunks.len()).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_paragraphs_respects_window() {
        let paragraph = "word ".repeat(600);
        let chunks = chunk_paragraphs(&paragraph);
        assert!(chunks.len() >= 2, "should split into multiple chunks");
    }

    #[test]
    fn chunk_paragraphs_tracks_similarity_between_adjacent_paragraphs() {
        let content =
            "alpha beta gamma delta\n\nalpha beta delta epsilon\n\nomega sigma tau upsilon";
        let chunks = chunk_paragraphs(content);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].similarity_to_prev, None);
        assert!(chunks[1].similarity_to_prev.unwrap_or_default() > 0.0);
        assert_eq!(chunks[2].similarity_to_prev.unwrap_or_default(), 0.0);
    }

    #[test]
    fn cosine_similarity_returns_one_for_identical_text() {
        let similarity = cosine_similarity("alpha beta gamma", "alpha beta gamma");
        assert!((similarity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_returns_zero_for_disjoint_text() {
        let similarity = cosine_similarity("alpha beta gamma", "delta epsilon zeta");
        assert_eq!(similarity, 0.0);
    }

    #[test]
    fn chunk_semantic_merges_chunks_below_threshold() {
        let content = "# One\n\nalpha beta gamma\n\n## Two\n\nalpha beta delta\n\n## Three\n\nomega sigma tau";
        let chunks = chunk_semantic(content, "markdown", 0.2);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.contains("# One"));
        assert!(chunks[1].content.contains("## Two"));
        assert!(chunks[1].content.contains("## Three"));
        assert!(chunks[1].similarity_to_prev.is_some());
    }

    #[test]
    fn chunk_semantic_preserves_similarity_metadata_after_merging() {
        let content = "alpha beta gamma delta epsilon\n\ndelta epsilon zeta theta iota";
        let chunks = chunk_semantic(content, "text", 0.5);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].similarity_to_prev, None);
        assert!(chunks[0].content.contains("delta epsilon zeta"));
    }

    #[test]
    fn split_paragraph_handles_multibyte_utf8_without_panic() {
        let ligature = "ﬁ".repeat(600);
        let paragraph = format!("In this work we investigate {ligature}");
        let chunks = split_paragraph(&paragraph);
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(chunk.is_char_boundary(chunk.len()));
        }
    }

    #[test]
    fn chunk_paragraph_texts_handles_multibyte_utf8_without_panic() {
        let ligature = "ﬁ".repeat(600);
        let content = format!("alpha beta gamma delta epsilon\n\n{ligature}\n\ndelta epsilon iota");
        let chunks = chunk_paragraph_texts(&content);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn push_chunk_filters_short_text() {
        let mut chunks = Vec::new();
        push_chunk(&mut chunks, "hi");
        push_chunk(&mut chunks, "123");
        push_chunk(&mut chunks, "0 20 40 60");
        assert!(
            chunks.is_empty(),
            "all short/numeric chunks should be filtered"
        );
    }

    #[test]
    fn push_chunk_accepts_meaningful_text() {
        let mut chunks = Vec::new();
        push_chunk(&mut chunks, "noise reduction in weak lensing images");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn has_meaningful_content_rejects_pure_numbers() {
        assert!(!has_meaningful_content("123"));
        assert!(!has_meaningful_content("0.5"));
        assert!(!has_meaningful_content("0 20 40 60"));
        assert!(!has_meaningful_content("1.00"));
    }

    #[test]
    fn has_meaningful_content_accepts_text_with_words() {
        assert!(has_meaningful_content("noise reduction weak lensing"));
        assert!(has_meaningful_content("Introduction to signal processing"));
        assert!(has_meaningful_content("Ground Truth evaluation"));
    }
}
