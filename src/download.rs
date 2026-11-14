//! Tải chương, lọc ảnh quảng cáo, ghi CBZ (từng chương hoặc gộp 1 file).

use crate::i18n::{chapter_display, t};
use crate::sites::{chapter_images, get_bytes, Chapter, GetErr, Series};
use crate::tf;
use reqwest::blocking::Client;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Hai luật lọc quảng cáo, bật riêng hoặc cùng lúc. 1 KB = 1024 byte.
/// Một ảnh bị loại nếu khớp BẤT KỲ luật nào đang bật.
#[derive(Clone, Debug)]
pub struct AdFilter {
    /// Luật 1: loại ảnh NHỎ HƠN below_kb
    pub below_enabled: bool,
    pub below_kb: f64,
    /// Luật 2: loại ảnh LỚN HƠN above_kb
    pub above_enabled: bool,
    pub above_kb: f64,
    /// true: chỉ xét ảnh đầu và ảnh cuối của mỗi chương
    pub edges_only: bool,
    /// Lưu ảnh bị loại vào thư mục riêng để kiểm tra
    pub save_removed: bool,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub series: Series,
    pub chapters: Vec<Chapter>,
    pub out_dir: PathBuf,
    pub name: String,
    pub merge: bool,
    pub overwrite: bool,
    pub ad: AdFilter,
    pub workers: usize,
}

pub enum Msg {
    Log(String),
    Progress { done: usize, total: usize, text: String },
    Finished(String),
}

struct Ctx<'a> {
    tx: &'a Sender<Msg>,
    cancel: &'a AtomicBool,
}

impl Ctx<'_> {
    fn log(&self, s: impl Into<String>) {
        let _ = self.tx.send(Msg::Log(s.into()));
    }
    fn progress(&self, done: usize, total: usize, text: impl Into<String>) {
        let _ = self.tx.send(Msg::Progress { done, total, text: text.into() });
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn safe_name(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c| c == ' ' || c == '.').to_string();
    if s.is_empty() { "truyen".into() } else { s }
}

fn kb(n: usize) -> String {
    format!("{:.0} KB", n as f64 / 1024.0)
}

const MAGIC: [(&[u8], &str); 5] = [
    (b"\x89PNG", ".png"),
    (b"\xff\xd8", ".jpg"),
    (b"GIF8", ".gif"),
    (b"BM", ".bmp"),
    (b"RIFF", ".webp"),
];

pub fn image_ext(data: &[u8]) -> Option<&'static str> {
    MAGIC.iter().find(|(sig, _)| data.starts_with(sig)).map(|(_, e)| *e)
}

pub struct Page {
    pub data: Vec<u8>,
    pub ext: &'static str,
}

/// Tải tất cả ảnh song song (giữ đúng thứ tự).
fn fetch_pages(http: &Client, urls: &[String], referer: &str, workers: usize, ctx: &Ctx) -> Result<Vec<Page>, String> {
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<Page, String>>>> = urls.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|s| {
        for _ in 0..workers.clamp(1, 16).min(urls.len().max(1)) {
            s.spawn(|| loop {
                if ctx.cancelled() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= urls.len() {
                    return;
                }
                let r = match get_bytes(http, &urls[i], Some(referer)) {
                    Ok(d) => match image_ext(&d) {
                        Some(ext) => Ok(Page { data: d, ext }),
                        None => Err(tf!("trang {} không phải ảnh", "page {} is not an image", i + 1)),
                    },
                    Err(GetErr::NotFound) => Err(tf!("trang {} không tồn tại (404)", "page {} not found (404)", i + 1)),
                    Err(GetErr::Other(e)) => Err(tf!("trang {}: {e}", "page {}: {e}", i + 1)),
                };
                *slots[i].lock().unwrap() = Some(r);
            });
        }
    });
    if ctx.cancelled() {
        return Err(t("đã dừng", "stopped").into());
    }
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap_or_else(|| Err(t("chưa tải", "not downloaded").into())))
        .collect()
}

impl AdFilter {
    pub fn active(&self) -> bool {
        (self.below_enabled && self.below_kb > 0.0) || (self.above_enabled && self.above_kb > 0.0)
    }

    /// Ảnh có dung lượng `size` byte có bị coi là quảng cáo không (chưa xét vị trí).
    pub fn matches(&self, size: usize) -> bool {
        let size = size as f64;
        (self.below_enabled && self.below_kb > 0.0 && size < self.below_kb * 1024.0)
            || (self.above_enabled && self.above_kb > 0.0 && size > self.above_kb * 1024.0)
    }
}

/// Trả về (ảnh giữ lại, [(số thứ tự gốc, ảnh bị loại)], mọi_ảnh_đều_khớp).
/// Không bao giờ loại hết ảnh của một chương.
pub fn filter_ads(pages: Vec<Page>, ad: &AdFilter) -> (Vec<Page>, Vec<(usize, Page)>, bool) {
    if !ad.active() || pages.is_empty() {
        return (pages, vec![], false);
    }
    let n = pages.len();
    let drop: Vec<bool> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let candidate = !ad.edges_only || i == 0 || i == n - 1;
            candidate && n > 1 && ad.matches(p.data.len())
        })
        .collect();
    if drop.iter().all(|d| *d) {
        return (pages, vec![], true);
    }
    let (mut kept, mut removed) = (vec![], vec![]);
    for (i, (p, d)) in pages.into_iter().zip(drop).enumerate() {
        if d { removed.push((i + 1, p)) } else { kept.push(p) }
    }
    (kept, removed, false)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn comic_info(series: &str, title: &str, number: Option<&str>, pages: usize, bookmarks: &[(usize, String)]) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ComicInfo>\n");
    x += &format!("  <Series>{}</Series>\n  <Title>{}</Title>\n", xml_escape(series), xml_escape(title));
    if let Some(n) = number {
        x += &format!("  <Number>{}</Number>\n", xml_escape(n));
    }
    x += &format!("  <PageCount>{pages}</PageCount>\n  <LanguageISO>vi</LanguageISO>\n  <Manga>Yes</Manga>\n");
    if !bookmarks.is_empty() {
        x += "  <Pages>\n";
        for (i, t) in bookmarks {
            x += &format!("    <Page Image=\"{i}\" Bookmark=\"{}\" />\n", xml_escape(t));
        }
        x += "  </Pages>\n";
    }
    x + "</ComicInfo>\n"
}

fn opts() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true)
}

type Zip = ZipWriter<BufWriter<File>>;

fn zip_put(z: &mut Zip, name: &str, data: &[u8]) -> Result<(), String> {
    z.start_file(name, opts()).map_err(|e| tf!("lỗi ghi zip: {e}", "zip write error: {e}"))?;
    z.write_all(data).map_err(|e| tf!("lỗi ghi file: {e}", "file write error: {e}"))
}

fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Tên chương trong bookmark của file CBZ (giữ nguyên, không đổi theo ngôn ngữ giao diện)
fn chapter_label(ch: &Chapter) -> String {
    ch.title.clone()
}

/// Tải một chương: trả về ảnh đã lọc + ghi chú.
fn load_chapter(http: &Client, job: &Job, ch: &Chapter, ctx: &Ctx) -> Result<(Vec<Page>, String), String> {
    let urls = chapter_images(http, &job.series, ch)?;
    let pages = fetch_pages(http, &urls, &job.series.referer, job.workers, ctx)?;
    let (kept, removed, all_matched) = filter_ads(pages, &job.ad);
    let mut note = tf!("{} trang", "{} pages", kept.len());
    if all_matched {
        note += t(
            " (mọi ảnh đều khớp luật lọc nên giữ nguyên, hãy kiểm tra lại ngưỡng KB)",
            " (every image matched the ad rules so none were removed; check the KB limits)",
        );
    }
    if !removed.is_empty() {
        let list: Vec<String> = removed
            .iter()
            .map(|(i, p)| tf!("trang {i} ({})", "page {i} ({})", kb(p.data.len())))
            .collect();
        note += &tf!(", đã loại {}", ", removed {}", list.join(", "));
        if job.ad.save_removed {
            let dir = job.out_dir.join("_anh_bi_loai");
            if fs::create_dir_all(&dir).is_ok() {
                for (i, p) in &removed {
                    let f = dir.join(format!("{} - {} - trang {i:03}{}", safe_name(&job.name), safe_name(&chapter_label(ch)), p.ext));
                    let _ = fs::write(f, &p.data);
                }
            }
        }
    }
    Ok((kept, note))
}

pub fn run(job: Job, tx: Sender<Msg>, cancel: Arc<AtomicBool>) {
    let ctx = Ctx { tx: &tx, cancel: &cancel };
    let http = crate::sites::client();
    let summary = if job.merge { run_merge(&http, &job, &ctx) } else { run_each(&http, &job, &ctx) };
    let _ = tx.send(Msg::Finished(summary));
}

fn run_each(http: &Client, job: &Job, ctx: &Ctx) -> String {
    let dir = job.out_dir.join(safe_name(&job.name));
    if let Err(e) = fs::create_dir_all(&dir) {
        return tf!("Không tạo được thư mục {}: {e}", "Could not create folder {}: {e}", dir.display());
    }
    ctx.log(tf!("Lưu từng chương vào: {}", "Saving each chapter to: {}", dir.display()));
    let total = job.chapters.len();
    let (mut ok, mut skipped, mut failed) = (0, 0, Vec::new());
    for (i, ch) in job.chapters.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }
        let label = chapter_label(ch);
        let shown = chapter_display(ch);
        ctx.progress(i, total, format!("{shown} ({}/{total})", i + 1));
        let file_title =
            if ch.number.is_empty() { label.clone() } else { format!("{} - Chương {}", job.name, ch.number) };
        let cbz = dir.join(format!("{}.cbz", safe_name(&file_title)));
        if cbz.exists() && !job.overwrite {
            ctx.log(tf!("[đã có] {shown}", "[exists] {shown}"));
            skipped += 1;
            continue;
        }
        let res = load_chapter(http, job, ch, ctx).and_then(|(pages, note)| {
            let tmp = part_path(&cbz);
            let write = || -> Result<(), String> {
                let f = File::create(&tmp).map_err(|e| tf!("không tạo được file: {e}", "could not create file: {e}"))?;
                let mut z = ZipWriter::new(BufWriter::new(f));
                for (pi, p) in pages.iter().enumerate() {
                    zip_put(&mut z, &format!("{:04}{}", pi + 1, p.ext), &p.data)?;
                }
                let number = (!ch.number.is_empty()).then(|| ch.number.trim_start_matches('0').to_string());
                let info = comic_info(&job.name, &file_title, number.as_deref(), pages.len(), &[]);
                zip_put(&mut z, "ComicInfo.xml", info.as_bytes())?;
                z.finish()
                    .map_err(|e| tf!("lỗi đóng zip: {e}", "zip finish error: {e}"))?
                    .flush()
                    .map_err(|e| e.to_string())?;
                Ok(())
            };
            match write().and_then(|_| fs::rename(&tmp, &cbz).map_err(|e| e.to_string())) {
                Ok(()) => Ok(note),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(e)
                }
            }
        });
        match res {
            Ok(note) => {
                ok += 1;
                ctx.log(tf!("[xong] {shown}: {note}", "[done] {shown}: {note}"));
            }
            Err(e) => {
                if ctx.cancelled() {
                    break;
                }
                ctx.log(tf!("[LỖI] {shown}: {e}", "[ERROR] {shown}: {e}"));
                failed.push(shown);
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
    ctx.progress(total, total, "");
    let mut s = tf!(
        "Đã tải {ok} chương, bỏ qua {skipped} chương đã có",
        "Downloaded {ok} chapters, skipped {skipped} existing"
    );
    if !failed.is_empty() {
        s += &tf!(", lỗi {}: {}", ", failed {}: {}", failed.len(), failed.join(", "));
    }
    if ctx.cancelled() {
        s = tf!("Đã dừng. {s}", "Stopped. {s}");
    }
    s + &tf!(".\nThư mục: {}", ".\nFolder: {}", dir.display())
}

fn run_merge(http: &Client, job: &Job, ctx: &Ctx) -> String {
    if let Err(e) = fs::create_dir_all(&job.out_dir) {
        return tf!("Không tạo được thư mục {}: {e}", "Could not create folder {}: {e}", job.out_dir.display());
    }
    let out = job.out_dir.join(format!("{}.cbz", safe_name(&job.name)));
    if out.exists() && !job.overwrite {
        return tf!(
            "Đã có file {}.\nĐổi tên truyện, hoặc bật \"Ghi đè nếu file đã có\".",
            "{} already exists.\nChange the comic name, or turn on \"Overwrite existing files\".",
            out.display()
        );
    }
    let tmp = part_path(&out);
    let f = match File::create(&tmp) {
        Ok(f) => f,
        Err(e) => return tf!("Không tạo được file {}: {e}", "Could not create file {}: {e}", tmp.display()),
    };
    ctx.log(tf!("Gộp tất cả vào: {}", "Merging everything into: {}", out.display()));
    let mut z = ZipWriter::new(BufWriter::new(f));
    let total = job.chapters.len();
    let (mut pages_total, mut ci, mut bookmarks, mut failed) = (0usize, 0usize, Vec::new(), Vec::new());

    let result = (|| -> Result<(), String> {
        for (i, ch) in job.chapters.iter().enumerate() {
            if ctx.cancelled() {
                return Err(t("đã dừng", "stopped").into());
            }
            let label = chapter_label(ch);
            let shown = chapter_display(ch);
            ctx.progress(i, total, format!("{shown} ({}/{total})", i + 1));
            match load_chapter(http, job, ch, ctx) {
                Ok((pages, note)) => {
                    ci += 1;
                    bookmarks.push((pages_total, label.clone()));
                    for (pi, p) in pages.iter().enumerate() {
                        zip_put(&mut z, &format!("{ci:04}_{:04}{}", pi + 1, p.ext), &p.data)?;
                        pages_total += 1;
                    }
                    ctx.log(tf!("[xong] {shown}: {note}", "[done] {shown}: {note}"));
                }
                Err(e) => {
                    if ctx.cancelled() {
                        return Err(t("đã dừng", "stopped").into());
                    }
                    ctx.log(tf!("[LỖI] {shown}: {e} (bỏ qua chương này)", "[ERROR] {shown}: {e} (chapter skipped)"));
                    failed.push(shown);
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
        if ci == 0 {
            return Err(t("không tải được chương nào", "no chapter could be downloaded").into());
        }
        let info = comic_info(&job.name, &job.name, None, pages_total, &bookmarks);
        zip_put(&mut z, "ComicInfo.xml", info.as_bytes())?;
        Ok(())
    })();

    let finished = result.and_then(|_| {
        let mut w = z.finish().map_err(|e| tf!("lỗi đóng zip: {e}", "zip finish error: {e}"))?;
        w.flush().map_err(|e| e.to_string())?;
        drop(w);
        if out.exists() {
            fs::remove_file(&out).map_err(|e| tf!("không ghi đè được file cũ: {e}", "could not overwrite old file: {e}"))?;
        }
        fs::rename(&tmp, &out).map_err(|e| e.to_string())
    });
    ctx.progress(total, total, "");
    match finished {
        Ok(()) => {
            let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            let mut s = tf!(
                "Xong: {} ({ci} chương, {pages_total} trang, {:.1} MB)",
                "Done: {} ({ci} chapters, {pages_total} pages, {:.1} MB)",
                out.display(),
                size as f64 / 1048576.0
            );
            if !failed.is_empty() {
                s += &tf!(
                    "\nCác chương bị lỗi (không có trong file): {}",
                    "\nFailed chapters (not in the file): {}",
                    failed.join(", ")
                );
            }
            s
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            tf!(
                "Không tạo được file gộp: {e}. Không có file nào bị thay đổi.",
                "Could not create the merged file: {e}. No files were changed."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(kb: usize) -> Page {
        Page { data: vec![0; kb * 1024], ext: ".jpg" }
    }

    #[test]
    fn ads() {
        let f = |edges| AdFilter {
            below_enabled: true,
            below_kb: 100.0,
            above_enabled: false,
            above_kb: 0.0,
            edges_only: edges,
            save_removed: false,
        };
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(20), pg(300), pg(80)], &f(false));
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (2, vec![1, 3, 5]));
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(20), pg(300), pg(80)], &f(true));
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (3, vec![1, 5]));
        let (k, r, all) = filter_ads(vec![pg(5), pg(6)], &f(false));
        assert_eq!((k.len(), r.len(), all), (2, 0, true));

        // luật "lớn hơn" riêng, và hai luật cùng lúc
        let big = AdFilter { below_enabled: false, above_enabled: true, above_kb: 1000.0, ..f(false) };
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(1500), pg(300), pg(2000)], &big);
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (3, vec![3, 5]));
        let both = AdFilter { below_enabled: true, ..big.clone() };
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(1500), pg(300), pg(2000)], &both);
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (2, vec![1, 3, 5]));
        let both_edges = AdFilter { edges_only: true, ..both.clone() };
        let (k, r, _) = filter_ads(vec![pg(2000), pg(300), pg(1500), pg(300), pg(50)], &both_edges);
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (3, vec![1, 5]));
        // ngưỡng = đúng biên thì không loại (nhỏ hơn / lớn hơn là so sánh chặt)
        let (_, r, _) = filter_ads(vec![pg(100), pg(1000)], &both);
        assert!(r.is_empty());
        // tắt cả hai luật -> không loại gì
        let off = AdFilter { below_enabled: false, above_enabled: false, ..both };
        let (k, _, _) = filter_ads(vec![pg(1), pg(5000)], &off);
        assert_eq!(k.len(), 2);
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("Tập 15 - Chương 181: Mt.Silver?"), "Tập 15 - Chương 181- Mt.Silver-");
    }
}
