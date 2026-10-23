//! Attachments: ingest (sniff, store content-addressed), and conversion into model input —
//! `patchbay_llm` parts for Chat, ACP `ContentBlock`s for Code.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use patchbay_llm::Part;

use crate::db::{Attachment, Db, new_id};

pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_PER_MESSAGE: usize = 10;
const MAX_IMAGE_EDGE: u32 = 2048;
const PDF_MAX_RENDERED_PAGES: usize = 20;
const PDF_SCANNED_PAGE_CHARS: usize = 24;
const PDF_RENDER_DPI: u32 = 110;

const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "json", "jsonl", "yaml", "yml", "toml", "xml", "html",
    "htm", "css", "log", "ini", "cfg", "conf", "env", "sql", "rs", "py", "js", "jsx", "ts", "tsx",
    "go", "java", "kt", "c", "h", "cpp", "hpp", "cc", "cs", "rb", "php", "swift", "sh", "bash",
    "zsh", "fish", "ps1", "lua", "pl", "r", "scala", "hs", "ex", "exs", "erl", "clj", "dart",
    "vue", "svelte", "tex", "rst", "org", "adoc", "diff", "patch", "proto", "graphql", "tf", "nix",
    "dockerfile", "makefile", "gitignore", "svg",
];

pub fn attachments_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

/// Where an attachment's bytes live: content-addressed, so every node computes the same name.
pub fn stored_path(data_dir: &Path, sha: &str, name: &str) -> PathBuf {
    let ext = extension(name);
    attachments_dir(data_dir).join(if ext.is_empty() || ext.contains('.') {
        sha.to_owned()
    } else {
        format!("{sha}.{ext}")
    })
}

fn extension(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match Path::new(&lower).extension() {
        Some(e) => e.to_string_lossy().into_owned(),
        // `Makefile`, `Dockerfile`, …
        None => Path::new(&lower)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Classify bytes into ("image" | "pdf" | "text", mime).
pub fn classify(name: &str, bytes: &[u8]) -> Result<(&'static str, String)> {
    let ext = extension(name);
    if let Some(t) = infer::get(bytes) {
        let mime = t.mime_type();
        if mime == "application/pdf" {
            return Ok(("pdf", mime.into()));
        }
        if matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        ) {
            return Ok(("image", mime.into()));
        }
        // Zip/Office/etc. are binary; some text formats (e.g. xml, html) sniff as text types.
        if !mime.starts_with("text/") && mime != "application/xml" {
            bail!("unsupported file type: {mime} ({name})");
        }
    }
    if bytes.starts_with(b"%PDF-") {
        return Ok(("pdf", "application/pdf".into()));
    }
    let looks_text = std::str::from_utf8(bytes).is_ok()
        || (!bytes.iter().take(8192).any(|b| *b == 0) && TEXT_EXTENSIONS.contains(&ext.as_str()));
    if looks_text {
        let mime = match ext.as_str() {
            "md" | "markdown" => "text/markdown",
            "csv" => "text/csv",
            "json" | "jsonl" => "application/json",
            "html" | "htm" => "text/html",
            "xml" | "svg" => "application/xml",
            _ => "text/plain",
        };
        return Ok(("text", mime.into()));
    }
    bail!("unsupported binary file: {name}")
}

pub fn ingest(db: &Db, data_dir: &Path, name: &str, bytes: &[u8]) -> Result<Attachment> {
    if bytes.len() > MAX_FILE_BYTES {
        bail!(
            "{name} is {:.1} MB; the limit is {} MB",
            bytes.len() as f64 / 1_048_576.0,
            MAX_FILE_BYTES / 1_048_576
        );
    }
    let (kind, mime) = classify(name, bytes)?;
    let sha = hex(&Sha256::digest(bytes));
    std::fs::create_dir_all(attachments_dir(data_dir))?;
    let file = stored_path(data_dir, &sha, name);
    if !file.exists() {
        std::fs::write(&file, bytes).with_context(|| format!("write {}", file.display()))?;
    }
    let att = Attachment {
        id: new_id(),
        sha256: sha,
        name: name.to_owned(),
        mime,
        kind: kind.to_owned(),
        size: bytes.len() as i64,
        path: file.to_string_lossy().into_owned(),
    };
    db.insert_attachment(&att)?;
    Ok(att)
}

pub fn ingest_path(db: &Db, data_dir: &Path, path: &Path) -> Result<Attachment> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() as usize > MAX_FILE_BYTES {
        bail!("{} exceeds the {} MB limit", path.display(), MAX_FILE_BYTES / 1_048_576);
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    ingest(db, data_dir, &name, &bytes)
}

/// Provider-neutral prepared form, cached per sha so history replays don't re-extract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Prepared {
    Text { text: String },
    Image { mime: String, b64: String },
}

fn cache_path(data_dir: &Path, att: &Attachment) -> PathBuf {
    attachments_dir(data_dir)
        .join("cache")
        .join(format!("{}.v1.json", att.sha256))
}

pub fn prepare(data_dir: &Path, att: &Attachment) -> Result<Vec<Prepared>> {
    let cache = cache_path(data_dir, att);
    if let Ok(raw) = std::fs::read(&cache)
        && let Ok(parts) = serde_json::from_slice::<Vec<Prepared>>(&raw)
    {
        return Ok(rename_text(parts, att));
    }
    let bytes = std::fs::read(&att.path).with_context(|| format!("read {}", att.path))?;
    let parts = match att.kind.as_str() {
        "image" => vec![prepare_image(&bytes, &att.mime)?],
        "pdf" => prepare_pdf(bytes)?,
        _ => vec![Prepared::Text {
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }],
    };
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, serde_json::to_vec(&parts)?);
    Ok(rename_text(parts, att))
}

/// Wrap text parts with the attachment's display name (the cache is name-independent).
fn rename_text(parts: Vec<Prepared>, att: &Attachment) -> Vec<Prepared> {
    parts
        .into_iter()
        .map(|p| match p {
            Prepared::Text { text } => Prepared::Text {
                text: format!(
                    "<file name=\"{}\" type=\"{}\">\n{}\n</file>",
                    att.name.replace('"', "'"),
                    att.mime,
                    text.trim_end()
                ),
            },
            other => other,
        })
        .collect()
}

fn prepare_image(bytes: &[u8], mime: &str) -> Result<Prepared> {
    let passthrough = matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp");
    let dims = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()
        .ok();
    let too_big = dims.is_some_and(|(w, h)| w.max(h) > MAX_IMAGE_EDGE);
    if passthrough && !too_big {
        return Ok(Prepared::Image {
            mime: mime.into(),
            b64: B64.encode(bytes),
        });
    }
    let img = image::load_from_memory(bytes).context("decode image")?;
    let img = if too_big {
        img.resize(
            MAX_IMAGE_EDGE,
            MAX_IMAGE_EDGE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let mut out = Vec::new();
    let (fmt, out_mime) = if mime == "image/jpeg" {
        (image::ImageFormat::Jpeg, "image/jpeg")
    } else {
        (image::ImageFormat::Png, "image/png")
    };
    let img = if fmt == image::ImageFormat::Jpeg {
        image::DynamicImage::ImageRgb8(img.to_rgb8())
    } else {
        img
    };
    img.write_to(&mut Cursor::new(&mut out), fmt)
        .context("encode image")?;
    Ok(Prepared::Image {
        mime: out_mime.into(),
        b64: B64.encode(&out),
    })
}

/// Text per page; pages with (almost) no extractable text are rendered to JPEG instead.
fn prepare_pdf(bytes: Vec<u8>) -> Result<Vec<Prepared>> {
    let doc = pdf_oxide::PdfDocument::from_bytes(bytes).map_err(|e| anyhow!("open PDF: {e}"))?;
    let pages = doc.page_count().map_err(|e| anyhow!("PDF page count: {e}"))?;
    let opts =
        pdf_oxide::rendering::RenderOptions::with_dpi(PDF_RENDER_DPI as _).as_jpeg(80);
    let mut text = String::new();
    let mut images = Vec::new();
    for idx in 0..pages {
        let page_text = doc.extract_text(idx).unwrap_or_default();
        text.push_str(&format!("--- Page {} ---\n", idx + 1));
        if page_text.trim().chars().count() >= PDF_SCANNED_PAGE_CHARS {
            text.push_str(page_text.trim_end());
            text.push('\n');
            continue;
        }
        if images.len() >= PDF_MAX_RENDERED_PAGES {
            text.push_str("[no extractable text; page image omitted (render limit reached)]\n");
            continue;
        }
        match pdf_oxide::rendering::render_page(&doc, idx, &opts) {
            Ok(img) => {
                text.push_str(&format!(
                    "[no extractable text; page {} attached as image]\n",
                    idx + 1
                ));
                images.push(Prepared::Image {
                    mime: "image/jpeg".into(),
                    b64: B64.encode(&img.data),
                });
            }
            Err(e) => text.push_str(&format!("[page could not be rendered: {e}]\n")),
        }
    }
    let mut out = vec![Prepared::Text { text }];
    out.extend(images);
    Ok(out)
}

/// The text of a PDF's pages (`pages` like "3" or "1-5", at most 20 at a time), for the Code
/// agent's Read tool. Scanned pages have no text and say so.
pub fn pdf_text(path: &Path, pages: Option<&str>) -> Result<String> {
    const MAX_PAGES: usize = 20;
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let doc = pdf_oxide::PdfDocument::from_bytes(bytes).map_err(|e| anyhow!("open PDF: {e}"))?;
    let total = doc.page_count().map_err(|e| anyhow!("PDF page count: {e}"))?;
    let (first, last) = match pages.map(str::trim).filter(|p| !p.is_empty()) {
        None => (1, total.min(MAX_PAGES)),
        Some(p) => {
            let (a, b) = p.split_once('-').unwrap_or((p, p));
            let a: usize = a.trim().parse().map_err(|_| anyhow!("pages must look like \"3\" or \"1-5\""))?;
            let b: usize = b.trim().parse().map_err(|_| anyhow!("pages must look like \"3\" or \"1-5\""))?;
            if a == 0 || b < a {
                bail!("pages must look like \"3\" or \"1-5\"");
            }
            if b - a + 1 > MAX_PAGES {
                bail!("read at most {MAX_PAGES} pages at a time");
            }
            (a, b.min(total))
        }
    };
    if first > total {
        bail!("the PDF has {total} pages");
    }
    let mut out = String::new();
    for idx in first..=last {
        let text = doc.extract_text(idx - 1).unwrap_or_default();
        out.push_str(&format!("--- Page {idx} of {total} ---\n"));
        if text.trim().is_empty() {
            out.push_str("[no extractable text on this page; it may be a scan]\n");
        } else {
            out.push_str(text.trim_end());
            out.push('\n');
        }
    }
    if last < total {
        out.push_str(&format!("\n[{} more pages; pass `pages` to read them]", total - last));
    }
    Ok(out)
}

pub fn to_content_parts(prepared: Vec<Prepared>) -> Vec<Part> {
    prepared
        .into_iter()
        .map(|p| match p {
            Prepared::Text { text } => Part::Text(text),
            Prepared::Image { mime, b64 } => Part::Image { mime, data: b64 },
        })
        .collect()
}

/// The PDF itself, base64-encoded. Only for providers that read PDFs natively.
pub fn pdf_document_part(att: &Attachment) -> Result<Part> {
    let bytes = std::fs::read(&att.path).with_context(|| format!("read {}", att.path))?;
    Ok(Part::Document {
        mime: "application/pdf".into(),
        name: att.name.clone(),
        data: B64.encode(bytes),
    })
}

/// ACP prompt blocks (wire JSON) for the Code tab.
pub fn to_acp_blocks(data_dir: &Path, att: &Attachment) -> Result<Vec<serde_json::Value>> {
    let uri = file_uri(Path::new(&att.path));
    Ok(match att.kind.as_str() {
        "image" => prepare(data_dir, att)?
            .into_iter()
            .filter_map(|p| match p {
                Prepared::Image { mime, b64 } => Some(serde_json::json!({
                    "type": "image", "mimeType": mime, "data": b64, "uri": uri,
                })),
                Prepared::Text { .. } => None,
            })
            .collect(),
        "text" => {
            let text = String::from_utf8_lossy(&std::fs::read(&att.path)?).into_owned();
            vec![serde_json::json!({
                "type": "resource",
                "resource": { "uri": format!("file:///{}", att.name), "mimeType": att.mime, "text": text },
            })]
        }
        // A PDF `resource_link` would be @-expanded as raw bytes, so send our own extraction:
        // text as an embedded resource, scanned pages as images.
        _ => prepare(data_dir, att)?
            .into_iter()
            .map(|p| match p {
                Prepared::Text { text } => serde_json::json!({
                    "type": "resource",
                    "resource": {
                        "uri": format!("file:///{}.txt", att.name),
                        "mimeType": "text/plain",
                        "text": text,
                    },
                }),
                Prepared::Image { mime, b64 } => serde_json::json!({
                    "type": "image", "mimeType": mime, "data": b64,
                }),
            })
            .collect(),
    })
}

pub fn file_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Small data-URL preview for UI chips.
pub fn thumbnail(att: &Attachment) -> Result<String> {
    if att.kind != "image" {
        bail!("not an image");
    }
    let bytes = std::fs::read(&att.path)?;
    let img = image::load_from_memory(&bytes)?.thumbnail(256, 256);
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(format!("data:image/png;base64,{}", B64.encode(&out)))
}

/// The prepared (resized, cached) image as a data URL, for full-size viewing.
pub fn image_data_url(data_dir: &Path, att: &Attachment) -> Result<String> {
    if att.kind != "image" {
        bail!("not an image");
    }
    match prepare(data_dir, att)?.into_iter().next() {
        Some(Prepared::Image { mime, b64 }) => Ok(format!("data:{mime};base64,{b64}")),
        _ => bail!("image could not be prepared"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_text_and_binary() {
        assert_eq!(classify("a.csv", b"a,b\n1,2\n").unwrap().0, "text");
        assert_eq!(classify("notes.md", "# héllo".as_bytes()).unwrap().1, "text/markdown");
        assert_eq!(classify("Makefile", b"all:\n\ttrue\n").unwrap().0, "text");
        assert!(classify("blob.bin", &[0u8, 159, 146, 150, 0, 1, 2]).is_err());
    }

    #[test]
    fn classifies_png_and_pdf() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(classify("x.png", &png).unwrap(), ("image", "image/png".into()));
        assert_eq!(classify("doc.pdf", b"%PDF-1.4\n").unwrap().0, "pdf");
    }

    #[test]
    fn downscales_large_images() {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4000, 1000)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let Prepared::Image { b64, .. } = prepare_image(&png, "image/png").unwrap() else {
            panic!("expected image")
        };
        let out = image::load_from_memory(&B64.decode(b64).unwrap()).unwrap();
        assert_eq!(out.width(), MAX_IMAGE_EDGE);
    }

    /// Minimal one-page PDF; `text` empty yields a page with no extractable text.
    fn pdf_fixture(text: &str) -> Vec<u8> {
        let stream = if text.is_empty() {
            "0 0 1 rg 20 20 100 100 re f".to_owned()
        } else {
            format!("BT /F1 18 Tf 30 100 Td ({text}) Tj ET")
        };
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offs {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn extracts_pdf_text() {
        let parts = prepare_pdf(pdf_fixture("Patchbay PDF fixture text")).unwrap();
        let Prepared::Text { text } = &parts[0] else { panic!() };
        assert!(text.contains("Patchbay PDF fixture text"), "{text}");
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn renders_textless_pdf_pages() {
        let parts = prepare_pdf(pdf_fixture("")).unwrap();
        assert!(
            parts.iter().any(|p| matches!(p, Prepared::Image { .. })),
            "{parts:?}"
        );
    }

    #[test]
    fn wraps_text_with_name() {
        let att = Attachment {
            id: "1".into(),
            sha256: "s".into(),
            name: "a.txt".into(),
            mime: "text/plain".into(),
            kind: "text".into(),
            size: 2,
            path: String::new(),
        };
        let parts = rename_text(vec![Prepared::Text { text: "hi\n".into() }], &att);
        let Prepared::Text { text } = &parts[0] else { panic!() };
        assert_eq!(text, "<file name=\"a.txt\" type=\"text/plain\">\nhi\n</file>");
    }
}
