//! Ngôn ngữ giao diện (Tiếng Việt / English).
//!
//! Lựa chọn được lưu vào file cài đặt dùng chung cho bản desktop và bản trình duyệt,
//! nên lần mở sau sẽ dùng lại ngôn ngữ của phiên trước (mặc định: Tiếng Việt).
//! Tên file, bookmark và nội dung CBZ không đổi theo ngôn ngữ, để thư viện truyện luôn nhất quán.

use crate::sites::Chapter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

pub fn is_en() -> bool {
    EN.load(Ordering::Relaxed)
}

pub fn code() -> &'static str {
    if is_en() { "en" } else { "vi" }
}

/// Chọn chuỗi theo ngôn ngữ hiện tại.
pub fn t(vi: &'static str, en: &'static str) -> &'static str {
    if is_en() { en } else { vi }
}

/// `tf!("tiếng Việt {x}", "English {x}", ...)`: như `format!` nhưng chọn mẫu theo ngôn ngữ.
#[macro_export]
macro_rules! tf {
    ($vi:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() { format!($en $(, $arg)*) } else { format!($vi $(, $arg)*) }
    };
}

fn settings_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CBZ_DOWNLOADER_CONFIG_DIR") {
        return Some(PathBuf::from(d).join("settings.txt"));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("CBZ Downloader"));
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("cbz_downloader"));
    base.map(|b| b.join("settings.txt"))
}

/// Đọc ngôn ngữ đã lưu (gọi một lần khi khởi động).
pub fn load() {
    let en = settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().any(|l| l.trim() == "lang=en"))
        .unwrap_or(false);
    EN.store(en, Ordering::Relaxed);
}

/// Đổi ngôn ngữ và lưu lại cho lần sau.
pub fn set_en(en: bool) {
    EN.store(en, Ordering::Relaxed);
    if let Some(p) = settings_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, format!("lang={}\n", if en { "en" } else { "vi" }));
    }
}

/// Tên chương để hiển thị: "Chương 1.1" -> "Chapter 1.1" khi dùng tiếng Anh.
/// Chỉ áp dụng cho tên do phần mềm tự đặt; tiêu đề bài Blogger giữ nguyên.
pub fn chapter_display(ch: &Chapter) -> String {
    if is_en() && !ch.number.is_empty()
        && let Some(rest) = ch.title.strip_prefix("Chương ") {
            return format!("Chapter {rest}");
        }
    ch.title.clone()
}
