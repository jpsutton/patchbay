//! Turn a web page into the text a model needs: the main content as Markdown, without the
//! navigation, banners, sidebars, footers, scripts and other furniture around it.

use std::collections::HashSet;

use regex::Regex;
use scraper::{ElementRef, Html, Selector};

/// Tags that never carry readable content.
const DROP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed", "form", "input", "button",
    "select", "textarea", "label", "link", "meta", "dialog", "audio", "video", "source", "picture", "img", "map",
];
/// Page furniture, dropped from inside the main content.
const FURNITURE_TAGS: &[&str] = &["nav", "aside", "footer"];
const FURNITURE_ROLES: &[&str] = &["navigation", "banner", "contentinfo", "complementary", "search", "dialog", "alertdialog"];
/// Class or id words that mean furniture wherever they appear in a class name.
const STRONG: &[&str] = &[
    "cookie", "cookies", "consent", "gdpr", "adsbygoogle", "advert", "advertisement", "sponsored", "newsletter", "popup",
    "modal", "breadcrumb", "breadcrumbs", "sidebar", "navbar", "footer", "paywall", "skiplink",
];
/// Words that only count when they are the whole class or id (`class="share"`, not `class="share-price"`).
const WEAK: &[&str] = &[
    "nav", "menu", "header", "ad", "ads", "promo", "banner", "share", "social", "related", "recommended", "comments", "widget", "toolbar", "skip",
    "sr-only", "visually-hidden", "hidden",
];
/// Where the main content usually is, in order of trust.
const MAIN_SELECTORS: &[&str] = &[
    "main", "[role=main]", "article", "#content", "#main-content", "#main", ".post-content", ".entry-content", ".article-body",
    ".article-content", ".markdown-body", ".content", ".post", "#article",
];
/// Least text, in characters, that makes a candidate the main content.
const MIN_MAIN_CHARS: usize = 250;

pub struct Extracted {
    pub title: Option<String>,
    pub markdown: String,
}

fn text_len(el: ElementRef) -> usize {
    el.text().map(|t| t.trim().len()).sum()
}

fn link_len(el: ElementRef) -> usize {
    let a = Selector::parse("a").unwrap();
    el.select(&a).map(text_len).sum()
}

fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split_whitespace().map(str::to_ascii_lowercase)
}

/// Whether a class or id marks furniture.
fn furniture_by_name(el: &ElementRef) -> bool {
    let names = el.value().attr("class").into_iter().flat_map(tokens).chain(el.value().attr("id").into_iter().flat_map(tokens));
    for name in names {
        if WEAK.contains(&name.as_str()) || STRONG.contains(&name.as_str()) {
            return true;
        }
        if name.split(['-', '_']).any(|t| STRONG.contains(&t)) {
            return true;
        }
    }
    false
}

fn hidden(el: &ElementRef) -> bool {
    let v = el.value();
    v.attr("hidden").is_some()
        || v.attr("aria-hidden") == Some("true")
        || v.attr("style").is_some_and(|s| {
            let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            s.contains("display:none") || s.contains("visibility:hidden")
        })
}

/// The element most likely to hold the page's content.
fn find_root(html: &Html) -> Option<ElementRef<'_>> {
    for css in MAIN_SELECTORS {
        let Ok(sel) = Selector::parse(css) else { continue };
        let found: Vec<ElementRef> = html.select(&sel).collect();
        // Several articles are a listing, not one article: look further.
        if *css == "article" && found.len() > 1 {
            continue;
        }
        if let Some(best) = found.into_iter().max_by_key(|e| text_len(*e))
            && text_len(best) >= MIN_MAIN_CHARS
        {
            return Some(best);
        }
    }
    // No landmark: the block with the most prose and the fewest links.
    let blocks = Selector::parse("div, section, article, td").unwrap();
    let prose = Selector::parse("p, pre, li, blockquote, h1, h2, h3, h4, h5, h6").unwrap();
    html.select(&blocks)
        .map(|b| {
            let text: usize = b.select(&prose).map(text_len).sum();
            let links = link_len(b);
            let density = if text == 0 { 1.0 } else { (links as f64 / text as f64).min(1.0) };
            (b, (text as f64 * (1.0 - density)) as usize)
        })
        .filter(|(_, score)| *score >= MIN_MAIN_CHARS)
        .max_by_key(|(_, score)| *score)
        .map(|(b, _)| b)
        .or_else(|| html.select(&Selector::parse("body").unwrap()).next())
}

/// The page as Markdown. `base` makes relative links absolute.
pub fn extract(raw_html: &str, base: Option<&reqwest::Url>) -> Extracted {
    let mut html = Html::parse_document(raw_html);
    let title = html
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|t| t.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    let Some(root_id) = find_root(&html).map(|r| r.id()) else {
        return Extracted { title, markdown: String::new() };
    };
    let root = ElementRef::wrap(html.tree.get(root_id).unwrap()).unwrap();
    let root_chars = text_len(root);

    let mut doomed = Vec::new();
    let mut seen: HashSet<_> = HashSet::new();
    for node in root.descendants().skip(1) {
        let Some(el) = ElementRef::wrap(node) else { continue };
        let name = el.value().name();
        let mut drop = DROP_TAGS.contains(&name) || hidden(&el);
        if !drop && FURNITURE_TAGS.contains(&name) {
            drop = true;
        }
        if !drop && name == "header" {
            // A page header is furniture; an article's header holds its title.
            drop = !el.select(&Selector::parse("h1, h2").unwrap()).next().is_some();
        }
        if !drop && el.value().attr("role").is_some_and(|r| FURNITURE_ROLES.contains(&r)) {
            drop = true;
        }
        if !drop && !matches!(name, "body" | "html" | "main" | "article" | "p" | "pre" | "code" | "table" | "h1" | "h2" | "h3") && furniture_by_name(&el) {
            drop = true;
        }
        if !drop && matches!(name, "ul" | "ol" | "div" | "section" | "p" | "table") {
            // A short run of nothing but links is a menu.
            let (text, links) = (text_len(el), link_len(el));
            let anchors = el.select(&Selector::parse("a").unwrap()).count();
            drop = anchors >= 3 && text > 0 && text < 400 && links * 10 >= text * 9;
        }
        // A false alarm that would take half the page is not furniture.
        if drop && root_chars > 500 && text_len(el) * 2 > root_chars && !DROP_TAGS.contains(&name) {
            drop = false;
        }
        if drop && seen.insert(el.id()) {
            doomed.push(el.id());
        }
    }
    for id in doomed {
        if let Some(mut n) = html.tree.get_mut(id) {
            n.detach();
        }
    }
    let root = ElementRef::wrap(html.tree.get(root_id).unwrap()).unwrap();
    let converter = htmd::HtmlToMarkdown::builder().skip_tags(vec!["script", "style", "head"]).build();
    let markdown = converter.convert(&root.html()).unwrap_or_else(|_| root.text().collect::<Vec<_>>().join(" "));
    Extracted { title, markdown: tidy(&markdown, base) }
}

/// Collapse the blank runs conversion leaves, drop empty links and make links absolute.
fn tidy(md: &str, base: Option<&reqwest::Url>) -> String {
    let empty_link = Regex::new(r"\[\s*\]\([^)]*\)").unwrap();
    let invisible = Regex::new("[\u{200b}\u{200c}\u{200d}\u{feff}]").unwrap();
    let link = Regex::new(r"\]\(([^)\s]+)").unwrap();
    let md = invisible.replace_all(md, "");
    // A `javascript:` link does nothing outside a browser: keep its text, lose the link.
    let js_link = Regex::new(r"(?i)\[([^\]]*)\]\(\s*javascript:[^)]*\)").unwrap();
    let md = js_link.replace_all(&md, "$1");
    let md = empty_link.replace_all(&md, "");
    let md = match base {
        Some(base) => link
            .replace_all(&md, |c: &regex::Captures| {
                let target = &c[1];
                if target.starts_with('#') || target.starts_with("mailto:") || target.starts_with("data:") || target.starts_with("javascript:") {
                    return c[0].to_owned();
                }
                match base.join(target) {
                    Ok(u) => format!("]({u}"),
                    Err(_) => c[0].to_owned(),
                }
            })
            .into_owned(),
        None => md.into_owned(),
    };
    let mut out = String::new();
    let mut blank = 0;
    for line in md.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = r##"<!doctype html><html><head><title>  Rust 1.99 released </title>
<script>window.tracker = "SCRIPT-JUNK";</script><style>.x{color:red}</style></head><body>
<header class="site-header"><a href="/">Home</a> <a href="/blog">Blog</a> <a href="/about">About</a> <a href="/jobs">Jobs</a></header>
<nav><ul><li><a href="/a">NAV-JUNK one</a></li><li><a href="/b">two</a></li></ul></nav>
<div class="cookie-banner">COOKIE-JUNK We use cookies. <button>Accept</button></div>
<div class="layout"><aside class="sidebar">SIDEBAR-JUNK <a href="/x">x</a></aside>
<main><article><header><h1>Rust 1.99 released</h1></header>
<p>The Rust team is happy to announce a new version of Rust, 1.99.0. This release brings several improvements to the compiler and the standard library that make everyday code faster to build and easier to write.</p>
<p>Read the <a href="/docs/notes">full notes</a> for every detail, including <code>let</code> chains and a faster linker. See <a href="https://example.org/ext">the guide</a> too, and a <a href="#top">jump link</a>.</p>
<p>Thanks to everyone who contributed to this release, from bug reports to patches to documentation fixes.</p>
<div class="share">SHARE-JUNK</div><section class="related"><h2>Related</h2>RELATED-JUNK</section>
</article></main></div>
<footer>FOOTER-JUNK © 2026 <a href="/privacy">Privacy</a></footer>
<script>var late = "LATE-SCRIPT";</script></body></html>"##;

    #[test]
    fn keeps_the_article_and_drops_the_furniture() {
        let base = reqwest::Url::parse("https://blog.example.com/posts/1").unwrap();
        let e = extract(ARTICLE, Some(&base));
        let md = &e.markdown;
        assert_eq!(e.title.as_deref(), Some("Rust 1.99 released"));
        assert!(md.contains("# Rust 1.99 released"), "{md}");
        assert!(md.contains("happy to announce a new version of Rust, 1.99.0"), "{md}");
        assert!(md.contains("`let` chains"), "{md}");
        for junk in ["SCRIPT-JUNK", "NAV-JUNK", "COOKIE-JUNK", "SIDEBAR-JUNK", "SHARE-JUNK", "RELATED-JUNK", "FOOTER-JUNK", "LATE-SCRIPT", "Accept"] {
            assert!(!md.contains(junk), "{junk} survived:\n{md}");
        }
        // Links resolve against the page; fragments stay.
        assert!(md.contains("(https://blog.example.com/docs/notes)"), "{md}");
        assert!(md.contains("(https://example.org/ext)") && md.contains("(#top)"), "{md}");
    }

    #[test]
    fn finds_content_without_landmarks_by_prose_and_link_density() {
        let prose = "This paragraph is the real content of the page and it is long enough to count as prose on its own merit. ".repeat(4);
        let html = format!(
            r#"<html><body><div id="menu"><a href="/1">One</a><a href="/2">Two</a><a href="/3">Three</a><a href="/4">Four</a></div>
<div class="wrap"><div class="links"><ul>{}</ul></div><div class="story"><h1>The Story</h1><p>{prose}</p><p>{prose}</p></div></div>
<div class="footer-links">FOOT-JUNK</div></body></html>"#,
            (1..40).map(|i| format!("<li><a href=\"/l{i}\">Link number {i} to elsewhere</a></li>")).collect::<String>()
        );
        let md = extract(&html, None).markdown;
        assert!(md.contains("# The Story") && md.contains("real content of the page"), "{md}");
        assert!(!md.contains("Link number") && !md.contains("FOOT-JUNK"), "{md}");
    }

    #[test]
    fn a_listing_of_articles_is_not_mistaken_for_one() {
        let one = |n: u32| format!("<article><h2>Post {n}</h2><p>{}</p></article>", format!("Summary of post {n}. ").repeat(15));
        let html = format!("<html><body><nav><a href='/'>NAV-JUNK</a></nav><div id=\"posts\">{}{}{}</div></body></html>", one(1), one(2), one(3));
        let md = extract(&html, None).markdown;
        assert!(md.contains("Post 1") && md.contains("Post 2") && md.contains("Post 3"), "{md}");
        assert!(!md.contains("NAV-JUNK"), "{md}");
    }

    #[test]
    fn plain_and_tiny_pages_survive() {
        let md = extract("<html><body><p>Just a short note.</p></body></html>", None).markdown;
        assert_eq!(md, "Just a short note.");
        assert_eq!(extract("", None).markdown, "");
    }

    #[test]
    fn javascript_links_lose_their_target() {
        let html = format!("<html><body><main><p>{} <a href=\"javascript:void(0)\">Click me</a> <a onclick=\"evil()\" style=\"color:red\" href=\"/ok\">fine</a></p></main></body></html>", "Plenty of ordinary words to make this the main content of the page. ".repeat(6));
        let md = extract(&html, Some(&reqwest::Url::parse("https://e.com/").unwrap())).markdown;
        assert!(md.contains("Click me") && !md.contains("javascript:") && !md.contains("evil()") && !md.contains("color:red"), "{md}");
        assert!(md.contains("[fine](https://e.com/ok)"), "{md}");
    }

    #[test]
    fn hidden_text_is_dropped() {
        let html = format!(
            "<html><body><main><p>{}</p><p style=\"display: none\">HIDDEN-JUNK</p><p aria-hidden=\"true\">ARIA-JUNK</p></main></body></html>",
            "Visible text that is long enough to be the main content of this page. ".repeat(6)
        );
        let md = extract(&html, None).markdown;
        assert!(md.contains("Visible text") && !md.contains("HIDDEN-JUNK") && !md.contains("ARIA-JUNK"), "{md}");
    }
}
