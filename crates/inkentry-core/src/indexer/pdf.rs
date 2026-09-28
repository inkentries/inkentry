/// Extract plain text from a PDF file, page by page.
/// Returns a Vec of (page_number, text) pairs.
/// Pages with no extractable text are skipped.
#[cfg(feature = "rich-formats")]
pub fn extract_pdf_text(path: &std::path::Path) -> anyhow::Result<Vec<(u32, String)>> {
    use lopdf::Document;
    let doc = Document::load(path)?;
    let mut pages = Vec::new();
    // get_pages() and extract_text() agree on the same 1-based page numbering.
    for page_num in doc.get_pages().keys().copied() {
        if let Ok(text) = doc.extract_text(&[page_num]) {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                pages.push((page_num, trimmed));
            }
        }
    }
    Ok(pages)
}
