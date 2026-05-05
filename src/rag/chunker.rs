#![allow(dead_code)]
//! Structural chunker for personal RAG: language detection + per-type strategies.
//!
//! Personal RAG supports only:
//! `.md .mdx`  → markdown (by heading)
//! `.pdf`      → paragraph / default

const MAX_CHUNK_TOKENS: usize = 512;
const OVERLAP_TOKENS: usize = 64;
// Rough approximation: 1 token ≈ 4 chars
const CHARS_PER_TOKEN: usize = 4;

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
        "markdown" => chunk_markdown(content),
        _ => chunk_paragraphs(content),
    }
}

/// Markdown: split on headings (`#`, `##`, `###`).
fn chunk_markdown(content: &str) -> Vec<(usize, String)> {
    let mut chunks: Vec<(usize, String)> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut idx = 0usize;

    for line in content.lines() {
        if line.starts_with('#') && !current.is_empty() {
            let text = current.join("\n").trim().to_owned();
            if !text.is_empty() {
                chunks.push((idx, text));
                idx += 1;
            }
            current.clear();
        }
        current.push(line);
    }
    if !current.is_empty() {
        let text = current.join("\n").trim().to_owned();
        if !text.is_empty() {
            chunks.push((idx, text));
        }
    }
    if chunks.is_empty() {
        chunk_paragraphs(content)
    } else {
        chunks
    }
}

/// Default: paragraph chunking with 512-token window and 64-token overlap.
fn chunk_paragraphs(content: &str) -> Vec<(usize, String)> {
    let window = MAX_CHUNK_TOKENS * CHARS_PER_TOKEN;
    let overlap = OVERLAP_TOKENS * CHARS_PER_TOKEN;

    let paragraphs: Vec<&str> = content
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    let mut chunks: Vec<(usize, String)> = Vec::new();
    let mut buf = String::new();
    let mut idx = 0usize;

    for para in &paragraphs {
        if buf.len() + para.len() > window && !buf.is_empty() {
            chunks.push((idx, buf.trim().to_owned()));
            idx += 1;
            let overlap_start = buf.len().saturating_sub(overlap);
            buf = buf[overlap_start..].to_owned();
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(para);
    }
    if !buf.trim().is_empty() {
        chunks.push((idx, buf.trim().to_owned()));
    }
    chunks
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
        let md = "# A\n\ntext\n\n## B\n\nmore";
        let chunks = chunk(md, "markdown");
        let indices: Vec<usize> = chunks.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, (0..chunks.len()).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_paragraphs_respects_window() {
        let para = "word ".repeat(150);
        let big = [para.as_str(); 6].join("\n\n");
        let chunks = chunk_paragraphs(&big);
        assert!(chunks.len() >= 2, "should split into multiple chunks");
    }
}
