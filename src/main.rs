// Ẩn cửa sổ console đen khi chạy bản release trên Windows
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod download;
mod i18n;
mod sites;
mod web;

use download::{AdFilter, Job, Msg};
use i18n::{chapter_display, t};
use eframe::egui;
use sites::Series;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() -> eframe::Result {
    // Chế độ dòng lệnh (dùng để kiểm thử): cbz_downloader --cli <url> <thư_mục> <each|merge> <KB> [edges] [chọn: 1-5]
    let args: Vec<String> = std::env::args().collect();
    i18n::load();
    if args.get(1).map(String::as_str) == Some("--cli") {
        cli(&args[2..]);
        return Ok(());
    }
    // Bản mở bằng trình duyệt: cbz_downloader --web  (thêm --no-open để không tự mở trình duyệt)
    if args.iter().any(|a| a == "--web") {
        web::run(!args.iter().any(|a| a == "--no-open"));
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CBZ Downloader")
            .with_inner_size([1060.0, 780.0])
            .with_min_inner_size([720.0, 560.0]),
        ..Default::default()
    };
    // cbz_downloader <url>: mở giao diện với địa chỉ điền sẵn và tự kiểm tra
    let start_url = args.get(1).filter(|a| !a.starts_with('-')).cloned();
    eframe::run_native(
        "CBZ Downloader",
        options,
        Box::new(|cc| {
            let mut app = App::new(cc);
            if let Some(u) = start_url {
                app.url = u;
                app.start_inspect();
            }
            Ok(Box::new(app))
        }),
    )
}

fn default_out_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => {
            let d = PathBuf::from(h).join("Downloads");
            if d.is_dir() { d } else { PathBuf::from(".") }
        }
        None => PathBuf::from("."),
    }
}

/// Dùng font hệ thống có đủ dấu tiếng Việt (Segoe UI trên Windows).
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];
    if let Some(data) = candidates.iter().find_map(|p| std::fs::read(p).ok()) {
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "system",
            egui::FontData::from_owned(data),
            vec![
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Proportional,
                    priority: egui::epaint::text::FontPriority::Highest,
                },
                // chữ có dấu trong ô nhật ký (font monospace) dùng font này khi thiếu
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Monospace,
                    priority: egui::epaint::text::FontPriority::Lowest,
                },
            ],
        ));
    }
}

#[derive(PartialEq)]
enum Status {
    Idle,
    Checking,
    /// (số chương, tên trang): dựng câu thông báo lúc vẽ để đổi ngôn ngữ có tác dụng ngay
    Ok(usize, String),
    Err(String),
}

struct App {
    url: String,
    status: Status,
    series: Option<Series>,
    selected: Vec<bool>,
    filter: String,
    range_from: usize,
    range_to: usize,

    name: String,
    out_dir: String,
    merge: bool,
    overwrite: bool,
    ad_below_on: bool,
    ad_below_kb: f64,
    ad_above_on: bool,
    ad_above_kb: f64,
    ad_edges_only: bool,
    ad_save: bool,
    workers: usize,

    inspect_rx: Option<Receiver<Result<Series, String>>>,
    job_rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    log: Vec<String>,
    progress: f32,
    progress_text: String,
    result: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        Self {
            url: String::new(),
            status: Status::Idle,
            series: None,
            selected: vec![],
            filter: String::new(),
            range_from: 1,
            range_to: 1,
            name: String::new(),
            out_dir: default_out_dir().display().to_string(),
            merge: false,
            overwrite: false,
            ad_below_on: false,
            ad_below_kb: 100.0,
            ad_above_on: false,
            ad_above_kb: 2000.0,
            ad_edges_only: true,
            ad_save: false,
            workers: 4,
            inspect_rx: None,
            job_rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            log: vec![],
            progress: 0.0,
            progress_text: String::new(),
            result: None,
        }
    }

    fn busy(&self) -> bool {
        self.inspect_rx.is_some() || self.job_rx.is_some()
    }

    fn start_inspect(&mut self) {
        let url = self.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.inspect_rx = Some(rx);
        self.status = Status::Checking;
        self.series = None;
        self.result = None;
        thread::spawn(move || {
            let http = sites::client();
            let _ = tx.send(sites::inspect(&http, &url));
        });
    }

    fn visible(&self) -> Vec<usize> {
        let Some(s) = &self.series else { return vec![] };
        let f = self.filter.trim().to_lowercase();
        (0..s.chapters.len())
            .filter(|&i| {
                f.is_empty()
                    || s.chapters[i].title.to_lowercase().contains(&f)
                    || s.chapters[i].group.to_lowercase().contains(&f)
            })
            .collect()
    }

    fn start_job(&mut self) {
        let Some(series) = self.series.clone() else { return };
        let chapters: Vec<_> = series
            .chapters
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .map(|(c, _)| c.clone())
            .collect();
        if chapters.is_empty() {
            return;
        }
        let job = Job {
            series,
            chapters,
            out_dir: PathBuf::from(self.out_dir.trim()),
            name: if self.name.trim().is_empty() { "Truyen".into() } else { self.name.trim().to_string() },
            merge: self.merge,
            overwrite: self.overwrite,
            ad: AdFilter {
                below_enabled: self.ad_below_on,
                below_kb: self.ad_below_kb,
                above_enabled: self.ad_above_on,
                above_kb: self.ad_above_kb,
                edges_only: self.ad_edges_only,
                save_removed: self.ad_save,
            },
            workers: self.workers,
        };
        let (tx, rx) = channel();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        self.job_rx = Some(rx);
        self.log.clear();
        self.result = None;
        self.progress = 0.0;
        thread::spawn(move || download::run(job, tx, cancel));
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.inspect_rx
            && let Ok(r) = rx.try_recv() {
                self.inspect_rx = None;
                match r {
                    Ok(s) => {
                        let n = s.chapters.len();
                        self.status = Status::Ok(n, s.site.clone());
                        self.name = s.title.clone();
                        self.selected = vec![true; n];
                        self.range_from = 1;
                        self.range_to = n;
                        self.filter.clear();
                        self.series = Some(s);
                    }
                    Err(e) => self.status = Status::Err(e),
                }
            }
        let mut done = false;
        if let Some(rx) = &self.job_rx {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Log(s) => self.log.push(s),
                    Msg::Progress { done, total, text } => {
                        self.progress = if total == 0 { 1.0 } else { done as f32 / total as f32 };
                        self.progress_text = text;
                    }
                    Msg::Finished(s) => {
                        self.result = Some(s);
                        done = true;
                    }
                }
            }
        }
        if done {
            self.job_rx = None;
            self.progress = 1.0;
            self.progress_text.clear();
        }
    }

    // ------------------------------------------------------------ giao diện

    fn ui_top(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("CBZ Downloader");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // công tắc ngôn ngữ: đổi ngay và nhớ cho lần mở sau
                let en = i18n::is_en();
                if ui.selectable_label(en, "EN").on_hover_text("English").clicked() && !en {
                    i18n::set_en(true);
                }
                if ui.selectable_label(!en, "VI").on_hover_text("Tiếng Việt").clicked() && en {
                    i18n::set_en(false);
                }
                ui.separator();
                if ui
                    .button(t("🌐 Mở bản trình duyệt", "🌐 Open in browser"))
                    .on_hover_text(t("Mở giao diện này trong trình duyệt web", "Open this interface in your web browser"))
                    .clicked()
                    && let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).arg("--web").spawn();
                    }
            });
        });
        ui.horizontal(|ui| {
            ui.label(t("Địa chỉ trang truyện:", "Comic page address:"));
            let edit = ui.add_enabled(
                !self.busy(),
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text(t("vd. ", "e.g. ").to_string() + "https://truyenggvn.com/truyen-tranh/hanh-trinh-cua-uduchi-5804")
                    .desired_width(ui.available_width() - 110.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(!self.busy(), egui::Button::new(t("Kiểm tra", "Check"))).clicked() || enter {
                self.start_inspect();
            }
        });
        match &self.status {
            Status::Idle => {
                ui.weak(t(
                    "Hỗ trợ: các trang dạng truyenggvn.com và các trang Blogger (vd. pokemonspecial.com).",
                    "Supported: sites like truyenggvn.com and Blogger sites (e.g. pokemonspecial.com).",
                ));
            }
            Status::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t("Đang kiểm tra trang và đọc danh sách chương...", "Checking the page and reading the chapter list..."));
                });
            }
            Status::Ok(n, site) => {
                ui.colored_label(
                    egui::Color32::from_rgb(40, 150, 70),
                    tf!("✔ Tải được: {n} chương từ {site}", "✔ Can download: {n} chapters from {site}"),
                );
            }
            Status::Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), tf!("✘ Không tải được. {e}", "✘ Cannot download. {e}"));
            }
        }
        ui.add_space(4.0);
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        // cho chữ tự xuống dòng trong khung hẹp (câu tiếng Anh thường dài hơn)
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        let enabled = !self.busy();
        ui.add_enabled_ui(enabled, |ui| {
            ui.heading(t("Tùy chọn lưu", "Save options"));
            ui.label(t("Tên truyện (tên file):", "Comic name (file name):"));
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(ui.available_width() - 8.0));
            ui.label(t("Thư mục lưu:", "Save folder:"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(ui.available_width() - 95.0));
                if ui.button(t("Chọn…", "Browse…")).clicked()
                    && let Some(p) = rfd::FileDialog::new().set_directory(&self.out_dir).pick_folder() {
                        self.out_dir = p.display().to_string();
                    }
            });
            ui.add_space(4.0);
            ui.radio_value(&mut self.merge, false, t("Lưu từng chương thành file riêng", "Save each chapter as its own file"));
            ui.radio_value(&mut self.merge, true, t("Gộp tất cả chương đã chọn thành 1 file CBZ", "Merge all selected chapters into 1 CBZ file"));
            ui.weak(if self.merge {
                format!("→ {}.cbz", download::safe_name(&self.name))
            } else {
                tf!(
                    "→ thư mục \"{}\", chương đã có sẽ được bỏ qua",
                    "→ folder \"{}\", existing chapters are skipped",
                    download::safe_name(&self.name)
                )
            });
            ui.checkbox(&mut self.overwrite, t("Ghi đè nếu file đã có", "Overwrite existing files"));

            ui.separator();
            ui.heading(t("Loại bỏ trang quảng cáo", "Remove ad pages"));
            ui.weak(t(
                "Bật một hoặc cả hai luật. Ảnh khớp bất kỳ luật nào đang bật sẽ bị loại.",
                "Turn on one or both rules. An image matching any active rule is removed.",
            ));
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.ad_below_on, t("Loại ảnh nhỏ hơn", "Remove images smaller than"));
                ui.add_enabled(
                    self.ad_below_on,
                    egui::DragValue::new(&mut self.ad_below_kb).range(1.0..=100000.0).speed(1.0).suffix(" KB"),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.ad_above_on, t("Loại ảnh lớn hơn", "Remove images larger than"));
                ui.add_enabled(
                    self.ad_above_on,
                    egui::DragValue::new(&mut self.ad_above_kb).range(1.0..=100000.0).speed(5.0).suffix(" KB"),
                );
            });
            if self.ad_below_on && self.ad_above_on && self.ad_above_kb <= self.ad_below_kb {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 120, 30),
                    t(
                        "⚠ Ngưỡng \"lớn hơn\" đang nhỏ hơn ngưỡng \"nhỏ hơn\": mọi ảnh đều sẽ khớp luật.",
                        "⚠ The \"larger than\" limit is below the \"smaller than\" limit: every image will match.",
                    ),
                );
            }
            ui.add_enabled_ui(self.ad_below_on || self.ad_above_on, |ui| {
                ui.radio_value(
                    &mut self.ad_edges_only,
                    true,
                    t("Chỉ xét ảnh đầu và ảnh cuối mỗi chương", "Only check the first and last image of each chapter"),
                );
                ui.radio_value(&mut self.ad_edges_only, false, t("Xét mọi ảnh trong chương", "Check every image in the chapter"));
                ui.checkbox(
                    &mut self.ad_save,
                    t(
                        "Lưu ảnh bị loại vào thư mục _anh_bi_loai để kiểm tra",
                        "Save removed images to the _anh_bi_loai folder for review",
                    ),
                );
                ui.weak(t(
                    "Nếu mọi ảnh của một chương đều khớp luật, chương đó được giữ nguyên.",
                    "If every image in a chapter matches, the chapter is kept as is.",
                ));
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label(t("Số ảnh tải cùng lúc:", "Images downloaded at once:"))
                    .on_hover_text(t(
                        "Số ảnh của một chương được tải song song. Cao hơn thì nhanh hơn nhưng dễ bị trang web chặn.",
                        "How many images of a chapter download in parallel. Higher is faster but sites may block you.",
                    ));
                ui.add(egui::DragValue::new(&mut self.workers).range(1..=8));
            });
        });

        ui.add_space(8.0);
        let n_sel = self.selected.iter().filter(|s| **s).count();
        if self.job_rx.is_some() {
            if ui.add_sized([ui.available_width(), 32.0], egui::Button::new(t("■ Dừng", "■ Stop"))).clicked() {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if self.cancel.load(Ordering::Relaxed) {
                ui.weak(t("Đang dừng sau khi xong ảnh hiện tại...", "Stopping after the current images finish..."));
            }
        } else {
            let can = self.series.is_some() && n_sel > 0 && self.inspect_rx.is_none();
            let label = if self.merge {
                tf!("⬇ Tải {n_sel} chương và gộp thành 1 file", "⬇ Download {n_sel} chapters into 1 file")
            } else {
                tf!("⬇ Tải {n_sel} chương", "⬇ Download {n_sel} chapters")
            };
            if ui.add_enabled(can, egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 32.0))).clicked() {
                self.start_job();
            }
        }
    }

    fn ui_chapters(&mut self, ui: &mut egui::Ui) {
        let Some(series) = &self.series else {
            ui.centered_and_justified(|ui| {
                ui.weak(t(
                    "Nhập địa chỉ trang truyện rồi bấm \"Kiểm tra\" để xem danh sách chương.",
                    "Enter a comic page address and press \"Check\" to see the chapter list.",
                ));
            });
            return;
        };
        let total = series.chapters.len();
        let n_sel = self.selected.iter().filter(|s| **s).count();
        ui.heading(tf!("Danh sách chương ({n_sel}/{total} đã chọn)", "Chapters ({n_sel}/{total} selected)"));

        let busy = self.busy();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label(t("Lọc:", "Filter:"));
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(t("tên chương / nhóm", "chapter name / group")).desired_width(160.0));
                let vis = self.visible();
                if ui.button(t("Chọn hết", "Select all")).on_hover_text(t("Chọn mọi chương đang hiển thị", "Select every chapter shown")).clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = true);
                }
                if ui.button(t("Bỏ chọn hết", "Select none")).clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = false);
                }
                if ui.button(t("Đảo", "Invert")).clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = !self.selected[i]);
                }
            });
            ui.horizontal(|ui| {
                ui.label(t("Chọn theo số thứ tự: từ", "Select by number: from"));
                ui.add(egui::DragValue::new(&mut self.range_from).range(1..=total));
                ui.label(t("đến", "to"));
                ui.add(egui::DragValue::new(&mut self.range_to).range(1..=total));
                if ui.button(t("Chỉ chọn khoảng này", "Select only this range")).clicked() {
                    let (a, b) = (self.range_from.min(self.range_to), self.range_from.max(self.range_to));
                    for (i, s) in self.selected.iter_mut().enumerate() {
                        *s = (a..=b).contains(&(i + 1));
                    }
                }
            });
        });
        ui.separator();

        let vis = self.visible();
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let series = self.series.as_ref().unwrap();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, vis.len(), |ui, range| {
            for &i in &vis[range] {
                let ch = &series.chapters[i];
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy, |ui| ui.checkbox(&mut self.selected[i], ""));
                    ui.monospace(format!("{:>4}", i + 1));
                    ui.label(chapter_display(ch));
                    if !ch.group.is_empty() {
                        ui.weak(format!("[{}]", ch.group));
                    }
                });
            }
        });
    }

    fn ui_bottom(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        if self.job_rx.is_some() || self.result.is_some() || !self.log.is_empty() {
            let text = if self.job_rx.is_some() { tf!("Đang tải: {}", "Downloading: {}", self.progress_text) } else { String::new() };
            ui.add(egui::ProgressBar::new(self.progress).show_percentage().text(text));
        }
        if let Some(r) = &self.result {
            ui.colored_label(ui.visuals().strong_text_color(), r);
        }
        egui::ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
            for l in &self.log {
                if l.starts_with("[LỖI]") || l.starts_with("[ERROR]") {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), l);
                } else {
                    ui.monospace(l);
                }
            }
        });
        ui.add_space(4.0);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        if self.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        egui::Panel::top("top").show(ui, |ui| self.ui_top(ui));
        egui::Panel::bottom("bottom").resizable(true).show(ui, |ui| self.ui_bottom(ui));
        egui::Panel::right("options")
            .resizable(false)
            .exact_size(420.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| self.ui_options(ui))
            });
        egui::CentralPanel::default().show(ui, |ui| self.ui_chapters(ui));
    }
}

// ------------------------------------------------------------ dòng lệnh (kiểm thử)

fn cli(a: &[String]) {
    let usage = "cbz_downloader --cli <url> <thư_mục> <each|merge> <KB, 0=tắt> [edges] [từ-đến]";
    let (Some(url), Some(out), Some(mode), Some(kb)) = (a.first(), a.get(1), a.get(2), a.get(3)) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let http = sites::client();
    let series = match sites::inspect(&http, url) {
        Ok(s) => s,
        Err(e) => {
            println!("KHÔNG TẢI ĐƯỢC: {e}");
            return;
        }
    };
    println!("{} — {} chương", series.title, series.chapters.len());
    for (i, c) in series.chapters.iter().enumerate() {
        println!("  {:>3}. {} [{}] {}", i + 1, c.title, c.group, c.url);
    }
    let kb: f64 = kb.parse().unwrap_or(0.0);
    let edges = a.iter().any(|x| x == "edges");
    // above=<KB>: luật "lớn hơn"
    let above: f64 = a.iter().find_map(|x| x.strip_prefix("above=")).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    if a.iter().any(|x| x == "en") {
        i18n::set_en(true);
    }
    let mut chapters = series.chapters.clone();
    if let Some(r) = a.iter().find(|x| x.contains('-') && x.chars().all(|c| c.is_ascii_digit() || c == '-')) {
        let (x, y) = r.split_once('-').unwrap();
        let (x, y): (usize, usize) = (x.parse().unwrap_or(1), y.parse().unwrap_or(chapters.len()));
        chapters = chapters[x - 1..y.min(chapters.len())].to_vec();
    }
    let job = Job {
        name: series.title.clone(),
        series,
        chapters,
        out_dir: PathBuf::from(out),
        merge: mode == "merge",
        overwrite: false,
        ad: AdFilter {
            below_enabled: kb > 0.0,
            below_kb: kb,
            above_enabled: above > 0.0,
            above_kb: above,
            edges_only: edges,
            save_removed: true,
        },
        workers: 4,
    };
    let (tx, rx) = channel();
    let h = thread::spawn(move || download::run(job, tx, Arc::new(AtomicBool::new(false))));
    for m in rx {
        match m {
            Msg::Log(s) => println!("{s}"),
            Msg::Progress { .. } => {}
            Msg::Finished(s) => println!("== {s}"),
        }
    }
    h.join().unwrap();
}
