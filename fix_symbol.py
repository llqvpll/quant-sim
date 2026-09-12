# -*- coding: utf-8 -*-
import json, urllib.request, gzip

BASE = "http://127.0.0.1:7860"
FLOW_ID = "c25e4d72-5593-48fe-945e-5e3bdde16220"

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

for n in data["data"]["nodes"]:
    if n.get("id") == "MarketData-aF2Ci":
        old = n["data"]["node"]["template"]["symbol"]["value"]
        n["data"]["node"]["template"]["symbol"]["value"] = "600519"
        print(f"MarketData.symbol: {old!r} -> 600519")
    if n.get("id") == "TextInput-5pgMC":
        old = n["data"]["node"]["template"]["input_value"]["value"]
        n["data"]["node"]["template"]["input_value"]["value"] = "600519"
        print(f"TextInput.input_value: {old!r} -> 600519")

body = {"data": data["data"], "name": data.get("name"), "description": data.get("description")}
resp_raw = fetch(f"/api/v1/flows/{FLOW_ID}", token, method="PATCH", body=body)
print("PATCH:", json.loads(resp_raw).get("id") == FLOW_ID and "OK" or "FAIL")
