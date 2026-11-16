//! 看板 HTTP 服务：内嵌 dashboard.html + 两个只读 JSON 接口。
//!
//! 只监听本机、只读文件、每次请求重新读盘；看板靠 10 秒轮询拿新数据，不需要缓存。

use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::store::Paths;
use crate::types::BotStatus;
use crate::util;

/// 页面内联了 script/style，CSP 必须放行 inline，否则整页白屏。
pub const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

const CSP: &str = "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'";

/// 心跳超过该秒数视为机器人已停止（与 Python 版一致）。
const BEAT_TIMEOUT_SECS: i64 = 180;

/// 请求路径的分发结果；抽成纯函数，便于不起服务器就覆盖全部路由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseKind {
    Dashboard,
    State,
    Health,
    NotFound,
}

pub fn route(path: &str) -> ResponseKind {
    // 查询串不参与路由，与 Python 的 `self.path.split("?")[0]` 一致
    let path = path.split('?').next().unwrap_or("");
    match path {
        "/" | "/index.html" | "/dashboard.html" => ResponseKind::Dashboard,
        "/api/state" => ResponseKind::State,
        "/api/health" => ResponseKind::Health,
        _ => ResponseKind::NotFound,
    }
}

pub fn serve(paths: &Paths, host: &str, port: u16) -> AppResult<()> {
    let addr = format!("{host}:{port}");
    let server = tiny_http::Server::http(&addr).map_err(|err| {
        AppError::Invalid(format!("端口 {port} 已被占用或无法监听 {addr}: {err}"))
    })?;
    util::log(&format!("[看板] 服务已启动: http://{addr}/"));
    util::log(&format!("[看板] 数据源: {}", paths.state().display()));
    // 与 bot 共用同一套 SIGINT 标志：两处各装一个 handler 会互相覆盖
    util::install_interrupt_handler();
    // 看板是只读服务，退出时没有需要落盘的东西；用固定超时轮询只是为了及时响应中断
    loop {
        if util::interrupted() {
            break;
        }
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(request)) => handle(request, paths),
            Ok(None) => {}
            Err(err) => return Err(AppError::Io(err)),
        }
    }
    util::log("[看板] 服务已停止");
    Ok(())
}

/// 机器人是否在线：心跳文件存在、可解析且未超时。
///
/// 心跳里是**本地时间无时区**的 ISO 串，必须按本地时间比较，不能当 UTC。
pub fn bot_status(paths: &Paths, state: &Value, now: SystemTime) -> BotStatus {
    let last_scan = state
        .get("last_scan")
        .and_then(|item| item.get("time"))
        .and_then(|item| item.as_str())
        .filter(|text| !text.is_empty())
        .map(|text| text.to_string());

    let Some(beat) = read_json(&paths.heartbeat()) else {
        return BotStatus {
            running: false,
            last_beat: None,
            last_scan,
        };
    };
    let Some(last_beat) = beat
        .get("last_beat")
        .and_then(|item| item.as_str())
        .filter(|text| !text.is_empty())
        .map(|text| text.to_string())
    else {
        return BotStatus {
            running: false,
            last_beat: None,
            last_scan,
        };
    };
    let Some(parsed) = util::parse_datetime(&last_beat) else {
        // 时间戳坏了也把原文回给前端，便于人工排查
        return BotStatus {
            running: false,
            last_beat: Some(last_beat),
            last_scan,
        };
    };
    let now_local: chrono::NaiveDateTime =
        chrono::DateTime::<chrono::Local>::from(now).naive_local();
    let running = (now_local - parsed).num_seconds() < BEAT_TIMEOUT_SECS;
    BotStatus {
        running,
        last_beat: Some(last_beat.clone()),
        last_scan: last_scan.or(Some(last_beat)),
    }
}

fn handle(request: tiny_http::Request, paths: &Paths) {
    if request.method() != &tiny_http::Method::Get {
        send(request, 405, b"Method Not Allowed".to_vec(), "text/plain");
        return;
    }
    match route(request.url()) {
        ResponseKind::Dashboard => send(
            request,
            200,
            DASHBOARD_HTML.as_bytes().to_vec(),
            "text/html; charset=utf-8",
        ),
        ResponseKind::State => send(
            request,
            200,
            state_payload(paths, SystemTime::now()),
            "application/json; charset=utf-8",
        ),
        ResponseKind::Health => send(
            request,
            200,
            health_payload(SystemTime::now()),
            "application/json",
        ),
        ResponseKind::NotFound => send(request, 404, b"Not Found".to_vec(), "text/plain"),
    }
}

fn send(request: tiny_http::Request, status: u16, body: Vec<u8>, content_type: &str) {
    let mut response = tiny_http::Response::from_data(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Security-Policy", CSP),
    ] {
        match tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            Ok(header) => response = response.with_header(header),
            // 常量头理论上不会非法；真出问题也不让整条响应失败
            Err(_) => util::log(&format!("[警告] 响应头 {name} 非法，已跳过")),
        }
    }
    if let Err(err) = request.respond(response) {
        util::log(&format!("[警告] 响应发送失败: {err}"));
    }
}

fn state_payload(paths: &Paths, now: SystemTime) -> Vec<u8> {
    let state = read_json(&paths.state()).unwrap_or(Value::Null);
    let experience = read_json(&paths.experience()).unwrap_or(Value::Null);
    let bot = bot_status(paths, &state, now);
    let payload = json!({
        "ok": true,
        "ts": unix_ts(now),
        "bot": bot,
        "state": state,
        "experience": experience,
    });
    match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(err) => {
            util::log(&format!("[警告] /api/state 序列化失败: {err}"));
            b"{\"ok\":false}".to_vec()
        }
    }
}

fn health_payload(now: SystemTime) -> Vec<u8> {
    let payload = json!({"ok": true, "service": "quant-dashboard", "ts": unix_ts(now)});
    match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(_) => b"{\"ok\":false}".to_vec(),
    }
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn unix_ts(now: SystemTime) -> f64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|delta| delta.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-dash-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        Paths { base_dir: base }
    }

    fn write_beat(paths: &Paths, last_beat: &str) {
        let body = serde_json::json!({"last_beat": last_beat, "pid": 1});
        std::fs::write(
            paths.heartbeat(),
            serde_json::to_vec(&body).expect("心跳可序列化"),
        )
        .expect("写心跳");
    }

    #[test]
    fn route_table() {
        assert_eq!(route("/"), ResponseKind::Dashboard);
        assert_eq!(route("/index.html"), ResponseKind::Dashboard);
        assert_eq!(route("/dashboard.html"), ResponseKind::Dashboard);
        assert_eq!(route("/api/state"), ResponseKind::State);
        assert_eq!(route("/api/state?t=1"), ResponseKind::State);
        assert_eq!(route("/api/health"), ResponseKind::Health);
        assert_eq!(route("/robots.txt"), ResponseKind::NotFound);
        assert_eq!(route("/api"), ResponseKind::NotFound);
        assert_eq!(route(""), ResponseKind::NotFound);
    }

    #[test]
    fn bot_status_without_heartbeat_is_stopped() {
        let paths = temp_paths("no-beat");
        let status = bot_status(&paths, &Value::Null, SystemTime::now());
        assert!(!status.running);
        assert!(status.last_beat.is_none());
        assert!(status.last_scan.is_none());
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn bot_status_fresh_heartbeat_is_running() {
        let paths = temp_paths("fresh");
        write_beat(&paths, &util::now_iso());
        let status = bot_status(&paths, &serde_json::json!({}), SystemTime::now());
        assert!(status.running);
        assert!(status.last_beat.is_some());
        // 没有 last_scan 时回落到心跳时间
        assert_eq!(status.last_scan, status.last_beat);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn bot_status_stale_heartbeat_is_stopped() {
        let paths = temp_paths("stale");
        let stale = (util::now_local() - chrono::Duration::seconds(BEAT_TIMEOUT_SECS + 60))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        write_beat(&paths, &stale);
        let status = bot_status(&paths, &serde_json::json!({}), SystemTime::now());
        assert!(!status.running);
        assert_eq!(status.last_beat.as_deref(), Some(stale.as_str()));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn bot_status_reads_last_scan_from_state() {
        let paths = temp_paths("lastscan");
        write_beat(&paths, &util::now_iso());
        let state = serde_json::json!({"last_scan": {"time": "2026-11-16T14:30:00"}});
        let status = bot_status(&paths, &state, SystemTime::now());
        assert_eq!(status.last_scan.as_deref(), Some("2026-11-16T14:30:00"));
        assert!(status.running);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn bot_status_tolerates_corrupt_heartbeat() {
        let paths = temp_paths("corrupt-beat");
        std::fs::write(paths.heartbeat(), "{broken").expect("写坏心跳");
        let status = bot_status(&paths, &Value::Null, SystemTime::now());
        assert!(!status.running);
        assert!(status.last_beat.is_none());

        // 心跳文件能读但时间戳写坏了：原文回给前端，running 保持 false
        write_beat(&paths, "昨天下午");
        let status = bot_status(&paths, &Value::Null, SystemTime::now());
        assert!(!status.running);
        assert_eq!(status.last_beat.as_deref(), Some("昨天下午"));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn state_payload_shape() {
        let paths = temp_paths("payload");
        // 没有任何数据文件时 state/experience 必须是 null 而不是报错
        let body = state_payload(&paths, SystemTime::now());
        let value: Value = serde_json::from_slice(&body).expect("payload 是合法 JSON");
        assert_eq!(value["ok"], Value::Bool(true));
        assert!(value["ts"].as_f64().unwrap_or(0.0) > 1_600_000_000.0);
        assert_eq!(value["state"], Value::Null);
        assert_eq!(value["experience"], Value::Null);
        assert_eq!(value["bot"]["running"], Value::Bool(false));
        assert!(value["bot"]["last_beat"].is_null());

        // 有文件时原样透传，不裁剪字段
        std::fs::write(paths.state(), r#"{"cash": 1234.5, "positions": {}}"#).expect("写 state");
        std::fs::write(paths.experience(), r#"{"version": 2}"#).expect("写经验");
        let value: Value =
            serde_json::from_slice(&state_payload(&paths, SystemTime::now())).expect("合法 JSON");
        assert_eq!(value["state"]["cash"], serde_json::json!(1234.5));
        assert_eq!(value["experience"]["version"], serde_json::json!(2));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn health_payload_shape() {
        let value: Value =
            serde_json::from_slice(&health_payload(SystemTime::now())).expect("合法 JSON");
        assert_eq!(value["ok"], Value::Bool(true));
        assert_eq!(value["service"], serde_json::json!("quant-dashboard"));
        assert!(value["ts"].is_number());
    }

    #[test]
    fn dashboard_html_is_embedded() {
        assert!(DASHBOARD_HTML.contains("<html"));
        assert!(DASHBOARD_HTML.contains("/api/state"));
        // 页面必须自带内联样式，CSP 才会允许渲染
        assert!(DASHBOARD_HTML.contains("<style"));
    }

    #[test]
    fn occupied_port_reports_readable_error() {
        let paths = temp_paths("busy-port");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占住一个端口");
        let port = listener.local_addr().expect("本地地址").port();
        let err = serve(&paths, "127.0.0.1", port).expect_err("端口被占必须报错而不是 panic");
        let message = err.to_string();
        assert!(message.contains(&port.to_string()), "错误信息: {message}");
        assert!(
            message.contains("已被占用") || message.contains("无法监听"),
            "错误信息: {message}"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    /// 真起服务的冒烟测试；默认跳过，需要 `cargo test -- --ignored` 手动跑。
    #[test]
    #[ignore = "需要真实监听端口，手动用 --ignored 运行"]
    fn live_http_routes() {
        let paths = temp_paths("live");
        std::fs::write(paths.state(), r#"{"cash": 50000.0, "positions": {}}"#).expect("写 state");
        // 先占位取一个空闲端口再释放；冒烟测试接受被抢占的极小概率
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("取空闲端口");
            listener.local_addr().expect("本地地址").port()
        };
        let serve_paths = Paths {
            base_dir: paths.base_dir.clone(),
        };
        std::thread::spawn(move || {
            let _ = serve(&serve_paths, "127.0.0.1", port);
        });
        std::thread::sleep(Duration::from_millis(300));

        let (status, headers, body) = http_get(port, "/api/health");
        assert!(status.contains("200"), "状态行: {status}");
        let lower = headers.to_lowercase();
        assert!(lower.contains("cache-control: no-store"));
        assert!(lower.contains("x-content-type-options: nosniff"));
        assert!(lower.contains("content-security-policy"));
        assert!(body.contains("quant-dashboard"));

        let (status, headers, body) = http_get(port, "/");
        assert!(status.contains("200"), "状态行: {status}");
        assert!(headers.contains("text/html; charset=utf-8"));
        assert!(body.contains("/api/state"));

        let (status, _, body) = http_get(port, "/api/state");
        assert!(status.contains("200"), "状态行: {status}");
        assert!(body.contains("\"ok\":true"));
        assert!(body.contains("\"bot\""));

        let (status, headers, body) = http_get(port, "/nope");
        assert!(status.contains("404"), "状态行: {status}");
        assert!(headers.contains("text/plain"));
        assert_eq!(body, "Not Found");
    }

    fn http_get(port: u16, path: &str) -> (String, String, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("连接看板端口");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("发送请求");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).expect("读取响应");
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap_or("").to_string();
        let headers = lines.collect::<Vec<_>>().join("\n");
        (status, headers, body.to_string())
    }
}
