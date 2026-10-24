from news import NewsAnalyzer


def test_relevance_filter_drops_market_noise():
    analyzer = NewsAnalyzer()
    items = [
        {"title": "13只股盘后交易额超500万元", "content": "600519 也在名单中", "date": "2026-10-21"},
        {"title": "贵州茅台发布三季报", "content": "营收增长", "date": "2026-10-21"},
        {"title": "强势股追踪 主力资金净流入", "content": "", "date": "2026-10-20"},
    ]
    result = analyzer._filter_relevant(items, "600519", "贵州茅台")
    assert len(result) == 1
    assert "贵州茅台" in result[0]["title"]


def test_negation_does_not_flip_sentiment():
    assert not NewsAnalyzer._hit("公司未减持股份", ["减持"])
    assert NewsAnalyzer._hit("业绩不及预期", ["不及预期"])
    assert NewsAnalyzer._hit("基本面不错，业绩增长", ["增长"])


def test_summary_text_not_duplicated():
    analyzer = NewsAnalyzer()
    items = [
        {"title": "贵州茅台业绩增长", "content": "", "date": "2026-10-21"},
        {"title": "贵州茅台回购", "content": "", "date": "2026-10-21"},
    ]
    summary = analyzer._summarize("600519", "贵州茅台", items)
    assert "消息面偏" in summary["txt"]
    assert "偏偏" not in summary["txt"]


def test_date_parse():
    assert NewsAnalyzer._parse_date("2026-10-21 16:26:00").isoformat() == "2026-10-21"
    assert NewsAnalyzer._parse_date("10月21日") is not None
