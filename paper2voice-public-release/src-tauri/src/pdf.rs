use std::path::Path;

use anyhow::{bail, Context, Result};

pub fn extract_pdf_text_by_pages(path: &Path) -> Result<Vec<String>> {
    let pages = pdf_extract::extract_text_by_pages(path).with_context(|| {
        format!(
            "Failed to extract text by page from PDF: {}",
            path.display()
        )
    })?;

    let char_count = pages
        .iter()
        .map(|page| page.trim().chars().count())
        .sum::<usize>();

    if char_count < 300 {
        bail!("No extractable text found. This PDF may be scanned or image-based. MVP only supports selectable-text PDFs.");
    }

    Ok(pages)
}
