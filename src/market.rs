//! A 股行情数据源（腾讯实时/K 线 + 新浪全市场列表）。
//!
//! 三个数据源全部实测可用：
//! - `qt.gtimg.cn` 批量行情，**GBK 编码**，必须按字节取回再解码；
//! - `web.ifzq.gtimg.cn` 前复权 K 线，支持 `end` 参数截断（回测 as_of 用）；
//! - `vip.stock.finance.sina.com.cn` 全市场列表，UTF-8 JSON。
//!
//! 缓存一律 `Instant + TTL`：行情 15s、K 线 60s、列表 600s。K 线缓存键包含
//! `end_date`，否则回测不同 as_of 之间会互相污染。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use crate::calendar::Calendar;
use crate::http::{GetRequest, HttpClient};
use crate::types::{Bar, MarketSource, Quote, StockInfo};
use crate::util::{build_url, json_number, json_text, log, take_chars, ymd};

const QT_URL: &str = "https://qt.gtimg.cn/q=";
const KLINE_URL: &str = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get";
const SINA_KLINE_URL: &str =
    "https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData";
const STOCK_LIST_URL: &str =
    "https://vip.stock.finance.sina.com.cn/quotes_service/api/json_v2.php/Market_Center.getHQNodeData";
const STOCK_LIST_URL_BAK: &str =
    "https://quotes.sina.cn/cn/api/json_v2.php/CN_MarketDataService.getHQNodeData";

const QT_HEADERS: [(&str, &str); 2] = [
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    ),
    ("Referer", "https://gu.qq.com/"),
];
const LIST_HEADERS: [(&str, &str); 2] = [
    ("User-Agent", "Mozilla/5.0"),
    ("Referer", "https://finance.sina.com.cn/"),
];

pub const QUOTE_TTL: u64 = 15;
pub const KLINE_TTL: u64 = 60;
pub const LIST_TTL: u64 = 600;
pub const PAGE_SIZE: usize = 100;
pub const MAX_LIST_PAGES: usize = 60;
/// 腾讯批量接口单次最多接受的代码数。
const BATCH_SIZE: usize = 50;
/// 列表少于该数量时提示数据源可能不完整。
const MIN_LIST_SIZE: usize = 1000;

/// K 线缓存键：代码、周期、条数、截止日。截止日必须进键，否则回测不同
/// as_of 之间会互相污染。
type KlineKey = (String, String, usize, String);
type QuoteCache = HashMap<String, (Instant, Quote)>;
type KlineCache = HashMap<KlineKey, (Instant, Vec<Bar>)>;
type ListCache = Option<(Instant, Vec<StockInfo>)>;

pub struct MarketData {
    http: HttpClient,
    cal: Rc<Calendar>,
    quote_cache: QuoteCache,
    kline_cache: KlineCache,
    list_cache: ListCache,
}

impl MarketData {
    pub fn new(http: HttpClient, cal: Rc<Calendar>) -> Self {
        MarketData {
            http,
            cal,
            quote_cache: HashMap::new(),
            kline_cache: HashMap::new(),
            list_cache: None,
        }
    }

    /// 真实开市判断：交易时段 + 上证指数快照日期就是今天。
    /// 只看日历不够——节假日日历可能没更新，指数快照能兜住。
    pub fn market_is_open(&mut self, now: chrono::NaiveDateTime) -> bool {
        if !self.cal.is_trading_time(now) {
            return false;
        }
        let Some(quote) = self.get_index_quote() else {
            return false;
        };
        let stamp = quote.time;
        let bytes = stamp.as_bytes();
        if bytes.len() < 8 || !bytes[..8].iter().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let day = format!("{}-{}-{}", &stamp[0..4], &stamp[4..6], &stamp[6..8]);
        day == ymd(now.date())
    }

    /// 上证指数快照（代码 000001，交易所前缀 sh）。
    pub fn get_index_quote(&mut self) -> Option<Quote> {
        let key = "sh000001".to_string();
        if let Some((at, quote)) = self.quote_cache.get(&key) {
            if at.elapsed().as_secs() < QUOTE_TTL {
                return Some(quote.clone());
            }
        }
        let url = format!("{QT_URL}{key}");
        let bytes = self
            .http
            .get_bytes(&url, &to_pairs(&QT_HEADERS), false)
            .ok()?;
        let text = decode_gbk(&bytes);
        let payload = extract_payload(&text)?;
        let quote = parse_quote("000001", payload)?;
        self.quote_cache
            .insert(key, (Instant::now(), quote.clone()));
        Some(quote)
    }

    /// 单只股票实时行情。
    pub fn quote(&mut self, symbol: &str) -> Option<Quote> {
        let code = to_tencent_code(symbol)?;
        if let Some((at, quote)) = self.quote_cache.get(&code) {
            if at.elapsed().as_secs() < QUOTE_TTL {
                return Some(quote.clone());
            }
        }
        let url = format!("{QT_URL}{code}");
        let bytes = self
            .http
            .get_bytes(&url, &to_pairs(&QT_HEADERS), false)
            .ok()?;
        let text = decode_gbk(&bytes);
        let payload = extract_payload(&text)?;
        let quote = parse_quote(symbol, payload)?;
        self.quote_cache
            .insert(code, (Instant::now(), quote.clone()));
        Some(quote)
    }

    /// 批量行情：先查缓存，只并发请求缺失或过期的代码。
    /// 返回的 map 以 6 位代码为键（不带 sh/sz/bj 前缀），调用方无需再去前缀。
    pub fn quotes(&mut self, symbols: &[String]) -> HashMap<String, Quote> {
        let mut result: HashMap<String, Quote> = HashMap::new();
        if symbols.is_empty() {
            return result;
        }

        let mut missing: Vec<String> = Vec::new();
        let mut queued: HashSet<String> = HashSet::new();
        for symbol in symbols {
            let Some(code) = to_tencent_code(symbol) else {
                continue;
            };
            if let Some((at, quote)) = self.quote_cache.get(&code) {
                if at.elapsed().as_secs() < QUOTE_TTL {
                    result.insert(code[2..].to_string(), quote.clone());
                    continue;
                }
            }
            if queued.insert(code.clone()) {
                missing.push(code);
            }
        }
        if missing.is_empty() {
            return result;
        }
        missing.sort();

        let reqs: Vec<GetRequest> = missing
            .chunks(BATCH_SIZE)
            .map(|batch| {
                let codes = batch.join(",");
                GetRequest::new(format!("{QT_URL}{codes}")).with_headers(&QT_HEADERS)
            })
            .collect();

        let bodies = self.http.get_bytes_many(reqs);
        // 缓存只由主线程写，线程里不碰共享状态。
        let stamp = Instant::now();
        for body in bodies.into_iter().flatten() {
            let text = decode_gbk(&body);
            for line in text.trim().split('\n') {
                let Some((key_part, payload)) = line.split_once("=\"") else {
                    continue;
                };
                let key = key_part
                    .split('=')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .rsplit('_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let code =
                    if key.starts_with("sh") || key.starts_with("sz") || key.starts_with("bj") {
                        key[2..].to_string()
                    } else {
                        key
                    };
                let payload = payload.trim_end_matches(['"', ';']);
                if let Some(quote) = parse_quote(&code, payload) {
                    result.insert(code.clone(), quote.clone());
                    // 缓存键统一带交易所前缀，与单只请求的键保持一致。
                    let key = format!("{}{}", exchange_prefix(&code), code);
                    self.quote_cache.insert(key, (stamp, quote));
                }
            }
        }
        result
    }

    /// 前复权 K 线。`end_date` 指定时只取该日及之前的数据（回测 as_of）。
    pub fn kline(
        &mut self,
        symbol: &str,
        period: &str,
        limit: usize,
        end_date: Option<&str>,
    ) -> Vec<Bar> {
        let Some(code) = to_tencent_code(symbol) else {
            return Vec::new();
        };
        let period_key = if period == "1d" || period == "day" {
            "day"
        } else {
            "week"
        };
        let limit = limit.clamp(30, 640);
        let end = take_chars(end_date.unwrap_or(""), 10);
        let key = (code.clone(), period_key.to_string(), limit, end.clone());
        if let Some((at, rows)) = self.kline_cache.get(&key) {
            if at.elapsed().as_secs() < KLINE_TTL {
                return rows.clone();
            }
        }

        let mut rows: Vec<Bar> = Vec::new();
        let url = build_url(
            KLINE_URL,
            &[("param", format!("{code},{period_key},,{end},{limit},qfq"))],
        );
        if let Ok(data) = self.http.get_json(&url, &to_pairs(&QT_HEADERS), false) {
            let stock = data.get("data").and_then(|node| node.get(&code));
            let lines = stock
                .and_then(|node| node.get(format!("qfq{period_key}")))
                .or_else(|| stock.and_then(|node| node.get(period_key)))
                .and_then(|node| node.as_array());
            if let Some(lines) = lines {
                for line in lines {
                    if let Some(bar) = bar_from_json(line) {
                        rows.push(bar);
                    }
                }
            }
            if !end.is_empty() {
                rows.retain(|bar| bar.date <= end);
            }
        }

        // 北交所/部分标的腾讯日 K 不足，用新浪日线兜底。
        if rows.len() < 30 && period_key == "day" {
            let sina_limit = limit + if end.is_empty() { 0 } else { 120 };
            rows = self.sina_kline(&code, sina_limit, &end);
        }

        self.kline_cache.insert(key, (Instant::now(), rows.clone()));
        rows
    }

    fn sina_kline(&self, code: &str, limit: usize, end: &str) -> Vec<Bar> {
        let datalen = limit.clamp(30, 1023).to_string();
        let url = build_url(
            SINA_KLINE_URL,
            &[
                ("symbol", code.to_string()),
                ("scale", "240".to_string()),
                ("ma", "no".to_string()),
                ("datalen", datalen.clone()),
            ],
        );
        let Ok(payload) = self.http.get_json(&url, &to_pairs(&LIST_HEADERS), false) else {
            return Vec::new();
        };
        let Some(items) = payload.as_array() else {
            return Vec::new();
        };
        let mut rows: Vec<Bar> = Vec::new();
        for item in items {
            let date = take_chars(&json_text(item.get("day")), 10);
            let (Some(open), Some(close), Some(high), Some(low), Some(volume)) = (
                json_number(item.get("open")),
                json_number(item.get("close")),
                json_number(item.get("high")),
                json_number(item.get("low")),
                json_number(item.get("volume")),
            ) else {
                continue;
            };
            rows.push(Bar {
                date,
                open,
                close,
                high,
                low,
                // 新浪成交量单位是"股"，统一折算成"手"。
                volume: volume / 100.0,
            });
        }
        if end.is_empty() {
            return rows;
        }
        rows.retain(|bar| bar.date.as_str() <= end);
        // 指定 end 时只保留最近 `limit` 根，避免拼进多余的历史。
        let tail = datalen.parse::<usize>().unwrap_or(30).max(30);
        if rows.len() > tail {
            rows.split_off(rows.len() - tail)
        } else {
            rows
        }
    }

    /// 全部沪深京 A 股，翻页到空页为止并去重。
    pub fn all_stocks(&mut self, force: bool) -> Vec<StockInfo> {
        if !force {
            if let Some((at, data)) = &self.list_cache {
                if !data.is_empty() && at.elapsed().as_secs() < LIST_TTL {
                    return data.clone();
                }
            }
        }

        let mut stocks: Vec<StockInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        let reqs: Vec<GetRequest> = (1..=MAX_LIST_PAGES)
            .map(|page| GetRequest::new(list_url(STOCK_LIST_URL, page)).with_headers(&LIST_HEADERS))
            .collect();
        for text in self.http.get_text_many(reqs).into_iter().flatten() {
            let items = parse_sina_list(&text);
            if items.is_empty() {
                continue;
            }
            let short_page = items.len() < PAGE_SIZE;
            append_items(&mut stocks, &mut seen, items);
            if short_page {
                break;
            }
        }

        if stocks.is_empty() {
            // 主域名整体失败时用备用域名再试前 20 页（串行，避免打爆备用站）。
            for page in 1..=20 {
                let url = list_url(STOCK_LIST_URL_BAK, page);
                let Ok(text) = self.http.get_text(&url, &to_pairs(&LIST_HEADERS), false) else {
                    break;
                };
                let items = parse_sina_list(&text);
                if items.is_empty() {
                    break;
                }
                let short_page = items.len() < PAGE_SIZE;
                append_items(&mut stocks, &mut seen, items);
                if short_page {
                    break;
                }
            }
        }

        if stocks.len() < MIN_LIST_SIZE {
            log(&format!(
                "[行情] 警告：股票列表仅 {} 只，数据源可能不完整",
                stocks.len()
            ));
        }
        self.list_cache = Some((Instant::now(), stocks.clone()));
        stocks
    }
}

impl MarketSource for MarketData {
    fn market_is_open(&mut self, now: chrono::NaiveDateTime) -> bool {
        MarketData::market_is_open(self, now)
    }

    fn get_all_a_stocks(&mut self, force: bool) -> Vec<StockInfo> {
        self.all_stocks(force)
    }

    fn get_realtime(&mut self, symbol: &str) -> Option<Quote> {
        self.quote(symbol)
    }

    fn get_realtime_batch(&mut self, symbols: &[String]) -> HashMap<String, Quote> {
        self.quotes(symbols)
    }

    fn get_kline(
        &mut self,
        symbol: &str,
        period: &str,
        limit: usize,
        end_date: Option<&str>,
    ) -> Vec<Bar> {
        self.kline(symbol, period, limit, end_date)
    }
}

fn to_pairs(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn list_url(base: &str, page: usize) -> String {
    build_url(
        base,
        &[
            ("page", page.to_string()),
            ("num", PAGE_SIZE.to_string()),
            ("sort", "changepercent".to_string()),
            ("asc", "0".to_string()),
            ("node", "hs_a".to_string()),
        ],
    )
}

fn exchange_prefix(code: &str) -> &'static str {
    if code.starts_with('6') || code.starts_with('9') {
        "sh"
    } else if code.starts_with('0') || code.starts_with('3') {
        "sz"
    } else {
        "bj"
    }
}

/// 腾讯行情是 GBK 编码。非法字节按 U+FFFD 替换，绝不让解码失败中断整轮扫描。
pub fn decode_gbk(bytes: &[u8]) -> String {
    let (text, _, _) = encoding_rs::GBK.decode(bytes);
    text.into_owned()
}

/// 从 `v_sh600519="..."` 这类响应里取出引号内的 payload。
fn extract_payload(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("=\"")?;
    Some(rest.trim_end_matches(['"', ';', '\n', '\r']))
}

/// `600519 -> sh600519`、`000001 -> sz000001`、`920285 -> bj920285`。
pub fn to_tencent_code(symbol: &str) -> Option<String> {
    let mut text = symbol.trim().to_uppercase();
    for suffix in [".SH", ".SZ", ".BJ"] {
        text = text.replace(suffix, "");
    }
    for prefix in ["SH", "SZ", "BJ"] {
        text = text.replace(prefix, "");
    }
    let text = text.trim().to_string();
    if text.len() != 6 || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if text.starts_with("920") || text.starts_with('4') || text.starts_with('8') {
        return Some(format!("bj{text}"));
    }
    if text.starts_with('6') || text.starts_with('9') {
        return Some(format!("sh{text}"));
    }
    if text.starts_with('0') || text.starts_with('3') {
        return Some(format!("sz{text}"));
    }
    None
}

/// 解析腾讯行情 payload（已解码成字符串，字段以 `~` 分隔）。
/// 字段不足 40 个或价格非正视为无效数据。
pub fn parse_quote(symbol: &str, payload: &str) -> Option<Quote> {
    let fields: Vec<&str> = payload.split('~').collect();
    if fields.len() < 40 {
        return None;
    }
    let num = |index: usize| -> f64 {
        fields
            .get(index)
            .map(|value| value.trim())
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(0.0)
    };
    let price = num(3);
    if price <= 0.0 {
        return None;
    }
    Some(Quote {
        code: symbol.to_string(),
        name: fields
            .get(1)
            .map(|v| v.trim().to_string())
            .unwrap_or_default(),
        price,
        prev_close: num(4),
        open: num(5),
        volume: num(6),
        high: num(33),
        low: num(34),
        change_pct: num(32),
        time: fields.get(30).map(|v| (*v).to_string()).unwrap_or_default(),
        turnover: num(38),
        limit_up: num(47),
        limit_down: num(48),
        volume_ratio: num(49),
    })
}

/// 解析新浪全市场列表（UTF-8 JSON 数组）。空串/`null`/`[]` 都返回空 Vec。
pub fn parse_sina_list(text: &str) -> Vec<StockInfo> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "[]" {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut stocks: Vec<StockInfo> = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let code = json_text(object.get("code"));
        let name = json_text(object.get("name"));
        if code.trim().is_empty() || name.trim().is_empty() {
            continue;
        }
        stocks.push(StockInfo {
            code,
            name,
            price: json_number(object.get("trade")).unwrap_or(0.0),
            change_pct: json_number(object.get("changepercent")).unwrap_or(0.0),
        });
    }
    stocks
}

fn append_items(stocks: &mut Vec<StockInfo>, seen: &mut HashSet<String>, items: Vec<StockInfo>) {
    for item in items {
        if item.code.is_empty() || item.name.is_empty() {
            continue;
        }
        if !seen.insert(item.code.clone()) {
            continue;
        }
        stocks.push(item);
    }
}

fn bar_from_json(line: &serde_json::Value) -> Option<Bar> {
    let cells = line.as_array()?;
    if cells.len() < 6 {
        return None;
    }
    let date = take_chars(&json_text(Some(&cells[0])), 10);
    let open = json_number(Some(&cells[1]))?;
    let close = json_number(Some(&cells[2]))?;
    let high = json_number(Some(&cells[3]))?;
    let low = json_number(Some(&cells[4]))?;
    let volume = json_number(Some(&cells[5]))?;
    Some(Bar {
        date,
        open,
        close,
        high,
        low,
        volume,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_tencent_code_prefixes() {
        assert_eq!(to_tencent_code("600519").as_deref(), Some("sh600519"));
        assert_eq!(to_tencent_code("000001").as_deref(), Some("sz000001"));
        assert_eq!(to_tencent_code("920285").as_deref(), Some("bj920285"));
        // 带交易所后缀/前缀的写法也要归一
        assert_eq!(to_tencent_code("600519.SH").as_deref(), Some("sh600519"));
        assert_eq!(to_tencent_code("SH600519").as_deref(), Some("sh600519"));
        assert_eq!(to_tencent_code("300750").as_deref(), Some("sz300750"));
        assert_eq!(to_tencent_code("830799").as_deref(), Some("bj830799"));
        assert_eq!(to_tencent_code("abc"), None);
        assert_eq!(to_tencent_code("60051"), None);
    }

    fn sample_payload() -> String {
        let mut fields: Vec<String> = (0..50).map(|_| "0".to_string()).collect();
        fields[1] = "贵州茅台".to_string();
        fields[3] = "1295.00".to_string();
        fields[4] = "1280.00".to_string();
        fields[5] = "1285.00".to_string();
        fields[6] = "12345".to_string();
        fields[30] = "20261021150000".to_string();
        fields[32] = "1.17".to_string();
        fields[33] = "1300.00".to_string();
        fields[34] = "1275.00".to_string();
        fields[38] = "9.99".to_string();
        fields[47] = "1408.00".to_string();
        fields[48] = "1152.00".to_string();
        fields[49] = "0.86".to_string();
        fields.join("~")
    }

    #[test]
    fn parse_quote_maps_documented_indices() {
        let quote = parse_quote("600519", &sample_payload()).expect("有效 payload");
        assert_eq!(quote.code, "600519");
        assert_eq!(quote.name, "贵州茅台");
        assert_eq!(quote.price, 1295.0);
        assert_eq!(quote.prev_close, 1280.0);
        assert_eq!(quote.open, 1285.0);
        assert_eq!(quote.volume, 12345.0);
        assert_eq!(quote.time, "20261021150000");
        assert_eq!(quote.change_pct, 1.17);
        assert_eq!(quote.high, 1300.0);
        assert_eq!(quote.low, 1275.0);
        assert_eq!(quote.turnover, 9.99);
        assert_eq!(quote.limit_up, 1408.0);
        assert_eq!(quote.limit_down, 1152.0);
        assert_eq!(quote.volume_ratio, 0.86);
    }

    #[test]
    fn parse_quote_rejects_short_or_zero_price() {
        assert!(parse_quote("600519", "1~2~3").is_none());
        let mut fields: Vec<String> = (0..50).map(|_| "0".to_string()).collect();
        fields[3] = "0".to_string();
        assert!(parse_quote("600519", &fields.join("~")).is_none());
    }

    #[test]
    fn parse_sina_list_skips_invalid_rows() {
        let text = r#"[
            {"code":"600519","name":"贵州茅台","trade":"1295.000","changepercent":"1.17"},
            {"code":"","name":"空代码","trade":"1","changepercent":"0"},
            {"code":"000001","name":"","trade":"1","changepercent":"0"},
            {"code":"000002","name":"万科A"}
        ]"#;
        let stocks = parse_sina_list(text);
        assert_eq!(stocks.len(), 2);
        assert_eq!(stocks[0].code, "600519");
        assert_eq!(stocks[0].price, 1295.0);
        assert_eq!(stocks[0].change_pct, 1.17);
        assert_eq!(stocks[1].code, "000002");
        assert_eq!(stocks[1].price, 0.0);
    }

    #[test]
    fn parse_sina_list_handles_empty_and_broken_input() {
        assert!(parse_sina_list("").is_empty());
        assert!(parse_sina_list("null").is_empty());
        assert!(parse_sina_list("[]").is_empty());
        assert!(parse_sina_list("<html>").is_empty());
        assert!(parse_sina_list(r#"{"code":1}"#).is_empty());
    }

    #[test]
    fn extract_payload_trims_quotes_and_semicolon() {
        let text = "v_sh600519=\"1~贵州茅台~600519\";\n";
        assert_eq!(extract_payload(text), Some("1~贵州茅台~600519"));
        assert_eq!(extract_payload("no quote here"), None);
    }

    /// "浦发银行" 的 GBK 字节，硬编码以便验证解码本身（而不是编码后再解码）。
    const PUFA_GBK: [u8; 8] = [0xC6, 0xD6, 0xB7, 0xA2, 0xD2, 0xF8, 0xD0, 0xD0];
    /// "*ST康德" 的 GBK 字节：星号与英文是大写 ASCII，中文按 GBK 双字节。
    const ST_KANGDE_GBK: [u8; 7] = [0x2A, 0x53, 0x54, 0xBF, 0xB5, 0xB5, 0xC2];

    #[test]
    fn decode_gbk_restores_chinese_name() {
        assert_eq!(decode_gbk(&PUFA_GBK), "浦发银行");
        // 截断的 GBK 双字节按 lossy 替换处理，不能 panic、不能整段丢失。
        let broken = [0xC6_u8, 0xD6, 0xFF];
        assert!(!decode_gbk(&broken).is_empty());
        assert!(decode_gbk(&[]).is_empty());
    }

    /// 名称必须穿过 GBK 解码进入涨跌停判定：*ST 主板按 5%，而不是 10%。
    #[test]
    fn gbk_name_drives_st_price_limit() {
        let mut fields: Vec<Vec<u8>> = (0..50).map(|_| b"0".to_vec()).collect();
        fields[1] = ST_KANGDE_GBK.to_vec();
        fields[3] = b"12.34".to_vec();
        let payload = fields.join(&b'~');
        let text = decode_gbk(&payload);

        let quote = parse_quote("600123", &text).expect("有效 payload");
        assert_eq!(quote.name, "*ST康德");
        assert!(crate::rules::is_st_name(&quote.name));
        assert_eq!(crate::rules::price_limit_ratio("600123", &quote.name), 0.05);
        // 去掉 ST 前缀后回到 10%，确认差异确实来自名称。
        assert_eq!(crate::rules::price_limit_ratio("600123", "康德"), 0.10);
    }

    /// 真实接口联调（默认跳过，手动执行：`cargo test live_ -- --ignored --nocapture`）。
    #[test]
    #[ignore = "需要访问腾讯/新浪真实接口"]
    fn live_sources_return_data() {
        let http = HttpClient::new(
            std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(20),
        )
        .expect("构建客户端");
        let mut md = MarketData::new(http, std::rc::Rc::new(crate::calendar::Calendar::empty()));

        let quote = md.quote("600519").expect("腾讯实时行情应可用");
        assert!(quote.price > 0.0);
        assert!(!quote.name.is_empty(), "GBK 解码后应拿到中文名");
        crate::util::log(&format!(
            "[联调] sh600519 {} 价格 {:.2} 涨幅 {:.2}% 时间 {}",
            quote.name, quote.price, quote.change_pct, quote.time
        ));

        let batch = md.quotes(&["600519".to_string(), "000001".to_string()]);
        assert_eq!(batch.len(), 2, "批量行情应返回两只");
        assert!(batch.contains_key("600519"));

        let bars = md.kline("600519", "day", 60, None);
        assert!(bars.len() >= 30, "日 K 至少 30 根，实际 {}", bars.len());
        assert!(bars.iter().all(|bar| bar.close > 0.0));
        crate::util::log(&format!(
            "[联调] 日K {} 根，最新 {} 收盘 {:.2}",
            bars.len(),
            bars.last().map(|b| b.date.as_str()).unwrap_or(""),
            bars.last().map(|b| b.close).unwrap_or(0.0)
        ));

        let cut = bars[bars.len() / 2].date.clone();
        let truncated = md.kline("600519", "day", 240, Some(&cut));
        assert!(
            truncated.iter().all(|bar| bar.date <= cut),
            "as_of 之后的数据必须被截断"
        );

        let stocks = md.all_stocks(true);
        assert!(!stocks.is_empty(), "新浪全市场列表应非空");
        crate::util::log(&format!(
            "[联调] 全市场 {} 只，首行 {} {}",
            stocks.len(),
            stocks[0].code,
            stocks[0].name
        ));
    }
}
