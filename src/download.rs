//! Tải chương, lọc ảnh quảng cáo, ghi CBZ (từng chương hoặc gộp 1 file).

use crate::sites::{chapter_images, get_bytes, Chapter, GetErr, Series};
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

#[derive(Clone, Debug)]
pub struct AdFilter {
    pub enabled: bool,
    /// Ảnh nhỏ hơn số KB này (1 KB = 1024 byte) bị coi là quảng cáo
    pub below_kb: f64,
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
                        None => Err(format!("trang {} không phải ảnh", i + 1)),
                    },
                    Err(GetErr::NotFound) => Err(format!("trang {} không tồn tại (404)", i + 1)),
                    Err(GetErr::Other(e)) => Err(format!("trang {}: {e}", i + 1)),
                };
                *slots[i].lock().unwrap() = Some(r);
            });
        }
    });
    if ctx.cancelled() {
        return Err("đã dừng".into());
    }
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap_or_else(|| Err("chưa tải".into())))
        .collect()
}

/// Trả về (ảnh giữ lại, [(số thứ tự gốc, ảnh bị loại)]). Không bao giờ loại hết ảnh của một chương.
pub fn filter_ads(pages: Vec<Page>, ad: &AdFilter) -> (Vec<Page>, Vec<(usize, Page)>, bool) {
    if !ad.enabled || ad.below_kb <= 0.0 || pages.is_empty() {
        return (pages, vec![], false);
    }
    let limit = (ad.below_kb * 1024.0) as usize;
    let n = pages.len();
    let drop: Vec<bool> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let candidate = !ad.edges_only || i == 0 || i == n - 1;
            candidate && n > 1 && p.data.len() < limit
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
    z.start_file(name, opts()).map_err(|e| format!("lỗi ghi zip: {e}"))?;
    z.write_all(data).map_err(|e| format!("lỗi ghi file: {e}"))
}

fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Tên hiển thị của chương trong tên file / bookmark
fn chapter_label(ch: &Chapter) -> String {
    ch.title.clone()
}

/// Tải một chương: trả về ảnh đã lọc + ghi chú.
fn load_chapter(http: &Client, job: &Job, ch: &Chapter, ctx: &Ctx) -> Result<(Vec<Page>, String), String> {
    let urls = chapter_images(http, &job.series, ch)?;
    let pages = fetch_pages(http, &urls, &job.series.referer, job.workers, ctx)?;
    let (kept, removed, all_small) = filter_ads(pages, &job.ad);
    let mut note = format!("{} trang", kept.len());
    if all_small {
        note += " (mọi ảnh đều nhỏ hơn ngưỡng nên giữ nguyên, hãy kiểm tra lại ngưỡng KB)";
    }
    if !removed.is_empty() {
        let list: Vec<String> = removed.iter().map(|(i, p)| format!("trang {i} ({})", kb(p.data.len()))).collect();
        note += &format!(", đã loại {}", list.join(", "));
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
        return format!("Không tạo được thư mục {}: {e}", dir.display());
    }
    ctx.log(format!("Lưu từng chương vào: {}", dir.display()));
    let total = job.chapters.len();
    let (mut ok, mut skipped, mut failed) = (0, 0, Vec::new());
    for (i, ch) in job.chapters.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }
        let label = chapter_label(ch);
        ctx.progress(i, total, format!("{label} ({}/{total})", i + 1));
        let file_title =
            if ch.number.is_empty() { label.clone() } else { format!("{} - Chương {}", job.name, ch.number) };
        let cbz = dir.join(format!("{}.cbz", safe_name(&file_title)));
        if cbz.exists() && !job.overwrite {
            ctx.log(format!("[đã có] {label}"));
            skipped += 1;
            continue;
        }
        let res = load_chapter(http, job, ch, ctx).and_then(|(pages, note)| {
            let tmp = part_path(&cbz);
            let write = || -> Result<(), String> {
                let f = File::create(&tmp).map_err(|e| format!("không tạo được file: {e}"))?;
                let mut z = ZipWriter::new(BufWriter::new(f));
                for (pi, p) in pages.iter().enumerate() {
                    zip_put(&mut z, &format!("{:04}{}", pi + 1, p.ext), &p.data)?;
                }
                let number = (!ch.number.is_empty()).then(|| ch.number.trim_start_matches('0').to_string());
                let info = comic_info(&job.name, &file_title, number.as_deref(), pages.len(), &[]);
                zip_put(&mut z, "ComicInfo.xml", info.as_bytes())?;
                z.finish().map_err(|e| format!("lỗi đóng zip: {e}"))?.flush().map_err(|e| e.to_string())?;
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
                ctx.log(format!("[xong] {label}: {note}"));
            }
            Err(e) => {
                if ctx.cancelled() {
                    break;
                }
                ctx.log(format!("[LỖI] {label}: {e}"));
                failed.push(label);
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
    ctx.progress(total, total, "");
    let mut s = format!("Đã tải {ok} chương, bỏ qua {skipped} chương đã có");
    if !failed.is_empty() {
        s += &format!(", lỗi {}: {}", failed.len(), failed.join(", "));
    }
    if ctx.cancelled() {
        s = format!("Đã dừng. {s}");
    }
    s + &format!(".\nThư mục: {}", dir.display())
}

fn run_merge(http: &Client, job: &Job, ctx: &Ctx) -> String {
    if let Err(e) = fs::create_dir_all(&job.out_dir) {
        return format!("Không tạo được thư mục {}: {e}", job.out_dir.display());
    }
    let out = job.out_dir.join(format!("{}.cbz", safe_name(&job.name)));
    if out.exists() && !job.overwrite {
        return format!("Đã có file {}.\nĐổi tên truyện, hoặc bật \"Ghi đè nếu đã có\".", out.display());
    }
    let tmp = part_path(&out);
    let f = match File::create(&tmp) {
        Ok(f) => f,
        Err(e) => return format!("Không tạo được file {}: {e}", tmp.display()),
    };
    ctx.log(format!("Gộp tất cả vào: {}", out.display()));
    let mut z = ZipWriter::new(BufWriter::new(f));
    let total = job.chapters.len();
    let (mut pages_total, mut ci, mut bookmarks, mut failed) = (0usize, 0usize, Vec::new(), Vec::new());

    let result = (|| -> Result<(), String> {
        for (i, ch) in job.chapters.iter().enumerate() {
            if ctx.cancelled() {
                return Err("đã dừng".into());
            }
            let label = chapter_label(ch);
            ctx.progress(i, total, format!("{label} ({}/{total})", i + 1));
            match load_chapter(http, job, ch, ctx) {
                Ok((pages, note)) => {
                    ci += 1;
                    bookmarks.push((pages_total, label.clone()));
                    for (pi, p) in pages.iter().enumerate() {
                        zip_put(&mut z, &format!("{ci:04}_{:04}{}", pi + 1, p.ext), &p.data)?;
                        pages_total += 1;
                    }
                    ctx.log(format!("[xong] {label}: {note}"));
                }
                Err(e) => {
                    if ctx.cancelled() {
                        return Err("đã dừng".into());
                    }
                    ctx.log(format!("[LỖI] {label}: {e} (bỏ qua chương này)"));
                    failed.push(label);
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
        if ci == 0 {
            return Err("không tải được chương nào".into());
        }
        let info = comic_info(&job.name, &job.name, None, pages_total, &bookmarks);
        zip_put(&mut z, "ComicInfo.xml", info.as_bytes())?;
        Ok(())
    })();

    let finished = result.and_then(|_| {
        let mut w = z.finish().map_err(|e| format!("lỗi đóng zip: {e}"))?;
        w.flush().map_err(|e| e.to_string())?;
        drop(w);
        if out.exists() {
            fs::remove_file(&out).map_err(|e| format!("không ghi đè được file cũ: {e}"))?;
        }
        fs::rename(&tmp, &out).map_err(|e| e.to_string())
    });
    ctx.progress(total, total, "");
    match finished {
        Ok(()) => {
            let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            let mut s = format!(
                "Xong: {} ({ci} chương, {pages_total} trang, {:.1} MB)",
                out.display(),
                size as f64 / 1048576.0
            );
            if !failed.is_empty() {
                s += &format!("\nCác chương bị lỗi (không có trong file): {}", failed.join(", "));
            }
            s
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            format!("Không tạo được file gộp: {e}. Không có file nào bị thay đổi.")
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
        let f = |edges| AdFilter { enabled: true, below_kb: 100.0, edges_only: edges, save_removed: false };
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(20), pg(300), pg(80)], &f(false));
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (2, vec![1, 3, 5]));
        let (k, r, _) = filter_ads(vec![pg(50), pg(300), pg(20), pg(300), pg(80)], &f(true));
        assert_eq!((k.len(), r.iter().map(|x| x.0).collect::<Vec<_>>()), (3, vec![1, 5]));
        let (k, r, all) = filter_ads(vec![pg(5), pg(6)], &f(false));
        assert_eq!((k.len(), r.len(), all), (2, 0, true));
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("Tập 15 - Chương 181: Mt.Silver?"), "Tập 15 - Chương 181- Mt.Silver-");
    }
}
