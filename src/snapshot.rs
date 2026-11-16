//! 本地行情快照：把一次抓取的全市场列表与日 K 落盘，回测只读磁盘、不联网。
//!
//! 快照只能锁住「抓取时刻能看到的股票池」；历史区间里后来退市的股票无法补回，
//! 所以幸存者偏差只能如实标注，不能靠本地文件消除。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::store::Paths;
use crate::types::{Bar, MarketSource, Quote, StockInfo};
use crate::util;

/// 写在 meta 里，提醒使用者不要把这批数据当成历史真实成分股。
const SURVIVORSHIP_NOTE: &str = "股票池为抓取时刻的当前全市场列表，历史区间存在幸存者偏差";
/// 抓取 K 线时额外多取的根数，与回测预热所需样本对齐。
const WARMUP_BARS: usize = 120;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub id: String,
    pub created_at: String,
    /// 当次抓到的全市场列表，只含 code/name/price/change_pct 四项。
    pub symbols: Vec<StockInfo>,
    /// 实际抓过日 K 的标的，也是离线源能回答的全部标的。
    pub kline_symbols: Vec<String>,
    pub benchmark: String,
    pub benchmark_source: String,
    pub note: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LatestPointer {
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct SnapshotEntry {
    pub id: String,
    pub created_at: String,
    pub kline_count: usize,
}

/// 只读磁盘的行情源。结构体里没有任何 HTTP 客户端，因此结构上不可能联网补数据。
pub struct SnapshotSource {
    meta: SnapshotMeta,
    kline: HashMap<String, Vec<Bar>>,
}

impl SnapshotSource {
    pub fn open(paths: &Paths, id: &str) -> AppResult<SnapshotSource> {
        let resolved = resolve_id(paths, id)?;
        let dir = paths.snapshot_dir(&resolved);
        let meta: SnapshotMeta = read_json(&dir.join("meta.json"))?;
        let mut kline: HashMap<String, Vec<Bar>> = HashMap::new();
        for code in &meta.kline_symbols {
            let file = dir.join("kline").join(file_name(code));
            if file.exists() {
                let bars: Vec<Bar> = read_json(&file)?;
                kline.insert(key_of(code), bars);
            }
        }
        let benchmark_file = dir.join("benchmark.json");
        if benchmark_file.exists() {
            let bars: Vec<Bar> = read_json(&benchmark_file)?;
            kline.insert(key_of(&meta.benchmark), bars);
        }
        Ok(SnapshotSource { meta, kline })
    }
}

impl MarketSource for SnapshotSource {
    fn market_is_open(&mut self, _now: chrono::NaiveDateTime) -> bool {
        false
    }

    fn get_all_a_stocks(&mut self, _force: bool) -> Vec<StockInfo> {
        self.meta.symbols.clone()
    }

    fn get_realtime(&mut self, _symbol: &str) -> Option<Quote> {
        None
    }

    fn get_realtime_batch(&mut self, _symbols: &[String]) -> HashMap<String, Quote> {
        HashMap::new()
    }

    fn get_kline(
        &mut self,
        symbol: &str,
        _period: &str,
        limit: usize,
        end_date: Option<&str>,
    ) -> Vec<Bar> {
        let Some(bars) = self.kline.get(&key_of(symbol)) else {
            util::log(&format!("[警告] 快照缺少 {symbol} 的日K"));
            return Vec::new();
        };
        // 先按 end_date 截断再取末尾 limit 根，先取后截会丢掉早期的 as_of 数据。
        let cut: String = end_date
            .map(|text| text.chars().take(10).collect())
            .unwrap_or_default();
        let mut rows: Vec<Bar> = if cut.is_empty() {
            bars.clone()
        } else {
            bars.iter()
                .filter(|bar| bar.date.as_str() <= cut.as_str())
                .cloned()
                .collect()
        };
        let start = rows.len().saturating_sub(limit);
        rows.drain(0..start);
        rows
    }
}

/// 让 `Box<dyn MarketSource>` 本身也能作为 Sized 的行情源传给回测，
/// 省去为泛型加 `?Sized` 而牵动指标层的 trait-object 转换。
impl MarketSource for Box<dyn MarketSource> {
    fn market_is_open(&mut self, now: chrono::NaiveDateTime) -> bool {
        self.as_mut().market_is_open(now)
    }

    fn get_all_a_stocks(&mut self, force: bool) -> Vec<StockInfo> {
        self.as_mut().get_all_a_stocks(force)
    }

    fn get_realtime(&mut self, symbol: &str) -> Option<Quote> {
        self.as_mut().get_realtime(symbol)
    }

    fn get_realtime_batch(&mut self, symbols: &[String]) -> HashMap<String, Quote> {
        self.as_mut().get_realtime_batch(symbols)
    }

    fn get_kline(
        &mut self,
        symbol: &str,
        period: &str,
        limit: usize,
        end_date: Option<&str>,
    ) -> Vec<Bar> {
        self.as_mut().get_kline(symbol, period, limit, end_date)
    }
}

/// 抓取一次并落盘。只抓指定标的与基准指数的日 K，全市场列表只存四项字段。
pub fn create(
    paths: &Paths,
    md: &mut dyn MarketSource,
    symbols: &[String],
    benchmark: &str,
    days: usize,
) -> AppResult<(SnapshotMeta, usize)> {
    let id = fresh_id(paths)?;
    let stocks = md.get_all_a_stocks(true);
    let limit = days.max(1) + WARMUP_BARS;
    let mut kline: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    let mut kline_symbols: Vec<String> = Vec::new();
    for symbol in symbols {
        let rows = md.get_kline(symbol, "1d", limit, None);
        if rows.is_empty() {
            util::log(&format!("[警告] {symbol} 未取到日K，快照里将缺少该标的"));
        }
        kline.insert(symbol.clone(), rows);
        kline_symbols.push(symbol.clone());
    }
    let benchmark_code = crate::backtest::normalize_benchmark(benchmark);
    let benchmark_bars = md.get_kline(&benchmark_code, "1d", limit, None);
    let benchmark_source = if benchmark_bars.is_empty() {
        "unavailable".to_string()
    } else {
        format!("index:{benchmark_code}")
    };
    let meta = SnapshotMeta {
        id,
        created_at: util::now_iso(),
        symbols: stocks,
        kline_symbols,
        benchmark: benchmark_code,
        benchmark_source,
        note: SURVIVORSHIP_NOTE.to_string(),
    };
    let files = write_snapshot(paths, &meta, &kline, &benchmark_bars)?;
    Ok((meta, files))
}

/// 写入单个快照的全部文件，返回文件数（meta + 每标的一份 + 基准 + latest）。
pub fn write_snapshot(
    paths: &Paths,
    meta: &SnapshotMeta,
    kline: &BTreeMap<String, Vec<Bar>>,
    benchmark_bars: &[Bar],
) -> AppResult<usize> {
    let dir = paths.snapshot_dir(&meta.id);
    std::fs::create_dir_all(dir.join("kline"))?;
    write_json(&dir.join("meta.json"), meta)?;
    for (code, bars) in kline {
        write_json(&dir.join("kline").join(file_name(code)), bars)?;
    }
    write_json(&dir.join("benchmark.json"), benchmark_bars)?;
    write_json(
        &paths.snapshot_latest(),
        &LatestPointer {
            id: meta.id.clone(),
        },
    )?;
    Ok(3 + kline.len())
}

/// `--snapshot latest` 的解析入口；其它取值按原样当 id 用。
pub fn resolve_id(paths: &Paths, id: &str) -> AppResult<String> {
    if id != "latest" {
        return Ok(id.to_string());
    }
    let latest: LatestPointer = read_json(&paths.snapshot_latest())?;
    if latest.id.is_empty() {
        return Err(AppError::Invalid("latest.json 里没有快照 id".to_string()));
    }
    Ok(latest.id)
}

/// 只读快照的 meta；因子面板按 `kline_symbols` 决定默认标的。
pub fn read_meta(paths: &Paths, id: &str) -> AppResult<SnapshotMeta> {
    let resolved = resolve_id(paths, id)?;
    read_json(&paths.snapshot_dir(&resolved).join("meta.json"))
}

/// 列出已有快照，按 id 升序（时间字符串本身可排序）。
pub fn list(paths: &Paths) -> AppResult<Vec<SnapshotEntry>> {
    let dir = paths.snapshots_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<SnapshotEntry> = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        if !item.file_type()?.is_dir() {
            continue;
        }
        let meta_file = item.path().join("meta.json");
        if !meta_file.exists() {
            continue;
        }
        let Ok(meta) = read_json::<SnapshotMeta>(&meta_file) else {
            continue;
        };
        entries.push(SnapshotEntry {
            id: meta.id,
            created_at: meta.created_at,
            kline_count: meta.kline_symbols.len(),
        });
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(entries)
}

/// 同一秒内重复创建时加序号，避免覆盖上一份快照。
fn fresh_id(paths: &Paths) -> AppResult<String> {
    let base = util::now_local().format("%Y%m%d_%H%M%S").to_string();
    let mut id = base.clone();
    let mut suffix: u32 = 2;
    while paths.snapshot_dir(&id).exists() {
        id = format!("{base}_{suffix}");
        suffix += 1;
    }
    Ok(id)
}

/// 代码只允许字母数字进文件名，避免路径穿越。
fn key_of(code: &str) -> String {
    code.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// 单个标的的 K 线文件名；因子面板等只读方复用同一规则，避免各处命名漂移。
pub(crate) fn file_name(code: &str) -> String {
    format!("{}.json", key_of(code))
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> AppResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{text}\n"))?;
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> AppResult<T> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{Backtest, BacktestSettings};
    use crate::calendar::Calendar;
    use crate::signal::{AiSignal, AnalyzeRequest, GroundOptions};
    use crate::types::AiSource;
    use chrono::{Datelike, NaiveDate};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-snap-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        Paths { base_dir: base }
    }

    /// 造一段确定性日 K：收盘逐日 +0.1，跳过周末。
    fn make_bars(count: usize, start: f64) -> Vec<Bar> {
        let mut rows: Vec<Bar> = Vec::with_capacity(count);
        let mut day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("固定日期");
        let mut price = start;
        for index in 0..count {
            while day.weekday().num_days_from_monday() >= 5 {
                day = day.succ_opt().expect("日期递增");
            }
            let open = price + 0.1;
            let close = open + 0.1;
            rows.push(Bar {
                date: util::ymd(day),
                open,
                close,
                high: close + 0.2,
                low: open - 0.2,
                volume: 1000.0 + index as f64,
            });
            price = close;
            day = day.succ_opt().expect("日期递增");
        }
        rows
    }

    /// 只回答固定数据的抓取源，并用计数器证明离线读取不会再触发任何抓取。
    struct FakeFetcher {
        symbol_bars: Vec<Bar>,
        benchmark_bars: Vec<Bar>,
        stocks: Vec<StockInfo>,
        calls: Rc<Cell<usize>>,
    }

    impl FakeFetcher {
        fn new(calls: Rc<Cell<usize>>) -> FakeFetcher {
            let mut benchmark_bars = make_bars(90, 10.0);
            for bar in benchmark_bars.iter_mut() {
                bar.open = 3000.0;
                bar.close = 3000.0;
                bar.high = 3000.0;
                bar.low = 3000.0;
            }
            FakeFetcher {
                symbol_bars: make_bars(90, 10.0),
                benchmark_bars,
                stocks: vec![
                    StockInfo {
                        code: "600000".to_string(),
                        name: "浦发银行".to_string(),
                        price: 10.0,
                        change_pct: 1.0,
                    },
                    StockInfo {
                        code: "600519".to_string(),
                        name: "贵州茅台".to_string(),
                        price: 1450.0,
                        change_pct: -0.5,
                    },
                ],
                calls,
            }
        }
    }

    impl MarketSource for FakeFetcher {
        fn market_is_open(&mut self, _now: chrono::NaiveDateTime) -> bool {
            false
        }

        fn get_all_a_stocks(&mut self, _force: bool) -> Vec<StockInfo> {
            self.stocks.clone()
        }

        fn get_realtime(&mut self, _symbol: &str) -> Option<Quote> {
            None
        }

        fn get_realtime_batch(&mut self, _symbols: &[String]) -> HashMap<String, Quote> {
            HashMap::new()
        }

        fn get_kline(
            &mut self,
            symbol: &str,
            _period: &str,
            limit: usize,
            end_date: Option<&str>,
        ) -> Vec<Bar> {
            self.calls.set(self.calls.get() + 1);
            let rows = if symbol.starts_with("sh") || symbol.starts_with("sz") {
                self.benchmark_bars.clone()
            } else {
                self.symbol_bars.clone()
            };
            let cut: String = end_date
                .map(|text| text.chars().take(10).collect())
                .unwrap_or_default();
            let filtered: Vec<Bar> = if cut.is_empty() {
                rows
            } else {
                rows.into_iter()
                    .filter(|bar| bar.date.as_str() <= cut.as_str())
                    .collect()
            };
            let start = filtered.len().saturating_sub(limit);
            filtered.into_iter().skip(start).collect()
        }
    }

    struct FakeAi;

    impl AiSource for FakeAi {
        fn analyze(&self, req: &AnalyzeRequest<'_>) -> AiSignal {
            let raw = format!(
                r#"{{"signal":"buy","confidence":0.9,"position_pct":10,"reason":"test","symbol":"{}"}}"#,
                req.symbol
            );
            let opts = GroundOptions {
                verify_price: req.verify_price,
                expected_name: None,
                ..GroundOptions::default()
            };
            crate::signal::parse_signal(&raw, req.symbol, &opts)
        }
    }

    #[test]
    fn write_read_round_trip_keeps_exact_values() {
        let paths = temp_paths("roundtrip");
        let calls = Rc::new(Cell::new(0usize));
        let mut fetcher = FakeFetcher::new(Rc::clone(&calls));
        let (meta, files) =
            create(&paths, &mut fetcher, &["600000".to_string()], "000300", 20).expect("创建快照");
        assert_eq!(files, 4, "meta + 1 标的 + benchmark + latest");
        assert_eq!(meta.symbols.len(), 2);
        assert_eq!(meta.benchmark, "sh000300");
        assert_eq!(meta.note, SURVIVORSHIP_NOTE);
        assert!(meta.benchmark_source.contains("sh000300"));

        let mut source = SnapshotSource::open(&paths, &meta.id).expect("打开快照");
        let rows = source.get_kline("600000", "1d", 1000, None);
        assert_eq!(rows.len(), 90);
        assert_eq!(rows[89].date, make_bars(90, 10.0)[89].date);
        assert!((rows[89].close - make_bars(90, 10.0)[89].close).abs() < 1e-12);
        assert_eq!(source.get_all_a_stocks(false).len(), 2);
        assert_eq!(resolve_id(&paths, "latest").expect("解析 latest"), meta.id);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn end_date_truncates_before_taking_limit() {
        let paths = temp_paths("truncate");
        let calls = Rc::new(Cell::new(0usize));
        let mut fetcher = FakeFetcher::new(Rc::clone(&calls));
        let (meta, _) =
            create(&paths, &mut fetcher, &["600000".to_string()], "000300", 20).expect("创建快照");
        let mut source = SnapshotSource::open(&paths, &meta.id).expect("打开快照");
        let cut = make_bars(90, 10.0)[59].date.clone();
        let rows = source.get_kline("600000", "1d", 5, Some(&cut));
        assert_eq!(rows.len(), 5, "截断后还有 60 根，取末尾 5 根");
        assert_eq!(rows[4].date, cut);
        assert_eq!(rows[0].date, make_bars(90, 10.0)[55].date);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn missing_symbol_returns_empty_without_any_fetch() {
        let paths = temp_paths("missing");
        let calls = Rc::new(Cell::new(0usize));
        let mut fetcher = FakeFetcher::new(Rc::clone(&calls));
        let (meta, _) =
            create(&paths, &mut fetcher, &["600000".to_string()], "000300", 20).expect("创建快照");
        let after_create = calls.get();
        assert_eq!(after_create, 2, "一次标的 + 一次基准");

        let mut source = SnapshotSource::open(&paths, &meta.id).expect("打开快照");
        assert!(source.get_kline("999999", "1d", 100, None).is_empty());
        assert_eq!(
            calls.get(),
            after_create,
            "离线源不得再次触发抓取，也不持有任何 HTTP 客户端"
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn latest_pointer_and_list_order() {
        let paths = temp_paths("latest");
        let empty: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        for (id, created) in [
            ("20260101_000000", "2026-01-01T00:00:00"),
            ("20260102_000000", "2026-01-02T00:00:00"),
        ] {
            let meta = SnapshotMeta {
                id: id.to_string(),
                created_at: created.to_string(),
                symbols: Vec::new(),
                kline_symbols: vec!["600000".to_string()],
                benchmark: "sh000300".to_string(),
                benchmark_source: "index:sh000300".to_string(),
                note: SURVIVORSHIP_NOTE.to_string(),
            };
            write_snapshot(&paths, &meta, &empty, &[]).expect("写快照");
        }
        assert_eq!(
            resolve_id(&paths, "latest").expect("解析 latest"),
            "20260102_000000"
        );
        assert_eq!(
            resolve_id(&paths, "20260101_000000").unwrap(),
            "20260101_000000"
        );
        let entries = list(&paths).expect("列出快照");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "20260101_000000");
        assert_eq!(entries[0].created_at, "2026-01-01T00:00:00");
        assert_eq!(entries[0].kline_count, 1);
        assert_eq!(entries[1].id, "20260102_000000");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    /// 核心不变量：同一份快照跑两次，区间、基准与每笔成交完全一致。
    #[test]
    fn same_snapshot_produces_identical_backtests() {
        let paths = temp_paths("determinism");
        let calls = Rc::new(Cell::new(0usize));
        let mut fetcher = FakeFetcher::new(Rc::clone(&calls));
        let (meta, _) =
            create(&paths, &mut fetcher, &["600000".to_string()], "000300", 20).expect("创建快照");

        let run_once = || {
            let mut source = SnapshotSource::open(&paths, &meta.id).expect("打开快照");
            let ai = FakeAi;
            let mut settings =
                BacktestSettings::new(vec!["600000".to_string()], 20, 5, Some(50_000.0), 0.0, 0.05);
            settings.benchmark = "sh000300".to_string();
            settings.snapshot_id = Some(meta.id.clone());
            let mut bt = Backtest::new(
                &mut source,
                &ai,
                &paths,
                Rc::new(Calendar::empty()),
                settings,
            );
            bt.run().expect("回测可运行")
        };

        let first = run_once();
        let second = run_once();
        assert!(first.error.is_none() && second.error.is_none());
        assert_eq!(first.start, second.start);
        assert_eq!(first.end, second.end);
        assert_eq!(first.trading_days, second.trading_days);
        assert_eq!(first.benchmark_return_pct, second.benchmark_return_pct);
        assert_eq!(first.total_return_pct, second.total_return_pct);
        assert_eq!(first.trades.len(), second.trades.len());
        assert!(!first.trades.is_empty(), "必须有成交，否则这条断言没有意义");
        for (left, right) in first.trades.iter().zip(second.trades.iter()) {
            assert_eq!(left.date, right.date);
            assert_eq!(left.price, right.price);
            assert_eq!(left.shares, right.shares);
        }
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(paths.backtest_manifest()).expect("读 manifest"),
        )
        .expect("manifest 合法");
        assert_eq!(manifest["snapshot_id"], meta.id);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }
}
