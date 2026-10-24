# -*- coding: utf-8 -*-
"""个股新闻与情感打分（v2）。

旧版按 6 位代码搜索东财，返回大量“盘后交易额”“主力资金”之类的大盘泛新闻，
再按关键词硬打分，等于给 AI 喂噪声。v2 做了三件事：
1. 同时用“代码 + 公司名”搜索，并要求标题里出现公司名/代码，否则丢弃；
2. 只保留最近 N 天、去重后的新闻；
3. 情感词加否定窗口（“不及预期”“未减持”不会再被当成利好/利空）。
"""
from __future__ import annotations

import json
import re
import time
from datetime import date
from typing import Dict, List, Optional

import http_util

EM_SEARCH_URL = "https://search-api-web.eastmoney.com/search/jsonp"
HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://so.eastmoney.com/",
}

POSITIVE_KW = [
    "涨停", "大涨", "上涨", "增持", "回购", "中标", "签约", "合同", "大单", "订单",
    "预增", "扭亏", "盈利", "增长", "超预期", "利好", "突破", "新高", "分红",
    "送转", "收购", "并购", "重组", "获批", "获得", "合作", "签署", "产能扩张",
    "业绩预增", "增资", "注入",
]
NEGATIVE_KW = [
    "跌停", "大跌", "下跌", "减持", "亏损", "预亏", "下滑", "下降", "减少", "利空",
    "处罚", "违规", "立案", "调查", "警示", "风险", "诉讼", "质押", "爆仓", "退市",
    "ST", "戴帽", "解禁", "业绩下滑", "净利润下降", "亏损扩大", "不及预期",
]
NEGATION = ("不", "未", "无", "勿", "别", "暂不", "不宜", "并未", "没有")

_CACHE: Dict[str, tuple] = {}
NEWS_TTL = 600.0
MAX_AGE_DAYS = 7


class NewsAnalyzer:
    """抓取个股新闻并做相关性过滤 + 情感打分。"""

    def get_news_summary(self, symbol: str, name: Optional[str] = None, num: int = 5) -> dict:
        key = f"{symbol}:{name or ''}"
        now = time.time()
        hit = _CACHE.get(key)
        if hit and now - hit[0] < NEWS_TTL:
            return hit[1]

        items = self._fetch_news(symbol, name, num * 2)
        relevant = self._filter_relevant(items, symbol, name)
        result = self._summarize(symbol, name, relevant[:num])
        _CACHE[key] = (now, result)
        return result

    # ---------- 过滤与打分 ----------
    @staticmethod
    def _filter_relevant(items: List[dict], symbol: str, name: Optional[str]) -> List[dict]:
        result = []
        seen = set()
        for item in items:
            title = str(item.get("title", "")).strip()
            if not title or title in seen:
                continue
            matched = symbol in title
            if name:
                clean_name = re.sub(r"^(?:\*?ST|S\*ST|SST)", "", str(name)).strip()
                matched = matched or (bool(clean_name) and clean_name in title) or (name in title)
            if not matched:
                continue
            published = NewsAnalyzer._parse_date(item.get("date", ""))
            if published and (date.today() - published).days > MAX_AGE_DAYS:
                continue
            seen.add(title)
            result.append(item)
        return result

    @staticmethod
    def _summarize(symbol: str, name: Optional[str], items: List[dict]) -> dict:
        if not items:
            label = name or symbol
            return {
                "code": symbol, "total": 0, "positive": 0, "negative": 0,
                "neutral": 0, "score": 0.0, "sentiment": "无相关新闻",
                "headlines": [],
                "txt": f"暂无与{label}直接相关的最新新闻（已过滤大盘泛新闻）",
            }

        positive = negative = neutral = 0
        headlines = []
        for item in items:
            title = str(item.get("title", ""))
            content = str(item.get("content", ""))[:120]
            text = title + " " + content
            pos = NewsAnalyzer._hit(text, POSITIVE_KW)
            neg = NewsAnalyzer._hit(text, NEGATIVE_KW)
            if pos and not neg:
                positive += 1
                sentiment = "利好"
            elif neg and not pos:
                negative += 1
                sentiment = "利空"
            else:
                neutral += 1
                sentiment = "中性"
            headlines.append({
                "title": title, "date": item.get("date", ""),
                "media": item.get("mediaName", ""), "sentiment": sentiment,
            })

        total = len(items)
        score = round((positive - negative) / max(total, 1), 2)
        if score > 0.2:
            sentiment = "偏多"
        elif score < -0.2:
            sentiment = "偏空"
        else:
            sentiment = "中性"

        lines = [f"近{total}条相关新闻, 利好{positive} 利空{negative} 中性{neutral}, 消息面{sentiment}"]
        for item in headlines[:3]:
            lines.append(f"  [{item['sentiment']}] {item['title'][:50]}")
        return {
            "code": symbol, "total": total,
            "positive": positive, "negative": negative, "neutral": neutral,
            "score": score, "sentiment": sentiment,
            "headlines": headlines, "txt": "\n".join(lines),
        }

    @staticmethod
    def _hit(text: str, keywords: List[str]) -> bool:
        for keyword in keywords:
            start = 0
            while True:
                index = text.find(keyword, start)
                if index < 0:
                    break
                prefix = text[max(0, index - 4):index]
                # “不错”“不但”“不仅”是否定词例外
                if any(neg in prefix for neg in NEGATION) and not prefix.endswith(("不错", "不但", "不仅")):
                    start = index + len(keyword)
                    continue
                return True
        return False

    @staticmethod
    def _parse_date(value: str) -> Optional[date]:
        text = str(value or "").strip()
        if not text:
            return None
        match = re.search(r"(\d{4})[-年/](\d{1,2})[-月/](\d{1,2})", text)
        if match:
            try:
                return date(int(match.group(1)), int(match.group(2)), int(match.group(3)))
            except ValueError:
                return None
        match = re.search(r"(\d{1,2})月(\d{1,2})日", text)
        if match:
            try:
                return date(date.today().year, int(match.group(1)), int(match.group(2)))
            except ValueError:
                return None
        return None

    # ---------- 抓取 ----------
    @staticmethod
    def _fetch_news(symbol: str, name: Optional[str], num: int = 10) -> List[dict]:
        try:
            keyword = f"{symbol} {name}".strip() if name else str(symbol)
            payload = {
                "uid": "", "keyword": keyword, "type": ["cmsArticleWebOld"],
                "client": "web", "clientType": "web", "clientVersion": "curr",
                "param": {
                    "cmsArticleWebOld": {
                        "searchScope": "default", "sort": "default",
                        "pageIndex": 1, "pageSize": max(num, 5),
                        "preTag": "", "postTag": "",
                    }
                },
            }
            text = http_util.get_text(
                EM_SEARCH_URL,
                params={"cb": "cb", "param": json.dumps(payload, ensure_ascii=False)},
                headers=HEADERS, timeout=10,
            )
            text = text.strip()
            if text.startswith("cb("):
                text = text[3:]
                if text.endswith(");"):
                    text = text[:-2]
                elif text.endswith(")"):
                    text = text[:-1]
            data = json.loads(text)
            items = (data.get("result") or {}).get("cmsArticleWebOld") or []
            result = []
            for item in items:
                result.append({
                    "title": re.sub(r"<[^>]+>", "", str(item.get("title", ""))),
                    "date": item.get("date", ""),
                    "content": re.sub(r"<[^>]+>", "", str(item.get("content", ""))),
                    "mediaName": item.get("mediaName", ""),
                })
            return result
        except Exception as exc:  # noqa: BLE001
            print(f"[新闻] 获取失败({symbol}): {type(exc).__name__}: {exc}")
            return []
