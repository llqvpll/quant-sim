# -*- coding: utf-8 -*-
"""
生成 AI 模拟盘可视化看板(HTML) —— 实时自动刷新版
================================================
看板页面结构固定，数据完全由前端 JavaScript 每 10 秒从
/api/state 接口拉取并渲染，配合 dashboard_server.py 使用。

页面内容(全部自动刷新):
- 账户总览(总资产/现金/持仓市值/收益率)
- 持仓明细
- 净值曲线(ECharts)
- 交易记录 / AI信号日志
"""

import json
import os

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATE_FILE = os.path.join(BASE_DIR, "data", "state.json")
OUTPUT_FILE = os.path.join(BASE_DIR, "dashboard.html")

REFRESH_INTERVAL = 10  # 页面自动刷新间隔(秒)


def load_state():
    if not os.path.exists(STATE_FILE):
        return None
    with open(STATE_FILE, "r", encoding="utf-8") as f:
        return json.load(f)


def build_dashboard(state=None) -> str:
    """生成实时看板 HTML(数据由前端 fetch /api/state 获取, 零外网依赖)"""
    return r"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>AI 模拟量化交易看板</title>
<style>
* { margin:0; padding:0; box-sizing:border-box; }
body { background:#0f1420; color:#e8eaf0; font-family:'Segoe UI', 'Microsoft YaHei', sans-serif; padding:24px; }
.header { display:flex; justify-content:space-between; align-items:center; margin-bottom:6px; }
.header h1 { font-size:22px; color:#fff; }
.status { display:flex; align-items:center; gap:8px; font-size:13px; color:#8b95a7; }
.dot { width:10px; height:10px; border-radius:50%; display:inline-block; }
.dot.green { background:#52c41a; box-shadow:0 0 8px #52c41a; }
.dot.red { background:#f5222d; box-shadow:0 0 8px #f5222d; }
.dot.gray { background:#666; }
.sub-bar { color:#8b95a7; font-size:12px; margin-bottom:18px; }
.sub-bar b { color:#c9d1e0; font-weight:normal; }
.grid { display:grid; grid-template-columns:repeat(4, 1fr); gap:16px; margin-bottom:20px; }
.card { background:#1a2233; border-radius:12px; padding:18px; border:1px solid #2a3550; }
.card .label { color:#8b95a7; font-size:12px; margin-bottom:6px; }
.card .value { font-size:26px; font-weight:bold; color:#fff; }
.card .value.red { color:#f5222d; } .card .value.green { color:#52c41a; }
.card .sub { color:#8b95a7; font-size:12px; margin-top:4px; }
.panel { background:#1a2233; border-radius:12px; padding:18px; margin-bottom:20px; border:1px solid #2a3550; }
.panel h2 { font-size:15px; color:#c9d1e0; margin-bottom:12px; }
.panel h2 small { color:#8b95a7; font-weight:normal; font-size:12px; }
table { width:100%; border-collapse:collapse; font-size:13px; }
th { text-align:left; color:#8b95a7; font-weight:normal; padding:8px 6px; border-bottom:1px solid #2a3550; }
td { padding:8px 6px; border-bottom:1px solid #222c42; color:#d5dbe6; }
td small { color:#8b95a7; }
td.empty, td .empty { text-align:center; color:#999; }
#chart { width:100%; height:320px; position:relative; background:#141b2b; border-radius:8px; }
.err { background:#2a1b1b; border:1px solid #5c2b2b; color:#ff9c9c; border-radius:8px; padding:12px 16px; margin-bottom:16px; font-size:13px; }
.halt-banner { background:#3a2a10; border:1px solid #6b4a1a; color:#ffd591; border-radius:8px; padding:10px 16px; margin-bottom:16px; font-size:13px; display:none; }
@media (max-width: 900px) { .grid { grid-template-columns:repeat(2, 1fr); } }
</style>
</head>
<body>

<div class="header">
    <h1>📈 AI 模拟量化交易看板</h1>
    <div class="status"><span class="dot gray" id="bot-dot"></span><span id="bot-text">连接中...</span></div>
</div>
<div class="sub-bar">
    自动刷新 <b id="last-refresh">--:--:--</b> · <span id="last-scan">尚未扫描</span> · <span id="cap-info">虚拟资金 --</span>
</div>

<div class="err" id="err" style="display:none">⚠️ 无法连接看板服务，请确认已通过桌面「一键启动」脚本运行本系统。</div>
<div class="halt-banner" id="halt-banner">⛔ 熔断中：总资产回撤超阈值，已暂停新买入</div>

<div class="grid">
    <div class="card">
        <div class="label">总资产</div>
        <div class="value" id="val-total">--</div>
        <div class="sub">初始 <span id="val-init">--</span></div>
    </div>
    <div class="card">
        <div class="label">总收益率</div>
        <div class="value" id="val-return">--</div>
        <div class="sub">累计盈亏 <span id="val-pnl">--</span></div>
    </div>
    <div class="card">
        <div class="label">可用现金</div>
        <div class="value" id="val-cash">--</div>
        <div class="sub">持仓市值 <span id="val-mv">--</span></div>
    </div>
    <div class="card">
        <div class="label">持仓股票</div>
        <div class="value" id="val-pos-count">--</div>
        <div class="sub">累计交易 <span id="val-orders">--</span> 笔</div>
    </div>
</div>

<div class="panel">
    <h2>净值曲线 <small>实时</small></h2>
    <div id="chart"></div>
</div>

<div class="panel">
    <h2>当前持仓</h2>
    <table>
        <tr><th>股票</th><th>持仓股数</th><th>成本价</th><th>现价</th><th>盈亏</th></tr>
        <tbody id="pos-body"><tr><td colspan="5" class="empty">加载中...</td></tr></tbody>
    </table>
</div>

<div class="panel">
    <h2>最近交易</h2>
    <table>
        <tr><th>时间</th><th>方向</th><th>股票</th><th>股数</th><th>价格</th><th>原因</th></tr>
        <tbody id="order-body"><tr><td colspan="6" class="empty">加载中...</td></tr></tbody>
    </table>
</div>

<div class="panel">
    <h2>最近 AI 信号 <small>含消息面</small></h2>
    <table>
        <tr><th>时间</th><th>股票</th><th>信号</th><th>置信度</th><th>状态</th></tr>
        <tbody id="signal-body"><tr><td colspan="5" class="empty">加载中...</td></tr></tbody>
    </table>
</div>

<div class="panel">
    <h2>🧠 AI 学习进度 <small>从已平仓交易中提炼经验, 越跑越聪明</small></h2>
    <div class="grid" style="grid-template-columns:repeat(4,1fr);margin-bottom:14px">
        <div class="card">
            <div class="label">已平仓交易</div>
            <div class="value" id="exp-count" style="font-size:20px">--</div>
        </div>
        <div class="card">
            <div class="label">历史胜率</div>
            <div class="value" id="exp-winrate" style="font-size:20px">--</div>
        </div>
        <div class="card">
            <div class="label">平均每笔收益</div>
            <div class="value" id="exp-avgpnl" style="font-size:20px">--</div>
        </div>
        <div class="card">
            <div class="label">盈亏比</div>
            <div class="value" id="exp-pf" style="font-size:20px">--</div>
        </div>
    </div>
    <div id="exp-detail" style="color:#8b95a7;font-size:13px;line-height:1.8">暂无交易经验, 完成几笔交易后自动生成</div>
    <div style="margin-top:10px;color:#8b95a7;font-size:12px;border-top:1px solid #2a3550;padding-top:10px">
        💰 成本控制: 累计 AI 调用 <b id="usage-calls" style="color:#c9d1e0">0</b> 次 · 规则预筛免费跳过 <b id="usage-skipped" style="color:#c9d1e0">0</b> 只(技术规则打分, 零成本)
    </div>
</div>

<script>
const REFRESH_MS = 10000;   // 10 秒刷新一次
let chart = null;

const $ = id => document.getElementById(id);

function esc(s) {
    return String(s == null ? '' : s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function fmt(n, digits) {
    if (n == null || isNaN(n)) return '--';
    const d = (digits == null) ? 0 : digits;
    return Number(n).toLocaleString('zh-CN', {minimumFractionDigits: d, maximumFractionDigits: d});
}

function marketValue(state) {
    let mv = 0;
    for (const code in (state.positions || {})) {
        const p = state.positions[code];
        mv += p.shares * (p.last_price || p.avg_cost || 0);
    }
    return mv;
}

// ---------- 顶部状态 ----------
function renderBot(bot) {
    const dot = $('bot-dot'), txt = $('bot-text');
    if (!bot) { dot.className = 'dot gray'; txt.textContent = '服务状态未知'; return; }
    dot.className = bot.running ? 'dot green' : 'dot red';
    txt.textContent = bot.running ? '● 机器人运行中' : '● 机器人已停止';
    $('last-scan').textContent = bot.last_scan ? '上次扫描 ' + bot.last_scan.slice(11, 19) : '尚未扫描';
}// ---------- 卡片 ----------
function renderCards(state) {
    const init = state.initial_capital || 0;
    const cash = state.cash || 0;
    const mv = marketValue(state);
    const total = cash + mv;
    const ret = init ? (total - init) / init * 100 : 0;
    const pnl = total - init;

    $('cap-info').textContent = '虚拟资金 ¥' + fmt(init, 0);
    // 熔断横幅
    const halt = $('halt-banner');
    if (state.halted) {
        halt.style.display = 'block';
        halt.textContent = '⛔ 熔断中：' + (state.halt_reason || '总资产回撤超阈值，已暂停新买入');
    } else {
        halt.style.display = 'none';
    }

    $('val-total').textContent = '¥' + fmt(total, 0);
    $('val-init').textContent = '¥' + fmt(init, 0);
    const retEl = $('val-return');
    retEl.textContent = (ret >= 0 ? '+' : '') + ret.toFixed(2) + '%';
    retEl.className = 'value ' + (ret >= 0 ? 'red' : 'green');
    $('val-pnl').textContent = (pnl >= 0 ? '+' : '') + fmt(pnl, 0);
    $('val-pnl').style.color = pnl >= 0 ? '#f5222d' : '#52c41a';
    $('val-cash').textContent = '¥' + fmt(cash, 0);
    $('val-mv').textContent = '¥' + fmt(mv, 0);
    $('val-pos-count').textContent = Object.keys(state.positions || {}).length;
    $('val-orders').textContent = (state.orders || []).length;
}

// ---------- 净值曲线(纯Canvas, 零依赖) ----------
let chartCanvas = null, chartCtx = null;
function initChart() {
    const c = document.getElementById('chart');
    if (!c) return;
    // 创建 canvas 铺满容器
    c.innerHTML = '';
    const cv = document.createElement('canvas');
    cv.width = c.clientWidth || 800;
    cv.height = c.clientHeight || 320;
    cv.style.width = '100%';
    cv.style.height = '100%';
    c.appendChild(cv);
    chartCanvas = cv;
    chartCtx = cv.getContext('2d');
    // 窗口缩放时重绘
    window.addEventListener('resize', () => {
        if (!chartCanvas || !chartCanvas.parentElement) return;
        chartCanvas.width = chartCanvas.parentElement.clientWidth || 800;
        chartCanvas.height = chartCanvas.parentElement.clientHeight || 320;
        redrawChart();
    });
}

function redrawChart() {
    if (!chartCtx) return;
    const ctx = chartCtx, W = chartCanvas.width, H = chartCanvas.height;
    ctx.clearRect(0, 0, W, H);
    if (!window._chartData || window._chartData.dates.length < 2) {
        ctx.fillStyle = '#8b95a7';
        ctx.font = '13px Microsoft YaHei, sans-serif';
        ctx.textAlign = 'center';
        ctx.fillText('暂无净值数据', W / 2, H / 2);
        return;
    }
    const dates = window._chartData.dates, assets = window._chartData.assets;
    const padL = 70, padR = 20, padT = 20, padB = 30;
    const iw = W - padL - padR, ih = H - padT - padB;
    let min = Math.min.apply(null, assets), max = Math.max.apply(null, assets);
    if (max - min < 1) { min -= 1; max += 1; }
    const span = max - min;
    const px = i => padL + (dates.length <= 1 ? 0 : i / (dates.length - 1) * iw);
    const py = v => padT + (1 - (v - min) / span) * ih;

    // 网格线
    ctx.strokeStyle = '#222c42';
    ctx.lineWidth = 1;
    ctx.fillStyle = '#8b95a7';
    ctx.font = '11px Microsoft YaHei, sans-serif';
    ctx.textAlign = 'right';
    for (let g = 0; g <= 4; g++) {
        const v = min + span * g / 4, y = padT + ih * g / 4;
        ctx.beginPath(); ctx.moveTo(padL, y); ctx.lineTo(W - padR, y); ctx.stroke();
        ctx.fillText(v.toFixed(0), padL - 6, y + 4);
    }
    // X轴日期(最多显示6个)
    ctx.textAlign = 'center';
    const step = Math.max(1, Math.floor(dates.length / 6));
    for (let i = 0; i < dates.length; i += step) {
        ctx.fillText(String(dates[i]).slice(5), px(i), H - 8);
    }
    // 折线 + 面积
    ctx.beginPath();
    ctx.moveTo(px(0), py(assets[0]));
    for (let i = 1; i < assets.length; i++) ctx.lineTo(px(i), py(assets[i]));
    ctx.strokeStyle = '#f5222d';
    ctx.lineWidth = 2;
    ctx.stroke();
    // 面积
    ctx.lineTo(px(assets.length - 1), padT + ih);
    ctx.lineTo(px(0), padT + ih);
    ctx.closePath();
    ctx.fillStyle = 'rgba(245,34,45,0.12)';
    ctx.fill();
}

function updateChart(state) {
    const init = state.initial_capital || 0;
    const nvs = state.daily_net_values || [];
    const today = new Date().toISOString().slice(0, 10);
    const dates = [], assets = [];
    for (const nv of nvs) { dates.push(nv.date); assets.push(nv.total_assets); }
    const total = (state.cash || 0) + marketValue(state);
    if (dates.length && dates[dates.length - 1] === today) {
        assets[assets.length - 1] = Math.round(total * 100) / 100;   // 当天曲线实时跳动
    } else {
        dates.push(today);
        assets.push(Math.round(total * 100) / 100);
    }
    if (!dates.length || dates[0] !== '初始') { dates.unshift('初始'); assets.unshift(init); }
    window._chartData = { dates: dates, assets: assets };
    redrawChart();
}

// ---------- 表格 ----------
function renderPositions(state) {
    const tbody = $('pos-body');
    const positions = state.positions || {};
    const keys = Object.keys(positions);
    if (!keys.length) {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">空仓</td></tr>';
        return;
    }
    let html = '';
    for (const code of keys) {
        const p = positions[code];
        const cur = p.last_price || p.avg_cost || 0;
        const pnlPct = p.avg_cost ? (cur - p.avg_cost) / p.avg_cost * 100 : 0;
        const color = pnlPct >= 0 ? '#f5222d' : '#52c41a';
        html += '<tr><td>' + esc(p.name || code) + '<br><small>' + esc(code) + '</small></td>'
              + '<td>' + fmt(p.shares, 0) + '</td>'
              + '<td>' + fmt(p.avg_cost, 2) + '</td>'
              + '<td>' + fmt(cur, 2) + '</td>'
              + '<td style="color:' + color + ';font-weight:bold">' + (pnlPct >= 0 ? '+' : '') + pnlPct.toFixed(2) + '%</td></tr>';
    }
    tbody.innerHTML = html;
}

function renderOrders(state) {
    const tbody = $('order-body');
    const orders = state.orders || [];
    if (!orders.length) {
        tbody.innerHTML = '<tr><td colspan="6" class="empty">暂无交易</td></tr>';
        return;
    }
    let html = '';
    for (const o of orders.slice(-15).reverse()) {
        const color = o.action === 'BUY' ? '#f5222d' : '#52c41a';
        html += '<tr><td>' + esc(String(o.time).slice(0, 16)) + '</td>'
              + '<td style="color:' + color + ';font-weight:bold">' + (o.action === 'BUY' ? '买入' : '卖出') + '</td>'
              + '<td>' + esc(o.name || '') + '<br><small>' + esc(o.code || '') + '</small></td>'
              + '<td>' + fmt(o.shares, 0) + '</td>'
              + '<td>' + fmt(o.price, 2) + '</td>'
              + '<td>' + esc((o.reason || '').slice(0, 40)) + '</td></tr>';
    }
    tbody.innerHTML = html;
}

function renderSignals(state) {
    const tbody = $('signal-body');
    const signals = state.signal_log || [];
    if (!signals.length) {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">暂无信号</td></tr>';
        return;
    }
    const colorMap = { buy: '#f5222d', sell: '#52c41a', wait: '#faad14', hold: '#faad14', error: '#f5222d' };
    let html = '';
    for (const s of signals.slice(-10).reverse()) {
        const sig = s.signal || '';
        const color = colorMap[sig] || '#999';
        const label = { buy: '买入', sell: '卖出', wait: '观望', hold: '持有', error: '错误' }[sig] || sig;
        const conf = (s.confidence == null) ? '--' : (Number(s.confidence) * 100).toFixed(0) + '%';
        html += '<tr><td>' + esc(String(s.time || '').slice(0, 16)) + '</td>'
              + '<td>' + esc(s.name || '') + '<br><small>' + esc(s.code || '') + '</small></td>'
              + '<td style="color:' + color + ';font-weight:bold">' + label + '</td>'
              + '<td>' + conf + '</td>'
              + '<td>' + (s.executed ? '<span style="color:#52c41a">已执行</span>' : '<span style="color:#faad14">未执行</span>') + '</td></tr>';
    }
    tbody.innerHTML = html;
}

// ---------- AI 学习进度 ----------
function renderExperience(exp) {
    if (!exp || !exp.stats || !exp.stats.total_trades) {
        $('exp-count').textContent = '0';
        $('exp-winrate').textContent = '--';
        $('exp-avgpnl').textContent = '--';
        $('exp-pf').textContent = '--';
        $('exp-detail').textContent = '暂无交易经验, 完成几笔交易后自动生成. 机器人会统计自己的信号胜率, 越跑越聪明.';
    } else {
        const st = exp.stats;
        $('exp-count').textContent = st.total_trades;
        $('exp-winrate').textContent = st.win_rate + '%';
        const avg = $('exp-avgpnl');
        avg.textContent = (st.avg_pnl >= 0 ? '+' : '') + st.avg_pnl + '%';
        avg.style.color = st.avg_pnl >= 0 ? '#f5222d' : '#52c41a';
        $('exp-pf').textContent = st.profit_factor;

        let detail = '<b>按置信度分段:</b> ';
        const cs = st.confidence_stats || {};
        const parts = [];
        for (const k in cs) {
            parts.push(k + ' ' + cs[k].count + '笔 胜率' + cs[k].win_rate + '% 均收益' + (cs[k].avg_pnl >= 0 ? '+' : '') + cs[k].avg_pnl + '%');
        }
        detail += parts.join(' | ') || '暂无';
        if (st.recent && st.recent.length) {
            detail += '<br><b>最近平仓:</b> ';
            detail += st.recent.map(r => r.name + '(' + r.code + ') ' + (r.pnl_pct >= 0 ? '+' : '') + r.pnl_pct + '%').join(' | ');
        }
        $('exp-detail').innerHTML = detail;
    }
    // 成本统计(state.ai_usage)
    const usage = (window._state && window._state.ai_usage) || { calls: 0, rule_skipped: 0 };
    $('usage-calls').textContent = usage.calls || 0;
    $('usage-skipped').textContent = usage.rule_skipped || 0;
}

// ---------- 主渲染 ----------
function render(data) {
    renderBot(data.bot);
    const state = data.state;
    window._state = state;
    if (!state) {
        $('val-total').textContent = '--';
        $('val-return').textContent = '--';
        $('val-cash').textContent = '--';
        $('val-pos-count').textContent = '--';
        $('pos-body').innerHTML = '<tr><td colspan="5" class="empty">暂无模拟盘数据，请先启动交易机器人</td></tr>';
        $('order-body').innerHTML = '<tr><td colspan="6" class="empty">暂无交易</td></tr>';
        $('signal-body').innerHTML = '<tr><td colspan="5" class="empty">暂无信号</td></tr>';
        return;
    }
    renderCards(state);
    updateChart(state);
    renderPositions(state);
    renderOrders(state);
    renderSignals(state);
    renderExperience(data.experience);
}

// ---------- 轮询 ----------
async function refresh() {
    try {
        const res = await fetch('/api/state', { cache: 'no-store' });
        if (!res.ok) throw new Error('HTTP ' + res.status);
        const data = await res.json();
        if (!data.ok || !data.state) throw new Error('no data');
        render(data);
        $('err').style.display = 'none';
    } catch (e) {
        $('err').style.display = 'block';
    } finally {
        $('last-refresh').textContent = new Date().toLocaleTimeString('zh-CN', { hour12: false });
    }
}

refresh();
setInterval(refresh, REFRESH_MS);
initChart();
</script>
</body>
</html>"""


if __name__ == "__main__":
    html = build_dashboard()
    with open(OUTPUT_FILE, "w", encoding="utf-8") as f:
        f.write(html)
    print(f"看板已生成: {OUTPUT_FILE} (由 dashboard_server.py 提供服务)")
