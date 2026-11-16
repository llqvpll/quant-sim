//! 账户状态与持久化：state.json 的结构、原子写、备份与损坏恢复。
//!
//! 文件格式是外部契约：手改过的 state 与旧版本落盘文件都必须能直接读回。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, STATE_VERSION};
use crate::error::{AppError, AppResult};
use crate::util;

pub const MAX_SIGNAL_LOG: usize = 5_000;

/// 运行目录解析结果；所有数据文件都在 `base_dir/data` 下。
pub struct Paths {
    pub base_dir: PathBuf,
}

impl Paths {
    /// 顺序：`QUANT_SIM_HOME` → 带项目标记的可执行文件目录 → 当前工作目录。
    ///
    /// 刻意不用 `CARGO_MANIFEST_DIR`：构建机路径编进二进制后换机即失效。
    /// exe 目录必须带 `Cargo.toml` 或 `dashboard.html` 才算数，否则 `cargo run`
    /// 会把 data/ 建到 target/debug/ 下，一次 `cargo clean` 就清掉模拟账户。
    pub fn resolve() -> Paths {
        Paths::resolve_from(
            std::env::var_os("QUANT_SIM_HOME"),
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf())),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// `resolve` 的纯逻辑部分；把环境读取抽出去，便于用临时目录构造各种布局做单测。
    fn resolve_from(
        home: Option<std::ffi::OsString>,
        exe_dir: Option<PathBuf>,
        cwd: PathBuf,
    ) -> Paths {
        if let Some(home) = home {
            let candidate = PathBuf::from(home);
            if !candidate.as_os_str().is_empty() {
                return Paths {
                    base_dir: candidate,
                };
            }
        }
        if let Some(dir) = exe_dir {
            // 发布包把 exe 与 dashboard.html 放在同一目录；开发时 target/debug 两者都没有
            if dir.join("Cargo.toml").exists() || dir.join("dashboard.html").exists() {
                return Paths { base_dir: dir };
            }
        }
        Paths { base_dir: cwd }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.base_dir.join("data")
    }

    pub fn state(&self) -> PathBuf {
        self.data_dir().join("state.json")
    }

    pub fn experience(&self) -> PathBuf {
        self.data_dir().join("experience.json")
    }

    pub fn heartbeat(&self) -> PathBuf {
        self.data_dir().join("heartbeat.json")
    }

    pub fn lock(&self) -> PathBuf {
        self.data_dir().join("bot.lock")
    }

    pub fn holidays(&self) -> PathBuf {
        self.data_dir().join("holidays.json")
    }

    pub fn backtest_state(&self, slug: &str) -> PathBuf {
        self.data_dir().join(format!("backtest_{slug}.json"))
    }

    pub fn backtest_result(&self) -> PathBuf {
        self.data_dir().join("backtest_result.json")
    }

    pub fn backtest_manifest(&self) -> PathBuf {
        self.data_dir().join("backtest_manifest.json")
    }

    pub fn backtest_signals(&self) -> PathBuf {
        self.data_dir().join("backtest_signals.jsonl")
    }

    pub fn backtest_walk(&self) -> PathBuf {
        self.data_dir().join("backtest_walk.json")
    }

    /// 因子评估报告。
    pub fn factor_report(&self) -> PathBuf {
        self.data_dir().join("factor_report.json")
    }

    /// 规则因子面板（JSONL，一行一个标的日期样本）。
    pub fn factor_panel(&self) -> PathBuf {
        self.data_dir().join("factor_panel.jsonl")
    }

    pub fn snapshots_dir(&self) -> PathBuf {
        self.data_dir().join("snapshots")
    }

    pub fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(id)
    }

    pub fn snapshot_latest(&self) -> PathBuf {
        self.snapshots_dir().join("latest.json")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub name: String,
    pub shares: i64,
    pub avg_cost: f64,
    pub last_price: f64,
    pub buy_date: Option<String>,
    pub available_date: Option<String>,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    pub open_time: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Order {
    pub id: u64,
    pub time: String,
    pub action: String,
    pub code: String,
    pub name: String,
    pub price: f64,
    pub shares: i64,
    pub amount: f64,
    pub commission: f64,
    pub transfer_fee: f64,
    pub stamp_tax: f64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetValue {
    pub date: String,
    pub total_assets: f64,
    pub cash: f64,
    pub market_value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignalLogEntry {
    pub time: String,
    pub code: String,
    pub name: String,
    pub signal: String,
    pub confidence: f64,
    pub reason: String,
    pub executed: bool,
    pub skip_reason: String,
    pub note: String,
    pub price: Option<f64>,
    pub hallucination_risk: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AiUsage {
    pub calls: u64,
    pub errors: u64,
    pub rule_skipped: u64,
    pub cost_cny: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub created_at: String,
    pub initial_capital: f64,
    pub cash: f64,
    pub positions: BTreeMap<String, Position>,
    pub orders: Vec<Order>,
    pub daily_net_values: Vec<NetValue>,
    pub signal_log: Vec<SignalLogEntry>,
    pub peak_assets: f64,
    pub halted: bool,
    pub halt_reason: String,
    pub halted_at: Option<String>,
    pub ai_usage: AiUsage,
    pub last_scan: Value,
    pub next_order_id: u64,
    #[serde(default = "Config::default", deserialize_with = "Config::de_loose")]
    pub config: Config,
}

/// 新建账户；资金非正时返回错误，而不是构造一个不可交易的账户。
pub fn new_state(capital: Option<f64>) -> AppResult<State> {
    let init_cap = capital.unwrap_or_else(|| Config::default().initial_capital);
    if !init_cap.is_finite() || init_cap <= 0.0 {
        return Err(AppError::Invalid("初始资金必须为正数".to_string()));
    }
    let config = Config {
        initial_capital: init_cap,
        ..Config::default()
    };
    Ok(State {
        version: STATE_VERSION,
        created_at: util::now_iso(),
        initial_capital: init_cap,
        cash: init_cap,
        positions: BTreeMap::new(),
        orders: Vec::new(),
        daily_net_values: Vec::new(),
        signal_log: Vec::new(),
        peak_assets: init_cap,
        halted: false,
        halt_reason: String::new(),
        halted_at: None,
        ai_usage: AiUsage::default(),
        last_scan: Value::Object(serde_json::Map::new()),
        next_order_id: 1,
        config,
    })
}

/// 读取账户：主文件 → `.bak` → 归档损坏文件并新建。
///
/// 返回 `State` 而非 `Result`：调用方（Bot/看板）在任何情况下都要拿到一个可用账户。
pub fn load(paths: &Paths, capital: Option<f64>) -> State {
    let path = paths.state();
    if !path.exists() {
        return new_state(capital).unwrap_or_else(|err| {
            util::log(&format!("[警告] 初始资金非法({err})，回落到默认资金"));
            new_state(None).unwrap_or_else(|_| unreachable_state())
        });
    }

    match read_state(&path) {
        Ok(state) => state,
        Err(err) => {
            let bak = with_suffix(&path, ".bak");
            if bak.exists() {
                if let Ok(state) = read_state(&bak) {
                    util::log(&format!(
                        "[状态] 主状态损坏({err})，已从 {} 恢复",
                        file_label(&bak)
                    ));
                    return state;
                }
            }
            let backup = corrupt_path(&path);
            let moved = std::fs::rename(&path, &backup);
            if moved.is_ok() {
                util::log(&format!(
                    "[状态] 读取 {} 失败({err})，已另存为 {}，将新建账户",
                    path.display(),
                    file_label(&backup)
                ));
            } else {
                util::log(&format!(
                    "[状态] 读取 {} 失败({err})，另存损坏文件也失败，将新建账户",
                    path.display()
                ));
            }
            new_state(capital)
                .unwrap_or_else(|_| new_state(None).unwrap_or_else(|_| unreachable_state()))
        }
    }
}

/// 在 Value 层补齐缺省键，再由 serde 转强类型。
///
/// 这样旧版本 state（只写了部分字段）也能读，不需要为每个字段加 serde default。
pub fn migrate(raw: &mut Value) {
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    setdefault(obj, "version", Value::from(1));
    let init_cap = obj
        .get("initial_capital")
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| Config::default().initial_capital);
    setdefault(obj, "initial_capital", Value::from(init_cap));
    setdefault(obj, "created_at", Value::from(util::now_iso()));
    setdefault(obj, "cash", Value::from(init_cap));
    setdefault(obj, "peak_assets", Value::from(init_cap));
    setdefault(obj, "halted", Value::from(false));
    setdefault(obj, "halt_reason", Value::from(""));
    setdefault(obj, "halted_at", Value::Null);
    setdefault(
        obj,
        "ai_usage",
        serde_json::json!({"calls": 0, "errors": 0, "rule_skipped": 0, "cost_cny": 0.0}),
    );
    setdefault(obj, "last_scan", Value::Object(serde_json::Map::new()));
    setdefault(obj, "orders", Value::Array(Vec::new()));
    setdefault(obj, "signal_log", Value::Array(Vec::new()));
    setdefault(obj, "daily_net_values", Value::Array(Vec::new()));
    setdefault(obj, "positions", Value::Object(serde_json::Map::new()));

    let order_count = obj
        .get("orders")
        .and_then(|value| value.as_array())
        .map(|items| items.len())
        .unwrap_or(0);
    setdefault(obj, "next_order_id", Value::from(order_count as u64 + 1));

    let merged = Config::merged(obj.get("config"));
    if let Ok(value) = serde_json::to_value(&merged) {
        obj.insert("config".to_string(), value);
    }

    if let Some(Value::Object(positions)) = obj.get_mut("positions") {
        let codes: Vec<String> = positions.keys().cloned().collect();
        for code in codes {
            let Some(Value::Object(pos)) = positions.get_mut(&code) else {
                continue;
            };
            setdefault(pos, "name", Value::from(code.clone()));
            setdefault(pos, "shares", Value::from(0));
            let avg_cost = pos
                .get("avg_cost")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            setdefault(pos, "avg_cost", Value::from(avg_cost));
            setdefault(pos, "last_price", Value::from(avg_cost));
            setdefault(pos, "buy_date", Value::Null);
            // 可卖日需要交易日历才能算准，这里只补键，真正取值由 Simulator 补
            setdefault(pos, "available_date", Value::Null);
            setdefault(pos, "stop_loss", Value::Null);
            setdefault(pos, "take_profit", Value::Null);
            setdefault(pos, "open_time", Value::from(util::now_iso()));
        }
    }

    obj.insert("version".to_string(), Value::from(STATE_VERSION));
}

/// 与 `save` 相同，但允许指定路径（回测账户写自己的状态文件）。
pub fn save_to(path: &Path, state: &State) -> AppResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = with_suffix(path, ".tmp");
    let bak = with_suffix(path, ".bak");
    let text = serde_json::to_string_pretty(state)?;
    {
        // 作用域结尾即关闭句柄；Windows 下文件未关闭时 rename 会失败
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
    }
    if path.exists() {
        // 备份失败不影响主流程，主文件仍然有效
        let _ = std::fs::copy(path, &bak);
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 把 `<kind>.json` 轮转为 `<kind>_bak_<yyyymmdd_HHMMSS>.json`；源文件不存在时静默跳过。
pub fn backup_rotate(paths: &Paths, kind: &str) -> AppResult<()> {
    let src = paths.data_dir().join(format!("{kind}.json"));
    if !src.exists() {
        return Ok(());
    }
    let stamp = util::now_local().format("%Y%m%d_%H%M%S").to_string();
    let dst = paths.data_dir().join(format!("{kind}_bak_{stamp}.json"));
    std::fs::rename(&src, &dst)?;
    util::log(&format!("[状态] 旧文件已备份为 {}", file_label(&dst)));
    Ok(())
}

fn read_state(path: &Path) -> AppResult<State> {
    let text = std::fs::read_to_string(path)?;
    let mut raw: Value = serde_json::from_str(&text)?;
    if !raw.is_object() {
        return Err(AppError::Invalid("state 根节点不是对象".to_string()));
    }
    migrate(&mut raw);
    let state: State = serde_json::from_value(raw)?;
    Ok(state)
}

fn setdefault(obj: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    if !obj.contains_key(key) {
        obj.insert(key.to_string(), value);
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

/// 损坏文件另存为 `state.corrupt_<yyyymmdd_HHMMSS>.json`，保留原名前缀便于追溯。
fn corrupt_path(path: &Path) -> PathBuf {
    rename_with_stamp(path, "corrupt")
}

fn rename_with_stamp(path: &Path, kind: &str) -> PathBuf {
    let stamp = util::now_local().format("%Y%m%d_%H%M%S").to_string();
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "state.json".to_string());
    let renamed = match name.find(".json") {
        Some(index) => format!("{}.{kind}_{stamp}{}", &name[..index], &name[index..]),
        None => format!("{name}.{kind}_{stamp}"),
    };
    match path.parent() {
        Some(dir) => dir.join(renamed),
        None => PathBuf::from(renamed),
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 仅在 `new_state(None)` 也不可用时触发，用于把“不可达”写明而不是 unwrap 崩溃。
fn unreachable_state() -> State {
    State {
        version: STATE_VERSION,
        created_at: String::new(),
        initial_capital: 0.0,
        cash: 0.0,
        positions: BTreeMap::new(),
        orders: Vec::new(),
        daily_net_values: Vec::new(),
        signal_log: Vec::new(),
        peak_assets: 0.0,
        halted: true,
        halt_reason: "账户初始化失败".to_string(),
        halted_at: None,
        ai_usage: AiUsage::default(),
        last_scan: Value::Object(serde_json::Map::new()),
        next_order_id: 1,
        config: Config::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        Paths { base_dir: base }
    }

    #[test]
    fn new_state_rejects_bad_capital() {
        assert!(new_state(Some(0.0)).is_err());
        assert!(new_state(Some(-1.0)).is_err());
        assert!(new_state(Some(f64::NAN)).is_err());
        let state = new_state(None).expect("默认资金");
        assert_eq!(state.initial_capital, 50_000.0);
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.next_order_id, 1);
    }

    #[test]
    fn resolve_prefers_marked_exe_dir_then_cwd() {
        let root = std::env::temp_dir().join(format!(
            "quant-sim-resolve-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        // 发布布局：exe 与 dashboard.html 同目录
        let release = root.join("release");
        std::fs::create_dir_all(&release).expect("创建发布目录");
        std::fs::write(release.join("dashboard.html"), "<html></html>").expect("写看板");
        // 开发布局：cargo run 的 target/debug，两者都没有
        let dev = root.join("target").join("debug");
        std::fs::create_dir_all(&dev).expect("创建 debug 目录");
        // 另有只带 Cargo.toml 的目录，也算项目目录
        let source = root.join("src-tree");
        std::fs::create_dir_all(&source).expect("创建源码目录");
        std::fs::write(source.join("Cargo.toml"), "[package]").expect("写 Cargo.toml");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).expect("创建工作目录");

        assert_eq!(
            Paths::resolve_from(None, Some(release.clone()), cwd.clone()).base_dir,
            release
        );
        assert_eq!(
            Paths::resolve_from(None, Some(source.clone()), cwd.clone()).base_dir,
            source
        );
        // cargo run 场景：exe 目录没有标记文件，必须回落到 cwd，否则 data/ 会进 target/
        assert_eq!(
            Paths::resolve_from(None, Some(dev), cwd.clone()).base_dir,
            cwd
        );
        // 环境变量优先级最高，即便 exe 目录合格
        assert_eq!(
            Paths::resolve_from(
                Some(std::ffi::OsString::from(release.join("custom"))),
                Some(release),
                cwd
            )
            .base_dir,
            root.join("release").join("custom")
        );
        // 环境变量为空串视为未设置
        assert_eq!(
            Paths::resolve_from(Some(std::ffi::OsString::new()), None, PathBuf::from(".")).base_dir,
            PathBuf::from(".")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_twice_creates_backup() {
        let paths = temp_paths("store-save");
        let state = new_state(Some(50_000.0)).expect("新账户");
        save_to(&paths.state(), &state).expect("第一次保存");
        save_to(&paths.state(), &state).expect("第二次保存");
        assert!(paths.state().exists());
        assert!(with_suffix(&paths.state(), ".bak").exists());
        let text = std::fs::read_to_string(paths.state()).expect("读回");
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn corrupt_state_recovers_from_backup() {
        let paths = temp_paths("store-corrupt");
        let state = new_state(Some(50_000.0)).expect("新账户");
        save_to(&paths.state(), &state).expect("第一次保存");
        save_to(&paths.state(), &state).expect("第二次保存");
        std::fs::write(paths.state(), "{broken").expect("写入坏文件");
        let recovered = load(&paths, Some(50_000.0));
        assert_eq!(recovered.cash, 50_000.0);
        assert_eq!(recovered.version, STATE_VERSION);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn corrupt_without_backup_is_archived() {
        let paths = temp_paths("store-archive");
        std::fs::create_dir_all(paths.data_dir()).expect("创建 data");
        std::fs::write(paths.state(), "not json").expect("写入坏文件");
        let state = load(&paths, Some(12_000.0));
        assert_eq!(state.cash, 12_000.0);
        let archived: Vec<_> = std::fs::read_dir(paths.data_dir())
            .expect("列目录")
            .filter_map(|item| item.ok())
            .map(|item| item.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("state.corrupt_"))
            .collect();
        assert_eq!(archived.len(), 1, "损坏文件应另存为 state.corrupt_*.json");
        assert!(!paths.state().exists());
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn migrate_fills_missing_keys() {
        let mut raw = serde_json::json!({
            "cash": 1000.0,
            "initial_capital": 1000.0,
            "positions": {"600000": {"name": "浦发银行", "shares": 100, "avg_cost": 10.0}}
        });
        migrate(&mut raw);
        let state: State = serde_json::from_value(raw).expect("迁移后可反序列化");
        assert_eq!(state.version, STATE_VERSION);
        assert!(!state.halted);
        assert_eq!(state.next_order_id, 1);
        assert_eq!(state.ai_usage.calls, 0);
        let pos = state.positions.get("600000").expect("持仓存在");
        assert_eq!(pos.last_price, 10.0);
        assert_eq!(pos.shares, 100);
        assert!(pos.buy_date.is_none());
        assert!(pos.stop_loss.is_none());
    }

    #[test]
    fn bad_config_field_does_not_break_state() {
        let mut raw = serde_json::json!({
            "cash": 1000.0,
            "initial_capital": 1000.0,
            "config": {"stop_loss_pct": "把 -5 写成了文字", "max_positions": [3]}
        });
        migrate(&mut raw);
        let state: State = serde_json::from_value(raw).expect("坏配置也要能读");
        assert_eq!(state.config.stop_loss_pct, -5.0);
        assert_eq!(state.config.max_positions, 3);
    }

    #[test]
    fn backup_rotate_renames_existing_file() {
        let paths = temp_paths("store-rotate");
        std::fs::write(paths.state(), "{}").expect("写文件");
        backup_rotate(&paths, "state").expect("轮转");
        assert!(!paths.state().exists());
        let rotated: Vec<_> = std::fs::read_dir(paths.data_dir())
            .expect("列目录")
            .filter_map(|item| item.ok())
            .map(|item| item.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("state_bak_"))
            .collect();
        assert_eq!(rotated.len(), 1);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }
}
