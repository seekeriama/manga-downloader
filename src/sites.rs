//! Nhận diện trang web, liệt kê chương và lấy danh sách ảnh của từng chương.
//!
//! Hỗ trợ:
//!   - Trang dạng truyenggvn.com: /truyen-tranh/<tên>-<id>  (chương: <tên>-<id>-chap-N.html)
//!   - Trang Blogger (vd. pokemonspecial.com): liệt kê bài viết qua feed JSON của Blogger
//!
//! Từ chối:
//!   - cuutruyen.net (ảnh bị xáo trộn bằng DRM)

use crate::i18n::t;
use crate::tf;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;
use std::thread::sleep;
use std::time::Duration;

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

#[derive(Clone, Debug)]
pub enum Kind {
    Blogger,
    Truyenggvn { sid: String },
}

#[derive(Clone, Debug)]
pub struct Chapter {
    /// Tên hiển thị, vd. "Chương 1.1" hoặc tiêu đề bài Blogger
    pub title: String,
    pub url: String,
    /// Nhóm (nhãn Blogger), có thể rỗng
    pub group: String,
    /// Số chương dạng "0001" / "0001.1" (rỗng nếu trang không có số chương)
    pub number: String,
}

#[derive(Clone, Debug)]
pub struct Series {
    pub title: String,
    pub site: String,
    pub kind: Kind,
    pub referer: String,
    pub chapters: Vec<Chapter>,
}

pub fn client() -> Client {
    Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .expect("không tạo được HTTP client")
}

pub enum GetErr {
    NotFound,
    Other(String),
}

impl From<GetErr> for String {
    fn from(e: GetErr) -> String {
        match e {
            GetErr::NotFound => t("không tồn tại (404)", "not found (404)").into(),
            GetErr::Other(s) => s,
        }
    }
}

/// GET có thử lại (2s, 4s, 8s). 404 trả về ngay.
pub fn get_bytes(http: &Client, url: &str, referer: Option<&str>) -> Result<Vec<u8>, GetErr> {
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            sleep(Duration::from_secs(1 << attempt));
        }
        let mut req = http.get(url);
        if let Some(r) = referer {
            req = req.header("Referer", r);
        }
        match req.send() {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => return Err(GetErr::NotFound),
            Ok(resp) if resp.status().is_success() => match resp.bytes() {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last = tf!("lỗi đọc dữ liệu: {e}", "read error: {e}"),
            },
            Ok(resp) => last = format!("HTTP {}", resp.status()),
            Err(e) => last = tf!("lỗi kết nối: {e}", "connection error: {e}"),
        }
    }
    Err(GetErr::Other(format!("{last} ({url})")))
}

pub fn get_text(http: &Client, url: &str, referer: Option<&str>) -> Result<String, GetErr> {
    get_bytes(http, url, referer).map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ------------------------------------------------------------------ nhận diện

static TRUYENGGVN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<series>/truyen-tranh/[^/]+?-(?P<sid>\d+))(?:-chap-[^/]+?)?(?:\.html)?/?$").unwrap()
});

/// Kiểm tra URL: trả về thông tin truyện + danh sách chương, hoặc lý do không tải được.
pub fn inspect(http: &Client, input: &str) -> Result<Series, String> {
    let input = input.trim();
    let url = Url::parse(input)
        .or_else(|_| Url::parse(&format!("https://{input}")))
        .map_err(|_| {
            t(
                "Địa chỉ không hợp lệ. Hãy dán đầy đủ, vd. https://truyenggvn.com/truyen-tranh/...",
                "Invalid address. Paste the full link, e.g. https://truyenggvn.com/truyen-tranh/...",
            )
            .to_string()
        })?;
    let host = url.host_str().unwrap_or("").to_lowercase();

    if host.contains("cuutruyen") {
        return Err(t(
            "Không hỗ trợ cuutruyen.net: ảnh trên trang này bị xáo trộn bằng cơ chế bảo vệ bản quyền \
             (DRM), phần mềm không giải mã lớp bảo vệ này.",
            "cuutruyen.net is not supported: its images are scrambled with copy protection (DRM), \
             which this software does not decode.",
        )
        .into());
    }

    if let Some(c) = TRUYENGGVN_RE.captures(url.path()) {
        let origin = origin(&url);
        let series_url = format!("{origin}{}", &c["series"]);
        return inspect_truyenggvn(http, &origin, &series_url, &c["sid"]);
    }

    match inspect_blogger(http, &url) {
        Ok(s) => Ok(s),
        Err(BloggerErr::NotBlogger) => Err(tf!(
            "Chưa hỗ trợ trang {host}.\nHiện hỗ trợ: các trang dạng truyenggvn.com (/truyen-tranh/...) \
             và các trang chạy trên Blogger (vd. pokemonspecial.com).",
            "{host} is not supported yet.\nSupported: sites like truyenggvn.com (/truyen-tranh/...) \
             and sites running on Blogger (e.g. pokemonspecial.com)."
        )),
        Err(BloggerErr::Failed(e)) => Err(e),
    }
}

fn origin(url: &Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(p) = url.port() {
        s.push_str(&format!(":{p}"));
    }
    s
}

// ------------------------------------------------------------------ truyenggvn

/// "1" -> (1,0); "1-2" -> (1,2); "12-fix" -> (12,0)
fn chap_key(slug: &str) -> (u64, u64) {
    let mut parts = slug.split(['-', '.']);
    let a = parts.next().and_then(|p| p.parse().ok()).unwrap_or(u64::MAX);
    let b = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (a, b)
}

fn inspect_truyenggvn(http: &Client, origin: &str, series_url: &str, sid: &str) -> Result<Series, String> {
    let html = get_text(http, series_url, Some(&format!("{origin}/"))).map_err(|e| match e {
        GetErr::NotFound => {
            t("Trang truyện không tồn tại (404). Kiểm tra lại địa chỉ.", "Comic page not found (404). Check the address.")
                .to_string()
        }
        GetErr::Other(s) => tf!("Không mở được trang truyện: {s}", "Could not open the comic page: {s}"),
    })?;
    let doc = Html::parse_document(&html);

    let path = Url::parse(series_url).map(|u| u.path().to_string()).unwrap_or_default();
    let link_re = Regex::new(&format!(r"{}-chap-([0-9][0-9A-Za-z-]*)\.html", regex::escape(&path))).unwrap();
    let mut slugs: BTreeMap<(u64, u64), (String, String)> = BTreeMap::new();
    let a_sel = Selector::parse("a[href]").unwrap();
    for a in doc.select(&a_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if let Some(m) = link_re.captures(href) {
            let slug = m[1].to_string();
            let full = Url::parse(series_url)
                .and_then(|b| b.join(href))
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.to_string());
            slugs.entry(chap_key(&slug)).or_insert((slug, full));
        }
    }
    if slugs.is_empty() {
        return Err(t(
            "Mở được trang nhưng không tìm thấy danh sách chương (có thể trang đã đổi cấu trúc).",
            "The page opened but no chapter list was found (the site may have changed its layout).",
        )
        .into());
    }

    let title = first_text(&doc, "h1")
        .or_else(|| meta(&doc, "og:title"))
        .unwrap_or_else(|| "Truyện".into());

    let chapters = slugs
        .into_iter()
        .map(|((a, b), (_slug, url))| {
            let (number, title) = if b > 0 {
                (format!("{a:04}.{b}"), format!("Chương {a}.{b}"))
            } else if a == u64::MAX {
                (String::new(), format!("Chương {_slug}"))
            } else {
                (format!("{a:04}"), format!("Chương {a}"))
            };
            Chapter { title, url, group: String::new(), number }
        })
        .collect();

    Ok(Series {
        title,
        site: Url::parse(origin).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default(),
        kind: Kind::Truyenggvn { sid: sid.to_string() },
        referer: format!("{origin}/"),
        chapters,
    })
}

const LAZY_ATTRS: [&str; 5] = ["data-src", "data-original", "data-lazy-src", "data-url", "src"];

fn truyenggvn_images(html: &str, page_url: &str, sid: &str, slug: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let img_sel = Selector::parse("img").unwrap();
    let base = Url::parse(page_url).ok();
    let strict = Regex::new(&format!(r"/{sid}/{}/(\d+)\.[A-Za-z0-9]+(?:\?|$)", regex::escape(slug))).unwrap();
    let loose = Regex::new(&format!(r"/{sid}/[^/]+/(\d+)\.[A-Za-z0-9]+(?:\?|$)")).unwrap();

    for pat in [&strict, &loose] {
        let mut found: BTreeMap<u64, String> = BTreeMap::new();
        for img in doc.select(&img_sel) {
            for attr in LAZY_ATTRS {
                let v = img.value().attr(attr).unwrap_or("").trim();
                if v.is_empty() {
                    continue;
                }
                if let Some(m) = pat.captures(v) {
                    let n: u64 = m[1].parse().unwrap_or(0);
                    let full = base.as_ref().and_then(|b| b.join(v).ok()).map(|u| u.to_string()).unwrap_or(v.into());
                    found.entry(n).or_insert(full);
                    break;
                }
            }
        }
        if !found.is_empty() {
            return found.into_values().collect();
        }
    }

    // dự phòng: ảnh nằm trong script/JSON
    let text = html.replace("\\/", "/");
    let re = Regex::new(&format!(
        r#"https?:[^"'\s<>]*?/{sid}/{}/(\d+)\.[A-Za-z0-9]+(?:\?[^"'\s<>]*)?"#,
        regex::escape(slug)
    ))
    .unwrap();
    let mut found: BTreeMap<u64, String> = BTreeMap::new();
    for m in re.captures_iter(&text) {
        found.entry(m[1].parse().unwrap_or(0)).or_insert(m[0].to_string());
    }
    found.into_values().collect()
}

// ------------------------------------------------------------------ Blogger

enum BloggerErr {
    NotBlogger,
    Failed(String),
}

struct Post {
    title: String,
    url: String,
    published: String,
    labels: Vec<String>,
}

fn feed_url(origin: &str, label: Option<&str>) -> Url {
    let mut u = Url::parse(origin).unwrap();
    {
        let mut seg = u.path_segments_mut().unwrap();
        seg.clear().extend(["feeds", "posts", "summary"]);
        if let Some(l) = label {
            seg.extend(["-", l]);
        }
    }
    u
}

fn feed_json(http: &Client, mut u: Url, start: usize, max: usize) -> Result<Value, GetErr> {
    u.query_pairs_mut()
        .append_pair("alt", "json")
        .append_pair("max-results", &max.to_string())
        .append_pair("start-index", &start.to_string())
        .append_pair("orderby", "published");
    let body = get_text(http, u.as_str(), None)?;
    serde_json::from_str(&body).map_err(|_| GetErr::Other("feed: not JSON".into()))
}

fn feed_total(v: &Value) -> usize {
    v["feed"]["openSearch$totalResults"]["$t"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn fetch_feed(http: &Client, origin: &str, label: Option<&str>) -> Result<Vec<Post>, String> {
    let mut posts = Vec::new();
    let mut start = 1;
    loop {
        let v = feed_json(http, feed_url(origin, label), start, 150).map_err(String::from)?;
        let entries = v["feed"]["entry"].as_array().cloned().unwrap_or_default();
        for e in &entries {
            let Some(link) = e["link"].as_array().and_then(|ls| {
                ls.iter().find(|l| l["rel"] == "alternate").and_then(|l| l["href"].as_str())
            }) else {
                continue;
            };
            posts.push(Post {
                title: e["title"]["$t"].as_str().unwrap_or("").trim().to_string(),
                url: link.to_string(),
                published: e["published"]["$t"].as_str().unwrap_or("").to_string(),
                labels: e["category"]
                    .as_array()
                    .map(|cs| cs.iter().filter_map(|c| c["term"].as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            });
        }
        start += entries.len(); // Blogger có thể trả ít hơn số yêu cầu mỗi trang
        if entries.is_empty() || start > feed_total(&v) {
            break;
        }
        sleep(Duration::from_millis(300));
    }
    Ok(posts)
}

fn inspect_blogger(http: &Client, url: &Url) -> Result<Series, BloggerErr> {
    let origin = origin(url);
    let head = match feed_json(http, feed_url(&origin, None), 1, 0) {
        Ok(v) if v["feed"].is_object() => v,
        Ok(_) | Err(GetErr::NotFound) => return Err(BloggerErr::NotBlogger),
        Err(GetErr::Other(e)) if e.contains("JSON") => return Err(BloggerErr::NotBlogger),
        Err(GetErr::Other(e)) => return Err(BloggerErr::Failed(tf!("Không kết nối được: {e}", "Could not connect: {e}"))),
    };
    let blog_title = head["feed"]["title"]["$t"].as_str().unwrap_or("Truyện").trim().to_string();
    let total = feed_total(&head);

    // URL dạng /search/label/<nhãn> -> chỉ lấy nhãn đó
    let label_in_url = {
        let segs: Vec<String> = url
            .path_segments()
            .map(|s| s.map(urlencoding_decode).collect())
            .unwrap_or_default();
        segs.windows(3).find(|w| w[0] == "search" && w[1] == "label").map(|w| w[2].clone())
    };

    let fail = |e: String| BloggerErr::Failed(tf!("Lỗi khi đọc danh sách bài: {e}", "Error reading the post list: {e}"));
    let mut seen: HashMap<String, Post> = HashMap::new();
    if let Some(l) = &label_in_url {
        for p in fetch_feed(http, &origin, Some(l)).map_err(fail)? {
            seen.entry(p.url.clone()).or_insert(p);
        }
    } else {
        // gộp feed theo từng nhãn (ít bị cắt), rồi bổ sung bằng feed chung
        let labels: Vec<String> = head["feed"]["category"]
            .as_array()
            .map(|cs| cs.iter().filter_map(|c| c["term"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        for l in &labels {
            for p in fetch_feed(http, &origin, Some(l)).map_err(fail)? {
                seen.entry(p.url.clone()).or_insert(p);
            }
        }
        if seen.len() < total {
            for p in fetch_feed(http, &origin, None).map_err(fail)? {
                seen.entry(p.url.clone()).or_insert(p);
            }
        }
    }
    if seen.is_empty() {
        return Err(BloggerErr::Failed(t("Trang Blogger này không có bài viết nào.", "This Blogger site has no posts.").into()));
    }

    let mut posts: Vec<Post> = seen.into_values().collect();
    posts.sort_by(|a, b| a.published.cmp(&b.published).then(a.url.cmp(&b.url)));
    let chapters = posts
        .into_iter()
        .map(|p| Chapter {
            title: if p.title.is_empty() { p.url.clone() } else { p.title },
            url: p.url,
            group: p.labels.first().cloned().unwrap_or_default(),
            number: String::new(),
        })
        .collect();

    Ok(Series {
        title: label_in_url.map(|l| format!("{blog_title} - {l}")).unwrap_or(blog_title),
        site: url.host_str().unwrap_or("").to_string(),
        kind: Kind::Blogger,
        referer: format!("{origin}/"),
        chapters,
    })
}

fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(v) = s.get(i + 1..i + 3).ok_or(()).and_then(|h| u8::from_str_radix(h, 16).map_err(|_| ())) {
                out.push(v);
                i += 3;
                continue;
            }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const NAV_ALTS: [&str; 4] = ["chương cũ hơn", "chương mới hơn", "trang chủ", "back to top"];

fn is_blogger_img(u: &str) -> bool {
    u.contains("googleusercontent.com") || u.contains("bp.blogspot.com")
}

static BLOGGER_SIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/s\d+(-[a-z]+)?/").unwrap());

fn blogger_images(html: &str, page_url: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let base = Url::parse(page_url).ok();
    let body = [".post-body", ".entry-content", "article"]
        .iter()
        .find_map(|s| doc.select(&Selector::parse(s).unwrap()).next());
    let img_sel = Selector::parse("img").unwrap();
    let imgs: Vec<ElementRef> = match body {
        Some(b) => b.select(&img_sel).collect(),
        None => doc.select(&img_sel).collect(),
    };

    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for img in imgs {
        let alt = img.value().attr("alt").unwrap_or("").trim().to_lowercase();
        if NAV_ALTS.contains(&alt.as_str()) {
            continue;
        }
        let parent_href = img
            .parent()
            .and_then(ElementRef::wrap)
            .filter(|p| p.value().name() == "a")
            .and_then(|p| p.value().attr("href"))
            .filter(|h| is_blogger_img(h));
        let src = parent_href
            .or_else(|| img.value().attr("src"))
            .or_else(|| img.value().attr("data-src"))
            .unwrap_or("");
        if src.is_empty() || !is_blogger_img(src) {
            continue;
        }
        let full = base.as_ref().and_then(|b| b.join(src).ok()).map(|u| u.to_string()).unwrap_or(src.into());
        let full = BLOGGER_SIZE_RE.replace(&full, "/s0/").into_owned();
        if seen.insert(full.clone()) {
            out.push(full);
        }
    }
    out
}

// ------------------------------------------------------------------ chung

/// Danh sách URL ảnh của một chương, theo thứ tự.
pub fn chapter_images(http: &Client, series: &Series, ch: &Chapter) -> Result<Vec<String>, String> {
    let html = get_text(http, &ch.url, Some(&series.referer)).map_err(|e| match e {
        GetErr::NotFound => t("trang chương không tồn tại (404)", "chapter page not found (404)").to_string(),
        GetErr::Other(s) => s,
    })?;
    let imgs = match &series.kind {
        Kind::Blogger => blogger_images(&html, &ch.url),
        Kind::Truyenggvn { sid } => {
            let slug = Regex::new(r"-chap-([^/]+?)\.html")
                .unwrap()
                .captures(&ch.url)
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            truyenggvn_images(&html, &ch.url, sid, &slug)
        }
    };
    if imgs.is_empty() {
        return Err(t("không tìm thấy ảnh trong trang chương", "no images found on the chapter page").into());
    }
    Ok(imgs)
}

fn first_text(doc: &Html, sel: &str) -> Option<String> {
    let s = Selector::parse(sel).ok()?;
    let t = doc.select(&s).next()?.text().collect::<String>();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

fn meta(doc: &Html, prop: &str) -> Option<String> {
    let s = Selector::parse(&format!(r#"meta[property="{prop}"]"#)).ok()?;
    doc.select(&s).next()?.value().attr("content").map(|c| c.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(chap_key("1"), (1, 0));
        assert_eq!(chap_key("1-2"), (1, 2));
        assert_eq!(chap_key("12-fix"), (12, 0));
    }

    #[test]
    fn decode() {
        assert_eq!(urlencoding_decode("Pok%C3%A9mon+X"), "Pokémon X");
        assert_eq!(urlencoding_decode("abc%2"), "abc%2");
    }

    #[test]
    fn route() {
        let c = TRUYENGGVN_RE.captures("/truyen-tranh/hanh-trinh-cua-uduchi-5804-chap-1-2.html").unwrap();
        assert_eq!(&c["series"], "/truyen-tranh/hanh-trinh-cua-uduchi-5804");
        assert_eq!(&c["sid"], "5804");
        let c = TRUYENGGVN_RE.captures("/truyen-tranh/vua-tro-choi-full-mau-4813").unwrap();
        assert_eq!(&c["sid"], "4813");
    }
}
