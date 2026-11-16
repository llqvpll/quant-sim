//! 个股新闻抓取与情感打分。
//!
//! 直接用代码搜索会返回大量"盘后交易额""主力资金"这类大盘泛新闻，等于给模型喂
//! 噪声。这里做三层清洗：标题必须命中代码或公司名、只保留 7 天内、按标题去重；
//! 情感判断再加 4 字否定窗口（"不及预期""未减持"不会被当成利空/利好）。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{Datelike, NaiveDate};
use regex::Regex;
use serde::Serialize;

use crate::http::HttpClient;
use crate::types::NewsSource;
use crate::util::{
    build_url, contains_sub, ends_with_str, find_sub, json_text, log, round_to, strip_html,
    take_chars, today,
};

const EM_SEARCH_URL: &str = "https://search-api-web.eastmoney.com/search/jsonp";
const HEADERS: [(&str, &str); 2] = [
    (
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    ),
    ("Referer", "https://so.eastmoney.com/"),
];

pub const POSITIVE_KW: [&str; 31] = [
    "涨停",
    "大涨",
    "上涨",
    "增持",
    "回购",
    "中标",
    "签约",
    "合同",
    "大单",
    "订单",
    "预增",
    "扭亏",
    "盈利",
    "增长",
    "超预期",
    "利好",
    "突破",
    "新高",
    "分红",
    "送转",
    "收购",
    "并购",
    "重组",
    "获批",
    "获得",
    "合作",
    "签署",
    "产能扩张",
    "业绩预增",
    "增资",
    "注入",
];
pub const NEGATIVE_KW: [&str; 27] = [
    "跌停",
    "大跌",
    "下跌",
    "减持",
    "亏损",
    "预亏",
    "下滑",
    "下降",
    "减少",
    "利空",
    "处罚",
    "违规",
    "立案",
    "调查",
    "警示",
    "风险",
    "诉讼",
    "质押",
    "爆仓",
    "退市",
    "ST",
    "戴帽",
    "解禁",
    "业绩下滑",
    "净利润下降",
    "亏损扩大",
    "不及预期",
];
const NEGATION: [&str; 9] = ["不", "未", "无", "勿", "别", "暂不", "不宜", "并未", "没有"];
/// 这些词里的"不"不表示否定。
const POSITIVE_EXCEPTIONS: [&str; 3] = ["不错", "不但", "不仅"];
/// 否定窗口长度（字符数）。
const NEGATION_WINDOW: usize = 4;
/// 新闻缓存时长（秒）。
const NEWS_TTL: u64 = 600;
/// 超过该天数的新闻视为过期。
const MAX_AGE_DAYS: i64 = 7;
/// 摘要正文里引用的新闻条数上限。
const HEADLINE_LIMIT: usize = 3;
/// 摘要正文里单条标题的最大字符数。
const HEADLINE_TITLE_CHARS: usize = 50;

#[derive(Clone, Debug, Default)]
pub struct NewsItem {
    pub title: String,
    pub date: String,
    pub content: String,
    pub media_name: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Headline {
    pub title: String,
    pub date: String,
    pub media: String,
    pub sentiment: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NewsSummary {
    pub code: String,
    pub total: u32,
    pub positive: u32,
    pub negative: u32,
    pub neutral: u32,
    pub score: f64,
    pub sentiment: String,
    pub headlines: Vec<Headline>,
    /// 直接拼进 prompt 的文本摘要。
    pub txt: String,
}

pub struct NewsAnalyzer {
    http: HttpClient,
    cache: HashMap<String, (Instant, NewsSummary)>,
}

impl NewsAnalyzer {
    pub fn new(http: HttpClient) -> Self {
        NewsAnalyzer {
            http,
            cache: HashMap::new(),
        }
    }

    pub fn get_news_summary(
        &mut self,
        symbol: &str,
        name: Option<&str>,
        num: usize,
    ) -> NewsSummary {
        let key = format!("{}:{}", symbol, name.unwrap_or(""));
        if let Some((at, summary)) = self.cache.get(&key) {
            if at.elapsed().as_secs() < NEWS_TTL {
                return summary.clone();
            }
        }
        // 多抓一倍，过滤掉泛新闻后仍能凑够 num 条。
        let items = self.fetch_news(symbol, name, num.saturating_mul(2).max(1));
        let relevant = NewsAnalyzer::filter_relevant(&items, symbol, name, today());
        let summary = NewsAnalyzer::summarize(symbol, name, &relevant[..relevant.len().min(num)]);
        self.cache.insert(key, (Instant::now(), summary.clone()));
        summary
    }

    /// 只保留标题命中代码/公司名、7 天内、标题不重复的新闻。
    pub fn filter_relevant(
        items: &[NewsItem],
        symbol: &str,
        name: Option<&str>,
        today: NaiveDate,
    ) -> Vec<NewsItem> {
        let mut result: Vec<NewsItem> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let clean_name = name.map(strip_st_prefix).unwrap_or_default();
        for item in items {
            let title = item.title.trim();
            if title.is_empty() || seen.iter().any(|existing| existing == title) {
                continue;
            }
            let mut matched = title.contains(symbol);
            if let Some(raw_name) = name {
                matched = matched
                    || (!clean_name.is_empty() && title.contains(clean_name.as_str()))
                    || title.contains(raw_name);
            }
            if !matched {
                continue;
            }
            if let Some(published) = NewsAnalyzer::parse_date(&item.date, today) {
                if (today - published).num_days() > MAX_AGE_DAYS {
                    continue;
                }
            }
            seen.push(title.to_string());
            result.push(item.clone());
        }
        result
    }

    /// 情感打分并生成 prompt 文本。
    pub fn summarize(symbol: &str, name: Option<&str>, items: &[NewsItem]) -> NewsSummary {
        if items.is_empty() {
            let label = match name {
                Some(value) if !value.trim().is_empty() => value,
                _ => symbol,
            };
            return NewsSummary {
                code: symbol.to_string(),
                sentiment: "无相关新闻".to_string(),
                txt: format!("暂无与{label}直接相关的最新新闻（已过滤大盘泛新闻）"),
                ..Default::default()
            };
        }

        let mut positive = 0u32;
        let mut negative = 0u32;
        let mut neutral = 0u32;
        let mut headlines: Vec<Headline> = Vec::new();
        for item in items {
            let title = item.title.clone();
            let content = take_chars(&item.content, 120);
            let text = format!("{title} {content}");
            let pos = NewsAnalyzer::hit(&text, &POSITIVE_KW);
            let neg = NewsAnalyzer::hit(&text, &NEGATIVE_KW);
            let sentiment = if pos && !neg {
                positive += 1;
                "利好"
            } else if neg && !pos {
                negative += 1;
                "利空"
            } else {
                neutral += 1;
                "中性"
            };
            headlines.push(Headline {
                title,
                date: item.date.clone(),
                media: item.media_name.clone(),
                sentiment: sentiment.to_string(),
            });
        }

        let total = items.len() as u32;
        let score = round_to((positive as f64 - negative as f64) / total.max(1) as f64, 2);
        let sentiment = if score > 0.2 {
            "偏多"
        } else if score < -0.2 {
            "偏空"
        } else {
            "中性"
        };

        let mut lines: Vec<String> = vec![format!(
            "近{total}条相关新闻, 利好{positive} 利空{negative} 中性{neutral}, 消息面{sentiment}"
        )];
        for headline in headlines.iter().take(HEADLINE_LIMIT) {
            lines.push(format!(
                "  [{}] {}",
                headline.sentiment,
                take_chars(&headline.title, HEADLINE_TITLE_CHARS)
            ));
        }

        NewsSummary {
            code: symbol.to_string(),
            total,
            positive,
            negative,
            neutral,
            score,
            sentiment: sentiment.to_string(),
            headlines,
            txt: lines.join("\n"),
        }
    }

    /// 关键词命中判断，带否定窗口与"不错/不但/不仅"例外。
    pub fn hit(text: &str, keywords: &[&str]) -> bool {
        let chars: Vec<char> = text.chars().collect();
        for keyword in keywords {
            let mut start = 0;
            while let Some(index) = find_sub(&chars, keyword, start) {
                let window_start = index.saturating_sub(NEGATION_WINDOW);
                let prefix = &chars[window_start..index];
                let negated = NEGATION.iter().any(|neg| contains_sub(prefix, neg));
                let exception = POSITIVE_EXCEPTIONS
                    .iter()
                    .any(|item| ends_with_str(prefix, item));
                if negated && !exception {
                    start = index + keyword.chars().count();
                    continue;
                }
                return true;
            }
        }
        false
    }

    /// 解析新闻日期：支持 `2026-10-21`、`2026/10/21`、`2026年10月21日`、`10月21日`。
    pub fn parse_date(text: &str, today: NaiveDate) -> Option<NaiveDate> {
        let raw = text.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(caps) = full_date_regex().captures(raw) {
            let year = caps.get(1)?.as_str().parse::<i32>().ok()?;
            let month = caps.get(2)?.as_str().parse::<u32>().ok()?;
            let day = caps.get(3)?.as_str().parse::<u32>().ok()?;
            return NaiveDate::from_ymd_opt(year, month, day);
        }
        if let Some(caps) = month_day_regex().captures(raw) {
            let month = caps.get(1)?.as_str().parse::<u32>().ok()?;
            let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
            return NaiveDate::from_ymd_opt(today.year(), month, day);
        }
        None
    }

    /// 抓取东方财富搜索接口（JSONP 包裹），失败只记日志并返回空。
    fn fetch_news(&self, symbol: &str, name: Option<&str>, num: usize) -> Vec<NewsItem> {
        let keyword = match name {
            Some(value) if !value.trim().is_empty() => format!("{} {}", symbol, value.trim()),
            _ => symbol.to_string(),
        };
        let payload = serde_json::json!({
            "uid": "",
            "keyword": keyword,
            "type": ["cmsArticleWebOld"],
            "client": "web",
            "clientType": "web",
            "clientVersion": "curr",
            "param": {
                "cmsArticleWebOld": {
                    "searchScope": "default",
                    "sort": "default",
                    "pageIndex": 1,
                    "pageSize": num.max(5),
                    "preTag": "",
                    "postTag": "",
                }
            },
        });
        let url = build_url(
            EM_SEARCH_URL,
            &[("cb", "cb".to_string()), ("param", payload.to_string())],
        );
        let headers = headers_pairs();
        let text = match self.http.get_text(&url, &headers, false) {
            Ok(text) => text,
            Err(err) => {
                log(&format!("[新闻] 获取失败({symbol}): {err}"));
                return Vec::new();
            }
        };

        let body = text.trim();
        let body = if let Some(rest) = body.strip_prefix("cb(") {
            rest.strip_suffix(");")
                .or_else(|| rest.strip_suffix(')'))
                .unwrap_or(rest)
        } else {
            body
        };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(body) else {
            log(&format!("[新闻] 响应不是合法 JSON({symbol})"));
            return Vec::new();
        };
        let items = data
            .get("result")
            .and_then(|node| node.get("cmsArticleWebOld"))
            .and_then(|node| node.as_array());
        let Some(items) = items else {
            return Vec::new();
        };
        items
            .iter()
            .map(|item| NewsItem {
                title: strip_html(&json_text(item.get("title"))),
                date: json_text(item.get("date")),
                content: strip_html(&json_text(item.get("content"))),
                media_name: json_text(item.get("mediaName")),
            })
            .collect()
    }
}

impl NewsSource for NewsAnalyzer {
    fn summary(&mut self, code: &str, name: Option<&str>) -> NewsSummary {
        self.get_news_summary(code, name, 5)
    }
}

fn headers_pairs() -> Vec<(String, String)> {
    HEADERS
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// 去掉 `ST`/`*ST`/`S*ST`/`SST` 前缀，避免公司名匹配被前缀干扰。
fn strip_st_prefix(name: &str) -> String {
    let trimmed = name.trim();
    for prefix in ["S*ST", "*ST", "SST", "ST"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn full_date_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{4})[-年/](\d{1,2})[-月/](\d{1,2})").expect("日期正则为常量"))
}

fn month_day_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2})月(\d{1,2})日").expect("月日正则为常量"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, content: &str, date: &str) -> NewsItem {
        NewsItem {
            title: title.to_string(),
            date: date.to_string(),
            content: content.to_string(),
            media_name: "证券时报".to_string(),
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// 标题不含代码/公司名的泛新闻（盘后交易额、主力资金）会被过滤掉。
    #[test]
    fn relevance_filter_drops_market_noise() {
        let items = vec![
            item(
                "13只股盘后交易额超500万元",
                "600519 也在名单中",
                "2026-10-21",
            ),
            item("贵州茅台发布三季报", "营收增长", "2026-10-21"),
            item("强势股追踪 主力资金净流入", "", "2026-10-20"),
        ];
        let result =
            NewsAnalyzer::filter_relevant(&items, "600519", Some("贵州茅台"), day(2026, 10, 22));
        assert_eq!(result.len(), 1);
        assert!(result[0].title.contains("贵州茅台"));
    }

    /// 否定窗口生效："未减持"不算利空，"不错"属例外，"不及预期"仍算利空。
    #[test]
    fn negation_does_not_flip_sentiment() {
        assert!(!NewsAnalyzer::hit("公司未减持股份", &["减持"]));
        assert!(NewsAnalyzer::hit("业绩不及预期", &["不及预期"]));
        assert!(NewsAnalyzer::hit("基本面不错，业绩增长", &["增长"]));
    }

    /// 摘要正文只出现一次"消息面偏多"，不会拼成"偏偏"。
    #[test]
    fn summary_text_not_duplicated() {
        let items = vec![
            item("贵州茅台业绩增长", "", "2026-10-21"),
            item("贵州茅台回购", "", "2026-10-21"),
        ];
        let summary = NewsAnalyzer::summarize("600519", Some("贵州茅台"), &items);
        assert!(summary.txt.contains("消息面偏"));
        assert!(!summary.txt.contains("偏偏"));
        assert_eq!(summary.total, 2);
        assert_eq!(summary.positive, 2);
        assert_eq!(summary.score, 1.0);
        assert_eq!(summary.sentiment, "偏多");
    }

    /// 支持 `2026-10-21 16:26:00`、`2026年10月21日`、`10月21日` 三种日期写法。
    #[test]
    fn date_parse() {
        let today = day(2026, 10, 22);
        assert_eq!(
            NewsAnalyzer::parse_date("2026-10-21 16:26:00", today),
            Some(day(2026, 10, 21))
        );
        assert!(NewsAnalyzer::parse_date("10月21日", today).is_some());
        assert_eq!(
            NewsAnalyzer::parse_date("2026年10月21日", today),
            Some(day(2026, 10, 21))
        );
        assert_eq!(NewsAnalyzer::parse_date("", today), None);
        assert_eq!(NewsAnalyzer::parse_date("昨天", today), None);
    }

    #[test]
    fn empty_items_yield_neutral_summary() {
        let summary = NewsAnalyzer::summarize("600519", Some("贵州茅台"), &[]);
        assert_eq!(summary.total, 0);
        assert_eq!(summary.sentiment, "无相关新闻");
        assert!(summary.txt.contains("贵州茅台"));
        assert!(summary.headlines.is_empty());
    }

    #[test]
    fn stale_news_is_dropped() {
        let items = vec![
            item("贵州茅台旧闻", "", "2026-10-01"),
            item("贵州茅台新消息", "", "2026-10-21"),
        ];
        let result =
            NewsAnalyzer::filter_relevant(&items, "600519", Some("贵州茅台"), day(2026, 10, 22));
        assert_eq!(result.len(), 1);
        assert!(result[0].title.contains("新消息"));
    }

    #[test]
    fn duplicate_titles_are_dropped() {
        let items = vec![
            item("贵州茅台回购", "", "2026-10-21"),
            item("贵州茅台回购", "", "2026-10-21"),
        ];
        let result =
            NewsAnalyzer::filter_relevant(&items, "600519", Some("贵州茅台"), day(2026, 10, 22));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn negative_keywords_drive_sentiment() {
        let items = vec![item("贵州茅台业绩下滑被处罚", "", "2026-10-21")];
        let summary = NewsAnalyzer::summarize("600519", Some("贵州茅台"), &items);
        assert_eq!(summary.negative, 1);
        assert_eq!(summary.sentiment, "偏空");
    }

    #[test]
    fn st_prefix_is_stripped_for_matching() {
        assert_eq!(strip_st_prefix("*ST海航"), "海航");
        assert_eq!(strip_st_prefix("S*ST前锋"), "前锋");
        assert_eq!(strip_st_prefix("贵州茅台"), "贵州茅台");
    }

    /// 真实接口联调（默认跳过，手动执行：`cargo test live_ -- --ignored --nocapture`）。
    #[test]
    #[ignore = "需要访问东方财富真实接口"]
    fn live_news_returns_items() {
        let http = crate::http::HttpClient::new(
            std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(20),
        )
        .expect("构建客户端");
        let mut analyzer = NewsAnalyzer::new(http);
        let summary = analyzer.get_news_summary("600519", Some("贵州茅台"), 5);
        crate::util::log(&format!(
            "[联调] 新闻 {} 条，利好{} 利空{} 中性{}，消息面{}",
            summary.total, summary.positive, summary.negative, summary.neutral, summary.sentiment
        ));
        // 数据源可达时至少应抓到一条与公司直接相关的新闻。
        assert!(summary.total > 0, "应至少有一条相关新闻");
        assert!(!summary.txt.is_empty());
    }
}
