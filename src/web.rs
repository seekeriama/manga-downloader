//! Chế độ trình duyệt: chạy máy chủ nhỏ trên 127.0.0.1 (chỉ máy này truy cập được)
//! và mở giao diện web trong trình duyệt mặc định. Dùng lại toàn bộ phần tải/lọc/ghi CBZ.

use crate::download::{self, AdFilter, Job, Msg};
use crate::sites::{self, Series};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX_HTML: &str = include_str!("web/index.html");

#[derive(Default)]
struct JobState {
    running: bool,
    done: usize,
    total: usize,
    text: String,
    log: Vec<String>,
    result: Option<String>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    series: Option<Series>,
    job: JobState,
}

type Shared = Arc<Mutex<State>>;

pub fn run(open_browser: bool) {
    // nếu bản web đã chạy sẵn (vd. lỡ đóng tab mà chưa tắt) thì chỉ mở lại trình duyệt
    let probe = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_millis(800)).build();
    if let Ok(c) = probe {
        let running = c
            .get("http://127.0.0.1:8787/api/defaults")
            .send()
            .ok()
            .and_then(|r| r.text().ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v.get("out_dir").is_some());
        if running {
            if open_browser {
                open_url("http://127.0.0.1:8787/");
            }
            return;
        }
    }
    // thử cổng cố định trước (dễ nhớ), nếu bận thì để hệ điều hành chọn cổng trống
    let server = [8787u16, 8788, 8789, 0]
        .iter()
        .find_map(|p| Server::http(("127.0.0.1", *p)).ok())
        .expect("không mở được cổng mạng nội bộ");
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://127.0.0.1:{port}/");
    println!("Giao diện web: {url}  (đóng bằng nút \"Tắt phần mềm\" trên trang, hoặc Ctrl+C)");
    if open_browser {
        open_url(&url);
    }

    let state: Shared = Arc::new(Mutex::new(State::default()));
    let allowed_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    for req in server.incoming_requests() {
        let state = state.clone();
        let allowed = allowed_hosts.clone();
        thread::spawn(move || handle(req, state, &allowed));
    }
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        println!("Hãy tự mở trình duyệt và vào {url}");
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn send_json(req: Request, code: u16, v: Value) {
    let resp = Response::from_string(v.to_string())
        .with_status_code(code)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(resp);
}

fn handle(mut req: Request, state: Shared, allowed_hosts: &[String]) {
    // chặn trang web lạ gọi vào máy chủ nội bộ (DNS rebinding)
    let host_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| allowed_hosts.iter().any(|a| a == h.value.as_str()))
        .unwrap_or(false);
    if !host_ok {
        let _ = req.respond(Response::from_string("forbidden").with_status_code(403));
        return;
    }

    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = req.method().clone();

    if method == Method::Get && (path == "/" || path == "/index.html") {
        let resp = Response::from_string(INDEX_HTML)
            .with_header(header("Content-Type", "text/html; charset=utf-8"))
            .with_header(header("Cache-Control", "no-store"));
        let _ = req.respond(resp);
        return;
    }

    let mut body = String::new();
    if method == Method::Post {
        // chỉ nhận JSON: trình duyệt không gửi được JSON chéo trang mà không bị chặn
        let is_json = req
            .headers()
            .iter()
            .any(|h| h.field.equiv("Content-Type") && h.value.as_str().starts_with("application/json"));
        if !is_json || req.as_reader().take(1 << 20).read_to_string(&mut body).is_err() {
            send_json(req, 400, json!({"error": "yêu cầu không hợp lệ"}));
            return;
        }
    }
    let input: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

    match (method, path) {
        (Method::Get, "/api/defaults") => {
            send_json(req, 200, json!({"out_dir": crate::default_out_dir().display().to_string()}))
        }
        (Method::Post, "/api/inspect") => {
            let url = input["url"].as_str().unwrap_or("").to_string();
            if state.lock().unwrap().job.running {
                return send_json(req, 409, json!({"error": "Đang tải, hãy đợi xong hoặc bấm Dừng."}));
            }
            let http = sites::client();
            match sites::inspect(&http, &url) {
                Ok(s) => {
                    let v = json!({
                        "title": s.title,
                        "site": s.site,
                        "chapters": s.chapters.iter().map(|c| json!({"title": c.title, "group": c.group})).collect::<Vec<_>>(),
                    });
                    state.lock().unwrap().series = Some(s);
                    send_json(req, 200, v)
                }
                Err(e) => send_json(req, 200, json!({"error": e})),
            }
        }
        (Method::Post, "/api/start") => {
            let r = start_job(&state, &input);
            match r {
                Ok(()) => send_json(req, 200, json!({"ok": true})),
                Err(e) => send_json(req, 400, json!({"error": e})),
            }
        }
        (Method::Post, "/api/cancel") => {
            state.lock().unwrap().job.cancel.store(true, Ordering::Relaxed);
            send_json(req, 200, json!({"ok": true}))
        }
        (Method::Get, "/api/status") => {
            let since: usize = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("since="))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let st = state.lock().unwrap();
            let j = &st.job;
            let v = json!({
                "running": j.running,
                "cancelling": j.running && j.cancel.load(Ordering::Relaxed),
                "done": j.done,
                "total": j.total,
                "text": j.text,
                "log": j.log.get(since.min(j.log.len())..).unwrap_or(&[]),
                "log_len": j.log.len(),
                "result": j.result,
            });
            drop(st);
            send_json(req, 200, v)
        }
        (Method::Post, "/api/pick-folder") => {
            let cur = input["current"].as_str().unwrap_or("").to_string();
            let mut dlg = rfd::FileDialog::new();
            if !cur.is_empty() {
                dlg = dlg.set_directory(&cur);
            }
            let picked = dlg.pick_folder().map(|p| p.display().to_string());
            send_json(req, 200, json!({"path": picked}))
        }
        (Method::Post, "/api/quit") => {
            state.lock().unwrap().job.cancel.store(true, Ordering::Relaxed);
            send_json(req, 200, json!({"ok": true}));
            thread::sleep(Duration::from_millis(300));
            std::process::exit(0);
        }
        _ => send_json(req, 404, json!({"error": "không có"})),
    }
}

fn start_job(state: &Shared, v: &Value) -> Result<(), String> {
    let mut st = state.lock().unwrap();
    if st.job.running {
        return Err("Đang có một lượt tải chạy.".into());
    }
    let series = st.series.clone().ok_or("Hãy bấm Kiểm tra trước.")?;
    let idx: Vec<usize> = v["indices"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect())
        .unwrap_or_default();
    let chapters: Vec<_> = idx.iter().filter_map(|&i| series.chapters.get(i).cloned()).collect();
    if chapters.is_empty() {
        return Err("Chưa chọn chương nào.".into());
    }
    let out_dir = v["out_dir"].as_str().unwrap_or("").trim().to_string();
    if out_dir.is_empty() {
        return Err("Hãy nhập thư mục lưu.".into());
    }
    let name = v["name"].as_str().unwrap_or("").trim();
    let job = Job {
        name: if name.is_empty() { series.title.clone() } else { name.to_string() },
        series,
        chapters,
        out_dir: PathBuf::from(out_dir),
        merge: v["merge"].as_bool().unwrap_or(false),
        overwrite: v["overwrite"].as_bool().unwrap_or(false),
        ad: AdFilter {
            enabled: v["ad_enabled"].as_bool().unwrap_or(false),
            below_kb: v["ad_kb"].as_f64().unwrap_or(100.0),
            edges_only: v["ad_edges_only"].as_bool().unwrap_or(true),
            save_removed: v["ad_save"].as_bool().unwrap_or(false),
        },
        workers: v["workers"].as_u64().unwrap_or(4).clamp(1, 8) as usize,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    st.job = JobState { running: true, total: job.chapters.len(), cancel: cancel.clone(), ..Default::default() };
    drop(st);

    let (tx, rx) = channel();
    thread::spawn(move || download::run(job, tx, cancel));
    let state = state.clone();
    thread::spawn(move || {
        for m in rx {
            let mut st = state.lock().unwrap();
            match m {
                Msg::Log(s) => st.job.log.push(s),
                Msg::Progress { done, total, text } => {
                    st.job.done = done;
                    st.job.total = total;
                    st.job.text = text;
                }
                Msg::Finished(s) => {
                    st.job.result = Some(s);
                    st.job.running = false;
                    st.job.done = st.job.total;
                    st.job.text.clear();
                }
            }
        }
        state.lock().unwrap().job.running = false;
    });
    Ok(())
}
