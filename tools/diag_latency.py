"""延迟分层诊断：把「客户端/连接层」与「服务器层」的耗时拆开。

对比三种形态的主键点查延迟（同一份数据、同一批查询）：
  A. urllib 每次新建连接（现有 SDK 的行为）
  B. http.client keep-alive（复用同一条 TCP 连接）
  C. keep-alive + 预热后冷热对比（区分页缓存/磁盘因素）

用法：LDB_BASE / LDB_TOKEN 指向一个已启动的服务（tools/bench.py 的数据可以直接复用）。
"""

from __future__ import annotations

import http.client
import json
import os
import statistics
import sys
import time
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from littledb import connect  # noqa: E402

BASE = os.environ.get("LDB_BASE", "http://127.0.0.1:7802")
TOKEN = os.environ.get("LDB_TOKEN") or None
HOST_PORT = BASE.replace("http://", "").replace("https://", "")
HOST, _, PORT = HOST_PORT.partition(":")
PORT = int(PORT or 80)
N = int(os.environ.get("DIAG_N", "1500"))
TABLE = "diag"


def stats(samples: list[float]) -> str:
    ordered = sorted(samples)
    def pct(p):
        return ordered[min(len(ordered) - 1, int(len(ordered) * p))]
    mean = statistics.mean(ordered)
    return (f"mean={mean:.2f}ms p50={pct(0.5):.2f} p95={pct(0.95):.2f} "
            f"p99={pct(0.99):.2f} max={ordered[-1]:.2f}")


def auth_headers() -> dict:
    headers = {"Content-Type": "application/json"}
    if TOKEN:
        headers["Authorization"] = "Bearer " + TOKEN
    return headers


def via_urllib(row_id: int) -> None:
    body = json.dumps({"sql": f"SELECT id, k FROM {TABLE} WHERE id = ?", "params": [row_id]}).encode()
    request = urllib.request.Request(
        BASE + "/api/sql", data=body, headers=auth_headers(), method="POST")
    with urllib.request.urlopen(request, timeout=10) as response:
        response.read()


class KeepAlive:
    def __init__(self):
        self.conn = http.client.HTTPConnection(HOST, PORT, timeout=10)
        headers = auth_headers()
        self.token_headers = {k: v for k, v in headers.items()}

    def ask(self, row_id: int) -> None:
        body = json.dumps({"sql": f"SELECT id, k FROM {TABLE} WHERE id = ?", "params": [row_id]})
        self.conn.request("POST", "/api/sql", body=body, headers=self.token_headers)
        response = self.conn.getresponse()
        response.read()
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}")


def main() -> int:
    db = connect(BASE, token=TOKEN)
    try:
        db.drop_table(TABLE)
    except Exception:
        pass
    rows = 100_000
    print(f"准备数据：{TABLE} {rows} 行（批量插入）…")
    db.sql(f"CREATE TABLE {TABLE} (id INT PRIMARY KEY AUTO_INCREMENT, k INT, pad TEXT)")
    for start in range(0, rows, 1000):
        chunk = [{"k": start + j, "pad": "p" * 64} for j in range(min(1000, rows - start))]
        db.insert(TABLE, chunk)

    total = db.value(f"SELECT COUNT(*) FROM {TABLE}")
    print(f"数据就绪：{total} 行\n")

    # A. urllib 每次新建连接
    samples = []
    for i in range(N):
        row_id = (i * 7 % total) + 1
        t0 = time.perf_counter()
        via_urllib(row_id)
        samples.append((time.perf_counter() - t0) * 1000)
    print(f"A. urllib 每请求新建连接   {stats(samples)}")

    # B. keep-alive 复用连接
    ka = KeepAlive()
    samples = []
    for i in range(N):
        row_id = (i * 7 % total) + 1
        t0 = time.perf_counter()
        ka.ask(row_id)
        samples.append((time.perf_counter() - t0) * 1000)
    print(f"B. keep-alive 复用连接     {stats(samples)}")

    # C. keep-alive 二轮（完全热态）
    samples = []
    for i in range(N):
        row_id = (i * 7 % total) + 1
        t0 = time.perf_counter()
        ka.ask(row_id)
        samples.append((time.perf_counter() - t0) * 1000)
    print(f"C. keep-alive 第二轮       {stats(samples)}")

    print("\n判读：")
    print("  A >> B   → 尾部主要来自「每次新建 TCP 连接 + HTTP 握手」（客户端/连接层）")
    print("  B ≈ C 且尾巴仍在 → 尾部在服务器内部（锁/后台线程/调度），需要继续查")
    return 0


if __name__ == "__main__":
    sys.exit(main())
