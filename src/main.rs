// Ẩn cửa sổ console đen khi chạy bản release trên Windows
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod download;
mod sites;
mod web;

use download::{AdFilter, Job, Msg};
use eframe::egui;
use sites::Series;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() -> eframe::Result {
    // Chế độ dòng lệnh (dùng để kiểm thử): truyen_dl --cli <url> <thư_mục> <each|merge> <KB> [edges] [chọn: 1-5]
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        cli(&args[2..]);
        return Ok(());
    }
    // Bản mở bằng trình duyệt: truyen_dl --web  (thêm --no-open để không tự mở trình duyệt)
    if args.iter().any(|a| a == "--web") {
        web::run(!args.iter().any(|a| a == "--no-open"));
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Trình tải truyện CBZ")
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([720.0, 560.0]),
        ..Default::default()
    };
    // truyen_dl <url>: mở giao diện với địa chỉ điền sẵn và tự kiểm tra
    let start_url = args.get(1).filter(|a| !a.starts_with('-')).cloned();
    eframe::run_native(
        "Trình tải truyện CBZ",
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
    Ok(String),
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
    ad_enabled: bool,
    ad_kb: f64,
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
            ad_enabled: false,
            ad_kb: 100.0,
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
                enabled: self.ad_enabled,
                below_kb: self.ad_kb,
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
                        self.status = Status::Ok(format!("Tải được: {} chương từ {}", n, s.site));
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
            ui.heading("Trình tải truyện CBZ");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("🌐 Mở bản trình duyệt").on_hover_text("Mở giao diện này trong trình duyệt web").clicked()
                    && let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).arg("--web").spawn();
                    }
            });
        });
        ui.horizontal(|ui| {
            ui.label("Địa chỉ trang truyện:");
            let edit = ui.add_enabled(
                !self.busy(),
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("vd. https://truyenggvn.com/truyen-tranh/hanh-trinh-cua-uduchi-5804")
                    .desired_width(ui.available_width() - 110.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(!self.busy(), egui::Button::new("Kiểm tra")).clicked() || enter {
                self.start_inspect();
            }
        });
        match &self.status {
            Status::Idle => {
                ui.weak("Hỗ trợ: các trang dạng truyenggvn.com và các trang Blogger (vd. pokemonspecial.com).");
            }
            Status::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Đang kiểm tra trang và đọc danh sách chương...");
                });
            }
            Status::Ok(s) => {
                ui.colored_label(egui::Color32::from_rgb(40, 150, 70), format!("✔ {s}"));
            }
            Status::Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), format!("✘ Không tải được. {e}"));
            }
        }
        ui.add_space(4.0);
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        let enabled = !self.busy();
        ui.add_enabled_ui(enabled, |ui| {
            ui.heading("Tùy chọn lưu");
            ui.label("Tên truyện (tên file):");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(ui.available_width() - 8.0));
            ui.label("Thư mục lưu:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(ui.available_width() - 70.0));
                if ui.button("Chọn…").clicked()
                    && let Some(p) = rfd::FileDialog::new().set_directory(&self.out_dir).pick_folder() {
                        self.out_dir = p.display().to_string();
                    }
            });
            ui.add_space(4.0);
            ui.radio_value(&mut self.merge, false, "Lưu từng chương thành file riêng");
            ui.radio_value(&mut self.merge, true, "Gộp tất cả chương đã chọn thành 1 file CBZ");
            ui.weak(if self.merge {
                format!("→ {}.cbz", download::safe_name(&self.name))
            } else {
                format!("→ thư mục \"{}\", chương đã có sẽ được bỏ qua", download::safe_name(&self.name))
            });
            ui.checkbox(&mut self.overwrite, "Ghi đè nếu file đã có");

            ui.separator();
            ui.heading("Loại bỏ trang quảng cáo");
            ui.checkbox(&mut self.ad_enabled, "Bật lọc quảng cáo");
            ui.add_enabled_ui(self.ad_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Loại ảnh nhỏ hơn");
                    ui.add(egui::DragValue::new(&mut self.ad_kb).range(1.0..=10000.0).speed(1.0).suffix(" KB"));
                });
                ui.radio_value(&mut self.ad_edges_only, true, "Chỉ xét ảnh đầu và ảnh cuối mỗi chương");
                ui.radio_value(&mut self.ad_edges_only, false, "Xét mọi ảnh trong chương");
                ui.checkbox(&mut self.ad_save, "Lưu ảnh bị loại vào thư mục _anh_bi_loai để kiểm tra");
                ui.weak("Nếu mọi ảnh của một chương đều nhỏ hơn ngưỡng, chương đó được giữ nguyên.");
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Số ảnh tải cùng lúc:");
                ui.add(egui::DragValue::new(&mut self.workers).range(1..=8));
            });
        });

        ui.add_space(8.0);
        let n_sel = self.selected.iter().filter(|s| **s).count();
        if self.job_rx.is_some() {
            if ui.add_sized([ui.available_width(), 32.0], egui::Button::new("■ Dừng")).clicked() {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if self.cancel.load(Ordering::Relaxed) {
                ui.weak("Đang dừng sau khi xong ảnh hiện tại...");
            }
        } else {
            let can = self.series.is_some() && n_sel > 0 && self.inspect_rx.is_none();
            let label = if self.merge {
                format!("⬇ Tải {n_sel} chương và gộp thành 1 file")
            } else {
                format!("⬇ Tải {n_sel} chương")
            };
            if ui.add_enabled(can, egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 32.0))).clicked() {
                self.start_job();
            }
        }
    }

    fn ui_chapters(&mut self, ui: &mut egui::Ui) {
        let Some(series) = &self.series else {
            ui.centered_and_justified(|ui| {
                ui.weak("Nhập địa chỉ trang truyện rồi bấm \"Kiểm tra\" để xem danh sách chương.");
            });
            return;
        };
        let total = series.chapters.len();
        let n_sel = self.selected.iter().filter(|s| **s).count();
        ui.heading(format!("Danh sách chương ({n_sel}/{total} đã chọn)"));

        let busy = self.busy();
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("Lọc:");
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("tên chương / nhóm").desired_width(160.0));
                let vis = self.visible();
                if ui.button("Chọn hết").on_hover_text("Chọn mọi chương đang hiển thị").clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = true);
                }
                if ui.button("Bỏ chọn hết").clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = false);
                }
                if ui.button("Đảo").clicked() {
                    vis.iter().for_each(|&i| self.selected[i] = !self.selected[i]);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Chọn theo số thứ tự: từ");
                ui.add(egui::DragValue::new(&mut self.range_from).range(1..=total));
                ui.label("đến");
                ui.add(egui::DragValue::new(&mut self.range_to).range(1..=total));
                if ui.button("Chỉ chọn khoảng này").clicked() {
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
                    ui.label(&ch.title);
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
            let text = if self.job_rx.is_some() { format!("Đang tải: {}", self.progress_text) } else { String::new() };
            ui.add(egui::ProgressBar::new(self.progress).show_percentage().text(text));
        }
        if let Some(r) = &self.result {
            ui.colored_label(ui.visuals().strong_text_color(), r);
        }
        egui::ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
            for l in &self.log {
                if l.starts_with("[LỖI]") {
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
            .exact_size(370.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| self.ui_options(ui))
            });
        egui::CentralPanel::default().show(ui, |ui| self.ui_chapters(ui));
    }
}

// ------------------------------------------------------------ dòng lệnh (kiểm thử)

fn cli(a: &[String]) {
    let usage = "truyen_dl --cli <url> <thư_mục> <each|merge> <KB, 0=tắt> [edges] [từ-đến]";
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
        ad: AdFilter { enabled: kb > 0.0, below_kb: kb, edges_only: edges, save_removed: true },
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
