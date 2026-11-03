//! EPUB -> title, author and Markdown. Pure: bytes in, text out; storage and hashing live in `service`.
//!
//! An EPUB is a zip with `META-INF/container.xml` pointing at an OPF package file. The OPF names the metadata, a
//! manifest of files and the spine (reading order). Each linear spine document goes through the same htmd
//! conversion as web pages (images dropped). Chapter titles come from the EPUB3 nav document or the EPUB2 NCX when
//! they map to a spine file, else from the section's first heading.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use zip::ZipArchive;

/// Total uncompressed size the archive may declare, and the most we will read from it.
const MAX_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
/// Most entries an archive may hold.
const MAX_ENTRIES: usize = 20_000;
/// Most we read from a single entry (XML or XHTML text).
const MAX_ENTRY: u64 = 64 * 1024 * 1024;

/// Why the bytes are not a usable EPUB.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EpubError(pub String);

type Result<T> = std::result::Result<T, EpubError>;

fn err(m: impl Into<String>) -> EpubError {
    EpubError(m.into())
}

/// What `parse` extracts. `markdown` is the whole book in reading order.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub title: String,
    pub author: Option<String>,
    pub markdown: String,
}

/// Zip access with a shared read budget, so a bomb cannot make us inflate more than `MAX_UNCOMPRESSED` in total.
struct Archive<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    budget: u64,
}

impl Archive<'_> {
    /// The entry's bytes, `None` if it is not in the archive.
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let Ok(file) = self.zip.by_name(path) else {
            return Ok(None);
        };
        let limit = MAX_ENTRY.min(self.budget);
        let mut out = Vec::new();
        file.take(limit + 1).read_to_end(&mut out).map_err(|e| err(format!("could not read {path}: {e}")))?;
        if out.len() as u64 > limit {
            return Err(err(format!("{path} is too large to unpack")));
        }
        self.budget -= out.len() as u64;
        Ok(Some(out))
    }

    fn read_text(&mut self, path: &str) -> Result<Option<String>> {
        Ok(self.read(path)?.map(|b| String::from_utf8_lossy(&b).trim_start_matches('\u{feff}').to_string()))
    }
}

/// Parse an EPUB. `filename` (without directories) is the title of last resort.
pub fn parse(bytes: &[u8], filename: Option<&str>) -> Result<Book> {
    parse_limited(bytes, filename, MAX_UNCOMPRESSED)
}

fn parse_limited(bytes: &[u8], filename: Option<&str>, max_uncompressed: u64) -> Result<Book> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(|_| err("not an EPUB: not a zip file"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(err("EPUB has too many files"));
    }
    let mut declared = 0u64;
    for i in 0..zip.len() {
        declared = declared.saturating_add(zip.by_index_raw(i).map(|f| f.size()).unwrap_or(0));
    }
    if declared > max_uncompressed {
        return Err(err("EPUB is too large when unpacked"));
    }
    let mut archive = Archive { zip, budget: max_uncompressed };

    let container = archive.read_text("META-INF/container.xml")?.ok_or_else(|| err("not an EPUB: no META-INF/container.xml"))?;
    let container = roxmltree::Document::parse(&container).map_err(|_| err("not an EPUB: container.xml is not valid XML"))?;
    let opf_path = container
        .descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .and_then(|n| n.attribute("full-path"))
        .map(|p| resolve("", p))
        .ok_or_else(|| err("not an EPUB: container.xml names no package file"))?;
    let opf_xml = archive.read_text(&opf_path)?.ok_or_else(|| err("not an EPUB: the package file is missing"))?;
    let opf = roxmltree::Document::parse(&opf_xml).map_err(|_| err("not an EPUB: the package file is not valid XML"))?;
    let base = dir_of(&opf_path);

    let meta = |name: &str| -> Vec<String> {
        opf.descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == name && n.parent().is_some_and(|p| p.has_tag_name("metadata")))
            .map(|n| collapse(&text_of(n)))
            .filter(|t| !t.is_empty())
            .collect()
    };
    let title = meta("title").into_iter().next();
    let creators = meta("creator");
    let author = (!creators.is_empty()).then(|| creators.join(", "));

    struct Item {
        href: String,
        media_type: String,
        properties: String,
    }
    let manifest: HashMap<&str, Item> = opf
        .descendants()
        .filter(|n| n.has_tag_name("item"))
        .filter_map(|n| {
            let item = Item {
                href: resolve(&base, n.attribute("href")?),
                media_type: n.attribute("media-type").unwrap_or("").to_ascii_lowercase(),
                properties: n.attribute("properties").unwrap_or("").to_string(),
            };
            Some((n.attribute("id")?, item))
        })
        .collect();
    let spine = opf.descendants().find(|n| n.has_tag_name("spine")).ok_or_else(|| err("not an EPUB: the package has no spine"))?;

    // Chapter titles: the EPUB3 nav first, then the NCX; the first title seen for a file wins.
    let mut titles: HashMap<String, String> = HashMap::new();
    let nav = manifest.values().find(|i| i.properties.split_whitespace().any(|p| p == "nav"));
    let ncx = spine
        .attribute("toc")
        .and_then(|id| manifest.get(id))
        .or_else(|| manifest.values().find(|i| i.media_type == "application/x-dtbncx+xml"));
    if let Some(item) = nav
        && let Some(xml) = archive.read_text(&item.href)?
    {
        titles_from_nav(&xml, &dir_of(&item.href), &mut titles);
    }
    if let Some(item) = ncx
        && let Some(xml) = archive.read_text(&item.href)?
    {
        titles_from_ncx(&xml, &dir_of(&item.href), &mut titles);
    }

    let converter =
        htmd::HtmlToMarkdown::builder().skip_tags(vec!["head", "img", "picture", "svg", "figure", "script", "style"]).build();
    let mut sections = Vec::new();
    for itemref in spine.children().filter(|n| n.has_tag_name("itemref")) {
        if itemref.attribute("linear") == Some("no") {
            continue;
        }
        let Some(item) = itemref.attribute("idref").and_then(|id| manifest.get(id)) else {
            continue;
        };
        let is_html = matches!(item.media_type.as_str(), "application/xhtml+xml" | "text/html");
        if !is_html || item.properties.split_whitespace().any(|p| p == "nav") {
            continue;
        }
        let Some(html) = archive.read_text(&item.href)? else {
            continue;
        };
        let body = converter.convert(&expand_self_closing(&html)).map_err(|e| err(format!("could not read {}: {e}", item.href)))?;
        let body = crate::extract::strip_empty_links(&body).trim().to_string();
        if body.is_empty() {
            continue;
        }
        sections.push(section(titles.get(&item.href).cloned(), &body));
    }
    let markdown = sections.join("\n\n");
    if markdown.trim().is_empty() {
        return Err(err("this EPUB has no readable text"));
    }

    let title = title
        .or_else(|| {
            let stem = filename?.rsplit(['/', '\\']).next()?;
            let stem = stem.strip_suffix(".epub").or_else(|| stem.strip_suffix(".EPUB")).unwrap_or(stem).trim();
            (!stem.is_empty()).then(|| stem.to_string())
        })
        .unwrap_or_else(|| "Untitled book".into());
    Ok(Book { title, author, markdown })
}

/// `## <title>` then the body. The title is the nav/NCX one, else the body's first heading. A leading heading in
/// the body that repeats the title is dropped, so the chapter name appears once.
fn section(toc_title: Option<String>, body: &str) -> String {
    let first_heading = |s: &str| -> Option<String> {
        let line = s.lines().find(|l| !l.trim().is_empty())?.trim();
        let text = line.strip_prefix('#')?.trim_start_matches('#').trim();
        (!text.is_empty()).then(|| collapse(text))
    };
    let lead = first_heading(body);
    let title = toc_title.or_else(|| lead.clone());
    match title {
        Some(title) => {
            let rest = match &lead {
                Some(l) if l.eq_ignore_ascii_case(&title) => body.split_once('\n').map_or("", |(_, r)| r).trim(),
                _ => body,
            };
            format!("## {title}\n\n{rest}").trim_end().to_string()
        }
        None => body.to_string(),
    }
}

/// Titles from an EPUB3 navigation document: every link inside the `toc` nav.
fn titles_from_nav(xml: &str, base: &str, out: &mut HashMap<String, String>) {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return;
    };
    let is_toc = |n: &roxmltree::Node| {
        n.has_tag_name("nav") && n.attributes().any(|a| a.name() == "type" && a.value().split_whitespace().any(|t| t == "toc"))
    };
    let Some(nav) = doc.descendants().find(is_toc) else {
        return;
    };
    for a in nav.descendants().filter(|n| n.has_tag_name("a")) {
        let (Some(href), title) = (a.attribute("href"), collapse(&text_of(a))) else {
            continue;
        };
        if !title.is_empty() {
            out.entry(resolve(base, href)).or_insert(title);
        }
    }
}

/// Titles from an EPUB2 NCX: each `navPoint`'s label and content target.
fn titles_from_ncx(xml: &str, base: &str, out: &mut HashMap<String, String>) {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return;
    };
    for point in doc.descendants().filter(|n| n.has_tag_name("navPoint")) {
        let label = point.children().find(|n| n.has_tag_name("navLabel")).map(|l| collapse(&text_of(l)));
        let src = point.children().find(|n| n.has_tag_name("content")).and_then(|c| c.attribute("src"));
        if let (Some(label), Some(src)) = (label, src)
            && !label.is_empty()
        {
            out.entry(resolve(base, src)).or_insert(label);
        }
    }
}

fn text_of(node: roxmltree::Node) -> String {
    node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dir_of(path: &str) -> String {
    path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()
}

/// Zip path of `href` (percent-decoded, fragment and query dropped) relative to the directory `base`.
fn resolve(base: &str, href: &str) -> String {
    let href = href.split(['#', '?']).next().unwrap_or("");
    let href = percent_decode(href);
    let mut parts: Vec<&str> = if href.starts_with('/') { Vec::new() } else { base.split('/').filter(|p| !p.is_empty()).collect() };
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `%XX` escapes decoded; malformed escapes are kept as they are. Also used for the `X-Filename` header.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const VOID: [&str; 14] =
    ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// XHTML writes empty elements as `<a id="x"/>`, which an HTML parser reads as an open tag that swallows the rest of
/// the chapter. Rewrite every self-closing non-void tag as `<a id="x"></a>`.
fn expand_self_closing(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let name_len = rest[1..].find(|c: char| !c.is_ascii_alphanumeric() && c != ':').unwrap_or(rest.len() - 1);
        if name_len == 0 || !rest.as_bytes()[1].is_ascii_alphabetic() {
            out.push('<');
            rest = &rest[1..];
            continue;
        }
        // End of the tag: the first `>` outside a quoted attribute value.
        let mut quote = None;
        let mut end = None;
        for (i, c) in rest.char_indices().skip(1 + name_len) {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '>') => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            break;
        };
        let name = &rest[1..1 + name_len];
        let tag = &rest[..=end];
        if tag.ends_with("/>") && !VOID.contains(&name.to_ascii_lowercase().as_str()) {
            out.push_str(tag[..tag.len() - 2].trim_end());
            out.push_str("></");
            out.push_str(name);
            out.push('>');
        } else {
            out.push_str(tag);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    fn xhtml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>ignored</title></head><body>{body}</body></html>"#
        )
    }

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in files {
            zip.start_file(*name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// EPUB3 with a nav: chapter 1 titled by the nav, chapter 2 not in the nav (first heading), a non-linear
    /// footnote page and a cover that is only an image.
    fn epub3() -> Vec<u8> {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>The Test Book</dc:title><dc:creator>Ada Lovelace</dc:creator><dc:creator>Charles Babbage</dc:creator>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="cover" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="text/ch%201.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="notes" href="notes.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="cover"/><itemref idref="c1"/><itemref idref="c2"/><itemref idref="notes" linear="no"/></spine>
</package>"#;
        let nav = xhtml(
            r#"<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="toc"><ol>
              <li><a href="text/ch%201.xhtml#start">Chapter One</a></li></ol></nav>"#,
        );
        let cover = xhtml(r#"<img src="cover.png" alt="cover"/>"#);
        let ch1 = xhtml(r#"<h1>Chapter One</h1><p>It was a <em>bright</em> day.<br/></p><a id="anchor"/><p>Second paragraph.</p><img src="x.png"/>"#);
        let ch2 = xhtml(r#"<h2>The Second Chapter</h2><p>Nothing happened.</p>"#);
        let notes = xhtml("<p>Footnote text.</p>");
        zip_of(&[
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", opf),
            ("OEBPS/nav.xhtml", &nav),
            ("OEBPS/cover.xhtml", &cover),
            ("OEBPS/text/ch 1.xhtml", &ch1),
            ("OEBPS/text/ch2.xhtml", &ch2),
            ("OEBPS/notes.xhtml", &notes),
        ])
    }

    #[test]
    fn reads_metadata_chapters_and_drops_images_and_non_linear_items() {
        let book = parse(&epub3(), Some("whatever.epub")).unwrap();
        assert_eq!(book.title, "The Test Book");
        assert_eq!(book.author.as_deref(), Some("Ada Lovelace, Charles Babbage"));
        assert!(book.markdown.starts_with("## Chapter One\n\n"), "{}", book.markdown);
        // The nav title replaces the body's own identical heading instead of repeating it.
        assert_eq!(book.markdown.matches("Chapter One").count(), 1, "{}", book.markdown);
        assert!(book.markdown.contains("It was a *bright* day."), "{}", book.markdown);
        // The self-closing anchor did not swallow the second paragraph.
        assert!(book.markdown.contains("Second paragraph."), "{}", book.markdown);
        // Not in the nav: the first heading names the section.
        assert!(book.markdown.contains("## The Second Chapter\n\nNothing happened."), "{}", book.markdown);
        assert!(!book.markdown.contains("Footnote"));
        assert!(!book.markdown.contains("png"));
        assert!(!book.markdown.contains("ignored"), "head text leaked: {}", book.markdown);
    }

    #[test]
    fn ncx_titles_and_filename_fallback() {
        let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"></metadata>
  <manifest><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="a" href="a.html" media-type="application/xhtml+xml"/></manifest>
  <spine toc="ncx"><itemref idref="a"/></spine></package>"#;
        let ncx = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap>
  <navPoint id="p1"><navLabel><text>Opening</text></navLabel><content src="a.html"/></navPoint></navMap></ncx>"#;
        let bytes = zip_of(&[
            ("META-INF/container.xml", &CONTAINER.replace("OEBPS/", "")),
            ("content.opf", opf),
            ("toc.ncx", ncx),
            ("a.html", &xhtml("<p>Hello there.</p>")),
        ]);
        let book = parse(&bytes, Some("/tmp/My Story.epub")).unwrap();
        assert_eq!(book.title, "My Story");
        assert_eq!(book.author, None);
        assert_eq!(book.markdown, "## Opening\n\nHello there.");
    }

    #[test]
    fn rejects_things_that_are_not_epubs() {
        assert!(parse(b"just some text", None).is_err());
        assert!(parse(&zip_of(&[("a.txt", "hi")]), None).unwrap_err().0.contains("container.xml"));
        let no_opf = zip_of(&[("META-INF/container.xml", CONTAINER)]);
        assert!(parse(&no_opf, None).unwrap_err().0.contains("package file"));
        let image_only = zip_of(&[
            ("META-INF/container.xml", &CONTAINER.replace("OEBPS/", "")),
            (
                "content.opf",
                r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="a"/></spine></package>"#,
            ),
            ("a.xhtml", &xhtml(r#"<img src="x.png"/>"#)),
        ]);
        assert!(parse(&image_only, None).unwrap_err().0.contains("no readable text"));
    }

    #[test]
    fn refuses_a_zip_that_unpacks_too_large() {
        // A highly compressible entry that declares more than the cap: rejected from the directory, before inflating.
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("big.txt", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
        zip.write_all(&vec![b'a'; 2 * 1024 * 1024]).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert!(bytes.len() < 100 * 1024);
        assert!(parse_limited(&bytes, None, 1024 * 1024).unwrap_err().0.contains("too large"));
        // The same archive is fine under the real cap (it is just not an EPUB).
        assert!(parse(&bytes, None).unwrap_err().0.contains("container.xml"));
    }

    #[test]
    fn path_helpers() {
        assert_eq!(resolve("OEBPS/text", "../img/a%20b.png#x"), "OEBPS/img/a b.png");
        assert_eq!(resolve("OEBPS", "/abs/c.xhtml"), "abs/c.xhtml");
        assert_eq!(percent_decode("My%20Book%E2%80%99s.epub"), "My Book\u{2019}s.epub");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(expand_self_closing(r#"<p>a<br/><a id="x" title="/>"/>b</p>"#), r#"<p>a<br/><a id="x" title="/>"></a>b</p>"#);
    }
}
