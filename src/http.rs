//! HTTP 抓取层。
//!
//! 两个 `Agent`（快/慢）共用同一套 TLS 与重试策略：行情、列表这类短请求用 fast，
//! 大模型推理用 slow。`http_status_as_error(false)` 让 4xx/5xx 以正常响应返回，
//! 从而能按状态码决定"重试还是立即失败"——否则限流(429)和网关抖动(502)会被
//! 当成同一类错误。
//!
//! 批量抓取用 `thread::scope` + `mpsc`：线程只负责发请求并把结果回传，缓存由
//! 主线程统一写入，避免多线程争锁。

use std::sync::mpsc;
use std::time::Duration;

use crate::error::{AppError, AppResult};

/// 值得重试的状态码：超时、限流、网关类。
const RETRY_STATUS: [u16; 6] = [408, 429, 500, 502, 503, 504];
/// 单次调用最多尝试 3 次（与 Python 版 `retries=3` 一致）。
const MAX_ATTEMPTS: usize = 3;
/// 并发线程上限，对齐 Python `fetch_many(max_workers=8)`。
const MAX_WORKERS: usize = 8;
/// 失败响应摘要写入错误信息时的截断长度。
const BODY_SNIPPET: usize = 200;

/// 一次待抓取的 GET 请求。
#[derive(Clone, Debug)]
pub struct GetRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// 是否走慢通道（大模型相关请求）。
    pub slow: bool,
}

impl GetRequest {
    pub fn new(url: impl Into<String>) -> GetRequest {
        GetRequest {
            url: url.into(),
            headers: Vec::new(),
            slow: false,
        }
    }

    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> GetRequest {
        self.headers = headers
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        self
    }
}

pub struct HttpClient {
    fast: ureq::Agent,
    slow: ureq::Agent,
}

fn build_agent(timeout: Duration) -> AppResult<ureq::Agent> {
    // ureq 3 必须显式指定 TLS provider，否则运行期 panic。
    // 同时必须显式选择根证书来源：默认是 WebPki（Mozilla 内置列表），实测在
    // web.ifzq.gtimg.cn 与 search-api-web.eastmoney.com 上会报
    // "native-tls: unable to find any user-specified roots in the final cert chain"。
    // 改用平台信任库（Windows 证书存储 / 系统 CA），与浏览器、curl 的判定一致。
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let config = ureq::config::Config::builder()
        .tls_config(tls)
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

fn backoff(attempt: usize) {
    // 退避 0.4*(n+1) 秒，与 Python `time.sleep(0.4 * (attempt + 1))` 对齐。
    std::thread::sleep(Duration::from_millis(400 * (attempt as u64 + 1)));
}

fn body_snippet(body: &[u8]) -> String {
    let limit = body.len().min(BODY_SNIPPET);
    String::from_utf8_lossy(&body[..limit]).to_string()
}

impl HttpClient {
    pub fn new(fast: Duration, slow: Duration) -> AppResult<Self> {
        Ok(HttpClient {
            fast: build_agent(fast)?,
            slow: build_agent(slow)?,
        })
    }

    fn agent(&self, slow: bool) -> &ureq::Agent {
        if slow {
            &self.slow
        } else {
            &self.fast
        }
    }

    fn get_once(
        &self,
        url: &str,
        headers: &[(String, String)],
        slow: bool,
    ) -> AppResult<(u16, Vec<u8>)> {
        let mut request = self.agent(slow).get(url);
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request
            .call()
            .map_err(|err| AppError::Http(format!("请求失败 {url}: {err}")))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_vec()
            .map_err(|err| AppError::Http(format!("读取响应失败 {url}: {err}")))?;
        Ok((status, body))
    }

    /// GET 原始字节。腾讯行情是 GBK，必须拿到字节自行解码。
    pub fn get_bytes(
        &self,
        url: &str,
        headers: &[(String, String)],
        slow: bool,
    ) -> AppResult<Vec<u8>> {
        let mut last_error: Option<AppError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.get_once(url, headers, slow) {
                Ok((status, body)) => {
                    if (200..300).contains(&status) {
                        return Ok(body);
                    }
                    let snippet = body_snippet(&body);
                    let message = format!("HTTP {status} {url}: {snippet}");
                    last_error = Some(AppError::Http(message));
                    if RETRY_STATUS.contains(&status) && attempt + 1 < MAX_ATTEMPTS {
                        backoff(attempt);
                        continue;
                    }
                    return Err(last_error
                        .unwrap_or_else(|| AppError::Http(format!("HTTP {status} {url}"))));
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt + 1 < MAX_ATTEMPTS {
                        backoff(attempt);
                        continue;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| AppError::Http(format!("请求失败 {url}"))))
    }

    /// GET 文本（UTF-8，非法字节替换为 U+FFFD）。
    pub fn get_text(
        &self,
        url: &str,
        headers: &[(String, String)],
        slow: bool,
    ) -> AppResult<String> {
        let bytes = self.get_bytes(url, headers, slow)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn get_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        slow: bool,
    ) -> AppResult<serde_json::Value> {
        let text = self.get_text(url, headers, slow)?;
        serde_json::from_str(&text)
            .map_err(|err| AppError::Http(format!("响应不是合法 JSON {url}: {err}")))
    }

    /// 并发抓取原始字节。返回值的长度与顺序与 `reqs` 严格一致，单个失败为 `None`。
    /// GBK 数据源（腾讯行情）必须走这条路径，否则中文名会被当成非法 UTF-8。
    pub fn get_bytes_many(&self, reqs: Vec<GetRequest>) -> Vec<Option<Vec<u8>>> {
        let total = reqs.len();
        let mut results: Vec<Option<Vec<u8>>> = vec![None; total];
        if total == 0 {
            return results;
        }

        let workers = total.clamp(1, MAX_WORKERS);
        let chunk = total.div_ceil(workers);
        let (tx, rx) = mpsc::channel::<(usize, Option<Vec<u8>>)>();

        std::thread::scope(|scope| {
            for worker in 0..workers {
                let start = worker * chunk;
                let end = ((worker + 1) * chunk).min(total);
                if start >= end {
                    break;
                }
                let tx = tx.clone();
                let batch = &reqs[start..end];
                scope.spawn(move || {
                    for (offset, request) in batch.iter().enumerate() {
                        let bytes = self
                            .get_bytes(&request.url, &request.headers, request.slow)
                            .ok();
                        // 接收端在主线程，通道无界，发送不会阻塞。
                        let _ = tx.send((start + offset, bytes));
                    }
                });
            }
            drop(tx);
        });

        for (index, bytes) in rx {
            if index < total {
                results[index] = bytes;
            }
        }
        results
    }

    /// 并发抓取文本（UTF-8）。顺序与 `reqs` 一致，单个失败为 `None`。
    pub fn get_text_many(&self, reqs: Vec<GetRequest>) -> Vec<Option<String>> {
        self.get_bytes_many(reqs)
            .into_iter()
            .map(|bytes| bytes.map(|raw| String::from_utf8_lossy(&raw).into_owned()))
            .collect()
    }

    /// POST JSON，返回 `(状态码, 解析后的 body)`。非 2xx 在重试耗尽后返回错误。
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> AppResult<(u16, serde_json::Value)> {
        let mut last_error: Option<AppError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let mut request = self.agent(true).post(url);
            for (key, value) in headers {
                request = request.header(key.as_str(), value.as_str());
            }
            let outcome = request
                .send_json(body)
                .map_err(|err| AppError::Http(format!("请求失败 {url}: {err}")))
                .and_then(|response| {
                    let status = response.status().as_u16();
                    let bytes = response
                        .into_body()
                        .read_to_vec()
                        .map_err(|err| AppError::Http(format!("读取响应失败 {url}: {err}")))?;
                    Ok((status, bytes))
                });

            match outcome {
                Ok((status, bytes)) => {
                    if (200..300).contains(&status) {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        let value = serde_json::from_str(&text).map_err(|err| {
                            AppError::Http(format!(
                                "响应不是合法 JSON {url}: {err}; body={}",
                                body_snippet(&bytes)
                            ))
                        })?;
                        return Ok((status, value));
                    }
                    let snippet = body_snippet(&bytes);
                    last_error = Some(AppError::Http(format!("HTTP {status} {url}: {snippet}")));
                    if RETRY_STATUS.contains(&status) && attempt + 1 < MAX_ATTEMPTS {
                        backoff(attempt);
                        continue;
                    }
                    return Err(last_error
                        .unwrap_or_else(|| AppError::Http(format!("HTTP {status} {url}"))));
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt + 1 < MAX_ATTEMPTS {
                        backoff(attempt);
                        continue;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| AppError::Http(format!("请求失败 {url}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// 本地测试服务：/a、/b 返回文本，/boom 首次 500 之后 200，
    /// 其它路径 404（用于验证非重试错误立即失败）。
    struct TestServer {
        base: String,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start() -> TestServer {
            let server = tiny_http::Server::http("127.0.0.1:0").expect("测试服务启动失败");
            let addr = server
                .server_addr()
                .to_ip()
                .expect("测试服务需要 TCP 地址")
                .to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let mut boom_calls = 0;
                while !stop_flag.load(Ordering::SeqCst) {
                    let request = match server.recv_timeout(Duration::from_millis(50)) {
                        Ok(Some(req)) => req,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let url = request.url().to_string();
                    let response = match url.as_str() {
                        "/a" => tiny_http::Response::from_string("AAA"),
                        "/b" => tiny_http::Response::from_string("BBB"),
                        "/boom" => {
                            boom_calls += 1;
                            if boom_calls == 1 {
                                tiny_http::Response::from_string("boom").with_status_code(500)
                            } else {
                                tiny_http::Response::from_string("RECOVERED")
                            }
                        }
                        _ => tiny_http::Response::from_string("nope").with_status_code(404),
                    };
                    let _ = request.respond(response);
                }
            });
            TestServer {
                base: format!("http://{addr}"),
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{}", self.base, path)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn client() -> HttpClient {
        HttpClient::new(Duration::from_secs(5), Duration::from_secs(10)).expect("构建客户端")
    }

    #[test]
    fn get_text_reads_body() {
        let server = TestServer::start();
        let text = client().get_text(&server.url("/a"), &[], false).unwrap();
        assert_eq!(text, "AAA");
    }

    #[test]
    fn get_text_many_keeps_order_and_turns_failure_into_none() {
        let server = TestServer::start();
        let reqs = vec![
            GetRequest::new(server.url("/a")),
            GetRequest::new(server.url("/missing")),
            GetRequest::new(server.url("/b")),
        ];
        let results = client().get_text_many(reqs);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_deref(), Some("AAA"));
        assert!(results[1].is_none());
        assert_eq!(results[2].as_deref(), Some("BBB"));
    }

    #[test]
    fn get_text_many_handles_empty_input() {
        let results = client().get_text_many(Vec::new());
        assert!(results.is_empty());
    }

    #[test]
    fn retryable_status_is_retried() {
        let server = TestServer::start();
        let text = client().get_text(&server.url("/boom"), &[], false).unwrap();
        assert_eq!(text, "RECOVERED");
    }

    /// 缺 `root_certs(PlatformVerifier)` 时 native-tls 会关掉系统信任库，
    /// 表现为 K 线/新闻静默变空。这条测试专门拦住"简化"构造链的改动。
    #[test]
    fn agent_uses_platform_root_certs() {
        let agent = build_agent(Duration::from_secs(3)).expect("构建 agent");
        let tls = agent.config().tls_config();
        assert!(
            matches!(tls.root_certs(), ureq::tls::RootCerts::PlatformVerifier),
            "根证书必须来自平台信任库，实际是 {:?}",
            tls.root_certs()
        );
        assert!(!tls.disable_verification());
    }

    #[test]
    fn client_construction_configures_native_tls() {
        assert!(HttpClient::new(Duration::from_secs(3), Duration::from_secs(3)).is_ok());
    }
}
