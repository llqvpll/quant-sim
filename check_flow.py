# -*- coding: utf-8 -*-
import json, urllib.request, gzip, os

BASE = "http://127.0.0.1:7860"
FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"
BASE_DIR = os.path.dirname(os.path.abspath(__file__))

def fetch(url, token=None, method="GET", body=None):
    r = urllib.request.Request(BASE + url, method=method)
    if token:
        r.add_header("Authorization", "Bearer " + token)
    r.add_header("Accept-Encoding", "identity")
    r.add_header("Content-Type", "application/json")
    data = json.dumps(body).encode() if body is not None else None
    resp = urllib.request.urlopen(r, data=data, timeout=30)
    raw = resp.read()
    if resp.headers.get("Content-Encoding") == "gzip":
        raw = gzip.decompress(raw)
    return raw

token = json.loads(fetch("/api/v1/auto_login"))["access_token"]
raw = fetch(f"/api/v1/flows/{FLOW_ID}", token)
data = json.loads(raw)
with open(os.path.join(BASE_DIR, "flow_current.json"), "w", encoding="utf-8") as f:
    json.dump(data, f, ensure_ascii=False, indent=2)

print("流程名:", data.get("name"))
for n in data["data"]["nodes"]:
    nid = n.get("id", "")
    nd = n.get("data", {})
    if nid in ("TextInput-5pgMC", "MarketData-aF2Ci", "Prompt-GO1Cu"):
        tpl = nd.get("node", {}).get("template", {})
        print(f"\n--- {nid} ({nd.get('node',{}).get('display_name','')}) ---")
        for k, v in tpl.items():
            if isinstance(v, dict) and k != "code":
                print(f"  {k} = {v.get('value', '')!r}")
            elif isinstance(v, dict) and k == "code":
                print(f"  code = {len(v.get('value',''))} chars")
