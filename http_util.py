# -*- coding: utf-8 -*-
"""
零依赖 HTTP 工具
================
用 Python 标准库 urllib 替代第三方 requests，使本项目真正"零第三方依赖、克隆即跑"。
提供:
- get_text / get_json: 带重试的 GET，支持 params / 自定义 headers / 编码 / JSON 解析
- fetch_many: 并发抓取多个请求(IO 密集型提速, 量化扫描场景显著降延迟)

无任何第三方依赖。
"""

import json
import time
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

DEFAULT_HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
}


def get_text(url: str, params: dict = None, headers: dict = None,
             timeout: int = 15, encoding: str = "utf-8", retries: int = 3) -> str:
    """GET 请求并返回解码后的文本（失败重试，最终失败抛 RuntimeError）。"""
    full = url
    if params:
        full = url + ("&" if "?" in url else "?") + urllib.parse.urlencode(params)
    hdrs = dict(DEFAULT_HEADERS)
    if headers:
        hdrs.update(headers)
    last_exc = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(full, headers=hdrs)
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                raw = resp.read()
            return raw.decode(encoding, errors="replace")
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
            time.sleep(0.4 * (attempt + 1))
    raise RuntimeError(f"HTTP 请求失败 {full}: {type(last_exc).__name__}: {last_exc}")


def get_json(url: str, params: dict = None, headers: dict = None,
             timeout: int = 15, retries: int = 3):
    """GET 请求并解析为 JSON。"""
    text = get_text(url, params=params, headers=headers, timeout=timeout,
                    encoding="utf-8", retries=retries)
    return json.loads(text)


def fetch_many(tasks: list, max_workers: int = 10, timeout: int = 15) -> list:
    """并发抓取多个请求，显著降低 IO 等待(行情/列表分页场景)。

    tasks: list of dict，每项支持 url / params / headers / encoding / as_json / timeout。
    返回与 tasks 等长的列表(顺序一致)，单个失败为 None（不抛异常）。
    """
    def _one(t):
        try:
            if t.get("as_json"):
                return get_json(t["url"], t.get("params"), t.get("headers"),
                               t.get("timeout", timeout), t.get("retries", 3))
            return get_text(t["url"], t.get("params"), t.get("headers"),
                           t.get("timeout", timeout), t.get("encoding", "utf-8"),
                           t.get("retries", 3))
        except Exception:
            return None

    if not tasks:
        return []
    # 任务少时不滥用线程池
    workers = min(max_workers, max(1, len(tasks)))
    with ThreadPoolExecutor(max_workers=workers) as ex:
        return list(ex.map(_one, tasks))
