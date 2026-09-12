# -*- coding: utf-8 -*-
"""
消息面模块
==========
抓取个股最新新闻/公告, 做利好/利空情感打分, 供 AI 多重预测时叠加消息面:
- 东方财富个股新闻搜索接口(按代码精确匹配)
- 标题+摘要关键词情感打分(利好词/利空词)
- 输出结构化的消息面摘要: 新闻条数/情感得分/关键新闻标题

用法:
    from news import NewsAnalyzer
    na = NewsAnalyzer()
    result = na.get_news_summary("300816")
    # {"code","total","positive","negative","neutral","score","sentiment","headlines","txt"}
"""

import json
import time
import urllib.parse

import requests

# 东方财富搜索新闻接口
EM_SEARCH_URL = "https://search-api-web.eastmoney.com/search/jsonp"
HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    "Referer": "https://so.eastmoney.com/",
}

# 利好关键词
POSITIVE_KW = [
    "涨停", "大涨", "上涨", "增持", "回购", "中标", "签约", "合同", "大单", "订单",
    "预增", "扭亏", "盈利", "增长", "净利润上升", "超预期", "利好", "突破", "新高",
    "分红", "送转", "收购", "并购", "重组", "获批", "获得", "合作", "签署", "中标",
    "产能扩张", "业务扩展", "战略合作", "业绩预增", "增资", "注入",
]
# 利空关键词
NEGATIVE_KW = [
    "跌停", "大跌", "下跌", "减持", "亏损", "预亏", "下滑", "下降", "减少", "利空",
    "处罚", "违规", "立案", "调查", "警示", "风险", "诉讼", "质押", "爆仓", "退市",
    "ST", "戴帽", "解禁", "减持计划", "业绩下滑", "净利润下降", "亏损扩大",
]


class NewsAnalyzer:
    """抓取个股新闻并做情感打分"""

    def get_news_summary(self, symbol: str, num: int = 5) -> dict:
        """获取个股最新新闻摘要 + 情感打分"""
        items = self._fetch_news(symbol, num)
        if not items:
            return {
                "code": symbol, "total": 0, "positive": 0, "negative": 0,
                "neutral": 0, "score": 0, "sentiment": "无新闻",
                "headlines": [], "txt": "暂无该股最新新闻",
            }

        positive = negative = neutral = 0
        headlines = []
        for it in items:
            title = it.get("title", "")
            content = it.get("content", "")[:100]
            text = title + content
            pos_hit = any(k in text for k in POSITIVE_KW)
            neg_hit = any(k in text for k in NEGATIVE_KW)
            if pos_hit and not neg_hit:
                positive += 1
                sentiment = "利好"
            elif neg_hit and not pos_hit:
                negative += 1
                sentiment = "利空"
            else:
                neutral += 1
                sentiment = "中性"
            headlines.append({
                "title": title, "date": it.get("date", ""),
                "media": it.get("mediaName", ""), "sentiment": sentiment,
            })

        # 情感得分: 利好+1 利空-1 中性0, 归一化到 -1..1
        total = len(items)
        score = round((positive - negative) / max(total, 1), 2)
        if score > 0.2:
            sentiment = "偏多"
        elif score < -0.2:
            sentiment = "偏空"
        else:
            sentiment = "中性"

        # 给 AI 的摘要文本
        lines = [f"近{total}条新闻, 利好{positive} 利空{negative} 中性{neutral}, 消息面偏{sentiment}"]
        for h in headlines[:3]:
            lines.append(f"  [{h['sentiment']}] {h['title'][:40]}")
        txt = "\n".join(lines)

        return {
            "code": symbol, "total": total,
            "positive": positive, "negative": negative, "neutral": neutral,
            "score": score, "sentiment": sentiment,
            "headlines": headlines, "txt": txt,
        }

    # ---------- 内部实现 ----------
    @staticmethod
    def _fetch_news(symbol: str, num: int = 5) -> list:
        """从东财搜索个股新闻, 返回 [{title, date, content, mediaName}]"""
        try:
            param = {
                "uid": "", "keyword": symbol, "type": ["cmsArticleWebOld"],
                "client": "web", "clientType": "web", "clientVersion": "curr",
                "param": {
                    "cmsArticleWebOld": {
                        "searchScope": "default", "sort": "default",
                        "pageIndex": 1, "pageSize": num, "preTag": "", "postTag": "",
                    }
                },
            }
            r = requests.get(EM_SEARCH_URL,
                             params={"cb": "cb", "param": json.dumps(param, ensure_ascii=False)},
                             headers=HEADERS, timeout=10)
            text = r.text
            # 去掉 JSONP 包装: cb({...})
            if text.startswith("cb("):
                text = text[3:-1]
            data = json.loads(text)
            items = (data.get("result") or {}).get("cmsArticleWebOld") or []
            result = []
            for it in items:
                result.append({
                    "title": it.get("title", ""),
                    "date": it.get("date", ""),
                    "content": it.get("content", ""),
                    "mediaName": it.get("mediaName", ""),
                })
            return result
        except Exception:
            return []


if __name__ == "__main__":
    na = NewsAnalyzer()
    for code in ["300816", "000890", "600519"]:
        print(f"\n=== {code} ===")
        r = na.get_news_summary(code)
        print(r["txt"])
