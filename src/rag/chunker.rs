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
/// Minimum characters before a fragment is considered too small and should be
/// merged into a neighboring chunk.
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

/// Chunk `content` semantically: structural units (markdown sections or
/// paragraphs) merged while adjacent units stay lexically cohesive.
///
/// Plain text deliberately uses per-paragraph units instead of the windowed
/// packer (`chunk_paragraph_texts`): packing first would erase the natural
/// boundaries the similarity merge needs to operate on.
pub fn chunk_semantic(content: &str, lang: &str, threshold: f32) -> Vec<SemanticChunk> {
    let chunks = match lang {
        "markdown" => annotate_similarity(coalesce_small_chunks(chunk_markdown_texts(content))),
        _ => chunk_paragraphs(content),
    };
    merge_similar_chunks(chunks, threshold)
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

/// Merge adjacent chunks that are lexically cohesive — similarity at or above
/// `threshold` — so each chunk covers one topic and boundaries land where the
/// vocabulary shifts (TextTiling-style). Merged chunks never grow past the
/// structural window size, keeping retrieval granularity bounded.
///
/// Threshold guidance (measured on real markdown docs): adjacent same-topic
/// sections score ~0.2–0.5 on term-frequency cosine; unrelated ones ~0.0–0.15.
pub fn merge_similar_chunks(chunks: Vec<SemanticChunk>, threshold: f32) -> Vec<SemanticChunk> {
    let max_len = MAX_CHUNK_TOKENS * CHARS_PER_TOKEN;
    let mut iter = chunks.into_iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };

    let threshold = threshold.clamp(0.0, 1.0);
    let mut merged_texts = vec![first.content];

    for chunk in iter {
        let is_similar = chunk.similarity_to_prev.unwrap_or(0.0) >= threshold;
        let current = merged_texts
            .last_mut()
            .expect("merged_texts starts non-empty");
        let fits = current.len() + 2 + chunk.content.len() <= max_len;

        if is_similar && fits {
            current.push_str("\n\n");
            current.push_str(&chunk.content);
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
        coalesce_small_chunks(chunks)
    }
}

/// Default semantic paragraph chunking.
///
/// Each paragraph becomes its own chunk so similarity can be evaluated across
/// natural boundaries. Oversized paragraphs are still split with the same
/// window/overlap constraints used by structural chunking; undersized
/// fragments coalesce into their neighbor.
fn chunk_paragraphs(content: &str) -> Vec<SemanticChunk> {
    let units = content
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .flat_map(split_paragraph)
        .collect();

    annotate_similarity(coalesce_small_chunks(units))
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

    coalesce_small_chunks(chunks)
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
    if !trimmed.is_empty() {
        chunks.push(trimmed.to_owned());
    }
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

fn coalesce_small_chunks(chunks: Vec<String>) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;

    for chunk in chunks.into_iter().map(|chunk| chunk.trim().to_string()) {
        if chunk.is_empty() {
            continue;
        }

        if chunk.len() < MIN_CHUNK_CHARS {
            if let Some(last) = merged.last_mut() {
                last.push_str("\n\n");
                last.push_str(&chunk);
            } else {
                pending = Some(match pending.take() {
                    Some(mut acc) => {
                        acc.push_str("\n\n");
                        acc.push_str(&chunk);
                        acc
                    }
                    None => chunk,
                });
            }
            continue;
        }

        if let Some(pending_chunk) = pending.take() {
            merged.push(format!("{pending_chunk}\n\n{chunk}"));
            continue;
        }

        merged.push(chunk);
    }

    if let Some(pending_chunk) = pending {
        if let Some(last) = merged.last_mut() {
            last.push_str("\n\n");
            last.push_str(&pending_chunk);
        } else {
            merged.push(pending_chunk);
        }
    }

    merged
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
    fn chunk_semantic_merges_similar_neighbors_and_splits_at_topic_shifts() {
        // Sections One and Two share vocabulary (same topic); Three is disjoint.
        let content = "# One\n\nalpha beta gamma\n\n## Two\n\nalpha beta delta\n\n## Three\n\nomega sigma tau";
        let chunks = chunk_semantic(content, "markdown", 0.2);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.contains("# One"));
        assert!(chunks[0].content.contains("## Two"));
        assert!(chunks[1].content.contains("## Three"));
        assert!(chunks[1].similarity_to_prev.is_some());
    }

    #[test]
    fn chunk_semantic_keeps_dissimilar_chunks_separate() {
        // Cosine between these paragraphs is 0.4 (2 shared of 5 terms each),
        // below a 0.5 threshold — they must stay separate chunks.
        let content = "alpha beta gamma delta epsilon\n\ndelta epsilon zeta theta iota";
        let chunks = chunk_semantic(content, "text", 0.5);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].similarity_to_prev, None);
        assert!(chunks[1].similarity_to_prev.is_some());
    }

    #[test]
    fn chunk_semantic_merges_when_similarity_meets_threshold() {
        let content = "alpha beta gamma delta epsilon\n\ndelta epsilon zeta theta iota";
        let chunks = chunk_semantic(content, "text", 0.3);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].similarity_to_prev, None);
        assert!(chunks[0].content.contains("delta epsilon zeta"));
    }

    #[test]
    fn merge_similar_chunks_respects_window_size_cap() {
        // Identical content (similarity 1.0) would always merge, but the
        // combined size must never exceed the structural window.
        let big = "alpha beta gamma ".repeat(80); // ~1.3 KB each
        let texts: Vec<String> = (0..4).map(|_| big.trim().to_string()).collect();
        let chunks = merge_similar_chunks(annotate_similarity(texts), 0.2);

        let window = 512 * 4;
        assert!(
            chunks.len() > 1,
            "size cap must prevent a single mega-chunk"
        );
        for chunk in &chunks {
            assert!(chunk.content.len() <= window);
        }
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
    fn push_chunk_keeps_short_text_for_later_merge() {
        let mut chunks = Vec::new();
        push_chunk(&mut chunks, "hi");
        push_chunk(&mut chunks, "123");
        push_chunk(&mut chunks, "0 20 40 60");
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn push_chunk_accepts_meaningful_text() {
        let mut chunks = Vec::new();
        push_chunk(&mut chunks, "noise reduction in weak lensing images");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn coalesce_small_chunks_prefers_previous_chunk() {
        let chunks = coalesce_small_chunks(vec![
            "first substantial chunk with enough words".to_string(),
            "2".to_string(),
            "third substantial chunk with enough words".to_string(),
        ]);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("2"));
    }

    #[test]
    fn coalesce_small_chunks_attaches_leading_fragments_to_next_chunk() {
        let chunks = coalesce_small_chunks(vec![
            "1".to_string(),
            "2".to_string(),
            "first substantial chunk with enough words".to_string(),
        ]);

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].starts_with("1"));
        assert!(chunks[0].contains("first substantial chunk"));
    }
}

#[cfg(test)]
mod additional_tests {
    use super::*;

    // ── detect_lang ────────────────────────────────────────────────────

    #[test]
    fn detect_lang_with_path_prefix() {
        assert_eq!(detect_lang("/home/user/docs/file.md"), Some("markdown"));
    }

    #[test]
    fn detect_lang_with_dot_prefix() {
        assert_eq!(detect_lang(".hidden.pdf"), Some("text"));
    }

    #[test]
    fn detect_lang_case_sensitive_extension() {
        assert_eq!(detect_lang("file.MD"), None);
        assert_eq!(detect_lang("file.PDF"), None);
    }

    #[test]
    fn detect_lang_no_extension() {
        assert_eq!(detect_lang("Makefile"), None);
        assert_eq!(detect_lang("README"), None);
    }

    #[test]
    fn detect_lang_empty_path() {
        assert_eq!(detect_lang(""), None);
    }

    #[test]
    fn detect_lang_dot_only() {
        assert_eq!(detect_lang("."), None);
    }

    // ── term_frequencies ───────────────────────────────────────────────

    #[test]
    fn term_frequencies_empty_string() {
        let freq = term_frequencies("");
        assert!(freq.is_empty());
    }

    #[test]
    fn term_frequencies_single_word() {
        let freq = term_frequencies("hello");
        assert_eq!(freq.len(), 1);
        assert_eq!(freq["hello"], 1.0);
    }

    #[test]
    fn term_frequencies_repeated_tokens() {
        let freq = term_frequencies("cat cat cat");
        assert_eq!(freq.len(), 1);
        assert_eq!(freq["cat"], 3.0);
    }

    #[test]
    fn term_frequencies_lowercases_tokens() {
        let freq = term_frequencies("Foo foo FOO");
        assert_eq!(freq.len(), 1);
        assert_eq!(freq["foo"], 3.0);
    }

    #[test]
    fn term_frequencies_splits_on_non_alphanumeric() {
        let freq = term_frequencies("hello,world! it's-a test");
        assert!(freq.contains_key("hello"));
        assert!(freq.contains_key("world"));
        assert!(freq.contains_key("it"));
        assert!(freq.contains_key("s"));
        assert!(freq.contains_key("a"));
        assert!(freq.contains_key("test"));
    }

    #[test]
    fn term_frequencies_numeric_tokens() {
        let freq = term_frequencies("version 2.0 build 123");
        assert!(freq.contains_key("version"));
        assert!(freq.contains_key("2"));
        assert!(freq.contains_key("0"));
        assert!(freq.contains_key("build"));
        assert!(freq.contains_key("123"));
    }

    // ── cosine_similarity ──────────────────────────────────────────────

    #[test]
    fn cosine_similarity_empty_both() {
        assert_eq!(cosine_similarity("", ""), 0.0);
    }

    #[test]
    fn cosine_similarity_empty_left() {
        assert_eq!(cosine_similarity("", "hello world"), 0.0);
    }

    #[test]
    fn cosine_similarity_empty_right() {
        assert_eq!(cosine_similarity("hello world", ""), 0.0);
    }

    #[test]
    fn cosine_similarity_partial_overlap() {
        let sim = cosine_similarity("a b c d", "c d e f");
        assert!(sim > 0.0 && sim < 1.0);
    }

    #[test]
    fn cosine_similarity_symmetric() {
        let a = "alpha beta gamma";
        let b = "beta gamma delta";
        let sim_ab = cosine_similarity(a, b);
        let sim_ba = cosine_similarity(b, a);
        assert!((sim_ab - sim_ba).abs() < f32::EPSILON);
    }

    // ── annotate_similarity ────────────────────────────────────────────

    #[test]
    fn annotate_similarity_single_chunk() {
        let result = annotate_similarity(vec!["only one".to_string()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 0);
        assert_eq!(result[0].similarity_to_prev, None);
    }

    #[test]
    fn annotate_similarity_empty_input() {
        let result = annotate_similarity(Vec::new());
        assert!(result.is_empty());
    }

    #[test]
    fn annotate_similarity_two_identical_chunks() {
        let result = annotate_similarity(vec!["same text".into(), "same text".into()]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].similarity_to_prev, None);
        assert!((result[1].similarity_to_prev.unwrap() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn annotate_similarity_indices_are_sequential() {
        let chunks: Vec<String> = (0..5).map(|i| format!("chunk {i} text")).collect();
        let result = annotate_similarity(chunks);
        let indices: Vec<usize> = result.iter().map(|c| c.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn annotate_similarity_preserves_content() {
        let chunks = vec!["first".into(), "second".into(), "third".into()];
        let result = annotate_similarity(chunks);
        assert_eq!(result[0].content, "first");
        assert_eq!(result[1].content, "second");
        assert_eq!(result[2].content, "third");
    }

    // ── coalesce_small_chunks ──────────────────────────────────────────

    #[test]
    fn coalesce_small_chunks_empty_input() {
        assert!(coalesce_small_chunks(Vec::new()).is_empty());
    }

    #[test]
    fn coalesce_small_chunks_all_large() {
        let chunks = vec![
            "This is a substantial chunk of text with many words".into(),
            "Another substantial chunk of text with many words".into(),
        ];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn coalesce_small_chunks_all_small() {
        let chunks = vec!["tiny".into(), "small".into(), "little".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn coalesce_small_chunks_leading_small() {
        let chunks = vec!["a".into(), "b".into(), "This is a long enough chunk".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("a"));
        assert!(result[0].contains("b"));
        assert!(result[0].contains("long enough"));
    }

    #[test]
    fn coalesce_small_chunks_trailing_small() {
        let chunks = vec!["This is a long enough chunk".into(), "x".into(), "y".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("long enough"));
        assert!(result[0].contains("x"));
        assert!(result[0].contains("y"));
    }

    #[test]
    fn coalesce_small_chunks_interleaved_small() {
        let chunks = vec![
            "Long first chunk with many words here".into(),
            "x".into(),
            "Long second chunk with many words here".into(),
            "y".into(),
            "Long third chunk with many words here".into(),
        ];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 3);
        assert!(result[0].contains("x"));
        assert!(result[1].contains("y"));
    }

    #[test]
    fn coalesce_small_chunks_empty_strings_skipped() {
        let chunks = vec!["".into(), "This is a long enough chunk".into(), "".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn coalesce_small_chunks_whitespace_only_skipped() {
        let chunks = vec!["   \n  ".into(), "This is a long enough chunk".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn coalesce_small_chunks_pending_only() {
        let chunks = vec!["aa".into(), "bb".into()];
        let result = coalesce_small_chunks(chunks);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("aa"));
        assert!(result[0].contains("bb"));
    }

    // ── split_paragraph ────────────────────────────────────────────────

    #[test]
    fn split_paragraph_short_text_unchanged() {
        let text = "Short text that fits in one chunk.";
        let result = split_paragraph(text);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], text);
    }

    #[test]
    fn split_paragraph_exact_window_boundary() {
        let text = "x".repeat(MAX_CHUNK_TOKENS * CHARS_PER_TOKEN);
        let result = split_paragraph(&text);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn split_paragraph_one_over_window() {
        let text = "x".repeat(MAX_CHUNK_TOKENS * CHARS_PER_TOKEN + 1);
        let result = split_paragraph(&text);
        assert!(result.len() >= 2);
    }

    #[test]
    fn split_paragraph_preserves_all_content() {
        // The overlap mechanism means content is not exactly preserved on
        // join (some duplication at boundaries), but all chunks together
        // must cover every original character at least once.
        let text = "alpha beta gamma delta epsilon ".repeat(100);
        let result = split_paragraph(&text);
        assert!(result.len() >= 2, "must split into multiple chunks");
        let joined: String = result.join("");
        assert!(joined.len() >= text.len(), "joined chunks must cover original text");
    }

    #[test]
    fn split_paragraph_chunks_are_within_window() {
        let text = "word ".repeat(600);
        let result = split_paragraph(&text);
        let window = MAX_CHUNK_TOKENS * CHARS_PER_TOKEN;
        for chunk in &result {
            assert!(
                chunk.len() <= window + CHARS_PER_TOKEN,
                "chunk len {} exceeds window {window}",
                chunk.len()
            );
        }
    }

    // ── chunk_paragraph_texts ──────────────────────────────────────────

    #[test]
    fn chunk_paragraph_texts_empty_input() {
        let result = chunk_paragraph_texts("");
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_paragraph_texts_single_short_paragraph() {
        let result = chunk_paragraph_texts("Just one paragraph of text.");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_paragraph_texts_multiple_short_paragraphs_fit_one_chunk() {
        let result = chunk_paragraph_texts("First paragraph.\n\nSecond paragraph.");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_paragraph_texts_very_long_paragraph_splits() {
        let para = "word ".repeat(600);
        let result = chunk_paragraph_texts(&para);
        assert!(result.len() >= 2, "long paragraph must split");
    }

    #[test]
    fn chunk_paragraph_texts_blank_lines_filtered() {
        let content = "Paragraph one.\n\n\n\nParagraph two.";
        let result = chunk_paragraph_texts(content);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_paragraph_texts_many_short_paragraphs_merge() {
        let paragraphs: Vec<String> = (0..10).map(|i| format!("word{i} extra")).collect();
        let content = paragraphs.join("\n\n");
        let result = chunk_paragraph_texts(&content);
        assert_eq!(result.len(), 1);
    }

    // ── chunk_paragraphs ───────────────────────────────────────────────

    #[test]
    fn chunk_paragraphs_empty_input() {
        let result = chunk_paragraphs("");
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_paragraphs_single_paragraph() {
        let result = chunk_paragraphs("Just one paragraph.");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 0);
        assert_eq!(result[0].similarity_to_prev, None);
    }

    #[test]
    fn chunk_paragraphs_filters_blank_paragraphs() {
        let result = chunk_paragraphs("alpha beta gamma\n\n\n\nalpha beta gamma");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_paragraphs_long_paragraph_splits() {
        let big = "word ".repeat(600);
        let result = chunk_paragraphs(&big);
        assert!(result.len() >= 2);
    }

    // ── chunk (public API) ─────────────────────────────────────────────

    #[test]
    fn chunk_empty_markdown() {
        let result = chunk("", "markdown");
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_empty_text() {
        let result = chunk("", "text");
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_single_word_markdown() {
        let result = chunk("hello", "markdown");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 0);
    }

    #[test]
    fn chunk_single_word_text() {
        let result = chunk("hello", "text");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 0);
    }

    #[test]
    fn chunk_long_text_splits() {
        let text = "word ".repeat(600);
        let result = chunk(&text, "text");
        assert!(result.len() >= 2);
    }

    #[test]
    fn chunk_markdown_with_nested_headings() {
        let md = "# Top\n\n## Sub1\n\nText A here with enough words to avoid coalesce\n\n## Sub2\n\nText B here with enough words to avoid coalesce\n\n# Top2\n\nText C here with enough words to avoid coalesce";
        let result = chunk(md, "markdown");
        assert!(result.len() >= 2);
    }

    #[test]
    fn chunk_markdown_no_headings_falls_back_to_paragraphs() {
        let md = "No headings here.\n\nJust plain text paragraphs.";
        let result = chunk(md, "markdown");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_text_indices_start_at_zero() {
        // "hello" and "world" are short (< MIN_CHUNK_CHARS), so coalesce
        // merges them into one chunk at index 0.
        let result = chunk("hello\n\nworld", "text");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 0);
    }

    // ── merge_similar_chunks ───────────────────────────────────────────

    #[test]
    fn merge_similar_chunks_empty_input() {
        let result = merge_similar_chunks(Vec::new(), 0.5);
        assert!(result.is_empty());
    }

    #[test]
    fn merge_similar_chunks_single_chunk() {
        let chunks = vec![SemanticChunk {
            index: 0,
            content: "only one".into(),
            similarity_to_prev: None,
        }];
        let result = merge_similar_chunks(chunks, 0.5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].similarity_to_prev, None);
    }

    #[test]
    fn merge_similar_chunks_threshold_zero_always_merges() {
        let chunks = annotate_similarity(vec![
            "alpha beta gamma delta".into(),
            "alpha beta gamma delta".into(),
        ]);
        let result = merge_similar_chunks(chunks, 0.0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merge_similar_chunks_threshold_one_identical_still_merges() {
        let chunks = annotate_similarity(vec![
            "alpha beta gamma".into(),
            "alpha beta gamma".into(),
        ]);
        let result = merge_similar_chunks(chunks, 1.0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merge_similar_chunks_threshold_one_no_merge_for_partial_overlap() {
        let chunks = annotate_similarity(vec![
            "alpha beta gamma delta".into(),
            "alpha beta epsilon zeta".into(),
        ]);
        let result = merge_similar_chunks(chunks, 1.0);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn merge_similar_chunks_clamps_threshold() {
        let chunks = annotate_similarity(vec![
            "alpha beta gamma delta".into(),
            "alpha beta gamma delta".into(),
        ]);
        let result = merge_similar_chunks(chunks, 5.0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn merge_similar_chunks_negative_threshold_clamps_to_zero() {
        let chunks = annotate_similarity(vec![
            "alpha beta gamma".into(),
            "delta epsilon zeta".into(),
        ]);
        let result = merge_similar_chunks(chunks, -1.0);
        assert_eq!(result.len(), 1);
    }

    // ── chunk_markdown_texts ───────────────────────────────────────────

    #[test]
    fn chunk_markdown_texts_empty() {
        let result = chunk_markdown_texts("");
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_markdown_texts_single_heading() {
        let result = chunk_markdown_texts("# Title");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("Title"));
    }

    #[test]
    fn chunk_markdown_texts_heading_with_no_content() {
        // Both chunks are short (< MIN_CHUNK_CHARS), so coalesce merges them.
        let result = chunk_markdown_texts("# Heading\n\n## Next");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_markdown_texts_no_headings_uses_paragraphs() {
        let result = chunk_markdown_texts("Plain text without headings.\n\nAnother paragraph.");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_markdown_texts_only_hash_line_not_heading() {
        let result = chunk_markdown_texts("#nospacenewline\nmore text");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_markdown_texts_multiple_h1_sections() {
        let md = "# Section 1\n\nText one.\n\n# Section 2\n\nText two.\n\n# Section 3\n\nText three.";
        let result = chunk_markdown_texts(md);
        assert!(result.len() >= 3);
    }

    #[test]
    fn chunk_markdown_texts_code_block_not_split() {
        let md = "# Title\n\n```\ncode here\n```\n\nMore text.";
        let result = chunk_markdown_texts(md);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_markdown_texts_blank_only_input() {
        let result = chunk_markdown_texts("\n\n\n");
        assert!(result.is_empty());
    }

    // ── chunk_semantic (additional) ────────────────────────────────────

    #[test]
    fn chunk_semantic_empty_text() {
        let result = chunk_semantic("", "text", 0.5);
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_semantic_empty_markdown() {
        let result = chunk_semantic("", "markdown", 0.5);
        assert!(result.is_empty());
    }

    #[test]
    fn chunk_semantic_single_paragraph() {
        let result = chunk_semantic("Just one paragraph of text.", "text", 0.5);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].similarity_to_prev, None);
    }

    #[test]
    fn chunk_semantic_text_merge_all_threshold_zero() {
        let content = "alpha beta gamma\n\ndelta epsilon zeta";
        let result = chunk_semantic(content, "text", 0.0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn chunk_semantic_text_keep_separate_high_threshold() {
        // Paragraphs must be long enough to survive coalesce_small_chunks.
        // Fully disjoint vocabulary ensures similarity is 0.0, below 0.5.
        let content = "alpha beta gamma delta epsilon zeta eta theta iota kappa\n\nomega pi rho sigma tau upsilon phi chi psi omega";
        let result = chunk_semantic(content, "text", 0.5);
        assert_eq!(result.len(), 2);
    }
}
