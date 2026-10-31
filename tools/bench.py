"""LittleDB 性能基准（走 HTTP，按真实部署形态测）。

用法：
    set LDB_BASE=http://127.0.0.1:7802
    set LDB_TOKEN=xxx          （服务没开认证就不用）
    python tools/bench.py

场景（依次执行，输出吞吐与延迟分位数）：
  1. 逐条插入      每行一次提交（sync=full 时每行都 fsync，最坏情况）
  2. 批量插入      500 行/批，测批量吞吐
  3. 主键点查      WHERE id = ?，测 QPS 与 p50/p95/p99 延迟
  4. 索引范围扫    WHERE k BETWEEN ? AND ?（前导列索引）
  5. 单行更新      UPDATE ... WHERE id = ?（读-改-写全路径）
  6. 混合并发      多会话并发 插入+查询（默认 4 线程 × 5 秒）

可用环境变量调规模：BENCH_SINGLE / BENCH_BATCH / BENCH_POINT / BENCH_WORKERS / BENCH_SECONDS

解读提示：
  - sync=full（默认）每笔提交 fsync，逐条插入的数字主要反映磁盘；改 --sync normal 会明显提高
  - 走 HTTP 有网络/JSON 开销，测的是「应用拿到的真实性能」；想测引擎极限走 CLI（独占模式）
  - 数据页有缓存，冷启动比这里慢是正常的
"""

from __future__ import annotations

import os
import statistics
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk"))
from littledb import connect  # noqa: E402

BASE = os.environ.get("LDB_BASE", "http://127.0.0.1:7802")
TOKEN = os.environ.get("LDB_TOKEN") or None

N_SINGLE = int(os.environ.get("BENCH_SINGLE", "300"))
N_BATCH = int(os.environ.get("BENCH_BATCH", "20000"))
BATCH_SIZE = 500
N_POINT = int(os.environ.get("BENCH_POINT", "2000"))
N_RANGE = int(os.environ.get("BENCH_RANGE", "500"))
N_UPDATE = int(os.environ.get("BENCH_UPDATE", "1000"))
WORKERS = int(os.environ.get("BENCH_WORKERS", "4"))
SECONDS = float(os.environ.get("BENCH_SECONDS", "5"))

TABLE = "bench"
lines: list[str] = []


def report(label: str, detail: str) -> None:
    lines.append(f"{label:<12} {detail}")
    print(f"  {label:<12} {detail}")


def rate(n: int, seconds: float) -> str:
    return f"{n} 次 / {seconds:.2f}s = {n / seconds:,.0f} ops/s" if seconds > 0 else "n/a"


def percentiles(samples_ms: list[float]) -> str:
    ordered = sorted(samples_ms)
    def pct(p: float) -> float:
        idx = min(len(ordered) - 1, max(0, int(len(ordered) * p)))
        return ordered[idx]
    return f"p50={pct(0.50):.2f}ms p95={pct(0.95):.2f}ms p99={pct(0.99):.2f}ms max={ordered[-1]:.2f}ms"


def main() -> int:
    db = connect(BASE, token=TOKEN)

    # ---------- 准备：清掉旧表，建表 + 二级索引
    try:
        db.drop_table(TABLE)
    except Exception:
        pass
    db.sql(f"""CREATE TABLE {TABLE} (
        id INT PRIMARY KEY AUTO_INCREMENT,
        k INT NOT NULL,
        v INT DEFAULT 0,
        pad TEXT)""")
    db.sql(f"CREATE INDEX ix_bench_k ON {TABLE}(k)")

    # ---------- 1. 逐条插入（每行一次提交）
    started = time.perf_counter()
    for i in range(N_SINGLE):
        db.sql(f"INSERT INTO {TABLE} (k, v, pad) VALUES (?, ?, ?)",
               [i, 0, "x" * 64])
    elapsed = time.perf_counter() - started
    total = N_SINGLE
    report("逐条插入", rate(N_SINGLE, elapsed))

    # ---------- 2. 批量插入
    started = time.perf_counter()
    for start in range(total, total + N_BATCH, BATCH_SIZE):
        chunk = [{"k": (start + j) % 100000, "v": j, "pad": "y" * 64}
                 for j in range(min(BATCH_SIZE, total + N_BATCH - start))]
        db.insert(TABLE, chunk)
    elapsed = time.perf_counter() - started
    total += N_BATCH
    report("批量插入", f"{N_BATCH} 行 / {elapsed:.2f}s = {N_BATCH / elapsed:,.0f} 行/s")

    # ---------- 3. 主键点查（逐次计时）
    samples: list[float] = []
    started = time.perf_counter()
    for i in range(N_POINT):
        row_id = (i * 7 % total) + 1
        t0 = time.perf_counter()
        db.rows(f"SELECT id, k, v FROM {TABLE} WHERE id = ?", [row_id])
        samples.append((time.perf_counter() - t0) * 1000)
    elapsed = time.perf_counter() - started
    report("主键点查", f"{rate(N_POINT, elapsed)}  {percentiles(samples)}")

    # ---------- 4. 索引范围扫描
    samples = []
    started = time.perf_counter()
    for i in range(N_RANGE):
        low = (i * 13) % 90000
        t0 = time.perf_counter()
        db.value(f"SELECT COUNT(*) FROM {TABLE} WHERE k BETWEEN ? AND ?", [low, low + 100])
        samples.append((time.perf_counter() - t0) * 1000)
    elapsed = time.perf_counter() - started
    report("索引范围扫", f"{rate(N_RANGE, elapsed)}  {percentiles(samples)}")

    # ---------- 5. 单行更新
    samples = []
    started = time.perf_counter()
    for i in range(N_UPDATE):
        row_id = (i * 11 % total) + 1
        t0 = time.perf_counter()
        db.sql(f"UPDATE {TABLE} SET v = v + 1 WHERE id = ?", [row_id])
        samples.append((time.perf_counter() - t0) * 1000)
    elapsed = time.perf_counter() - started
    report("单行更新", f"{rate(N_UPDATE, elapsed)}  {percentiles(samples)}")

    # ---------- 6. 混合并发（多会话：每个线程独立连接）
    counters = [0] * WORKERS
    stop_flag = threading.Event()
    lock = threading.Lock()

    def worker(index: int) -> None:
        client = connect(BASE, token=TOKEN)  # 独立会话，互不阻塞
        i = 0
        while not stop_flag.is_set():
            try:
                client.sql(f"INSERT INTO {TABLE} (k, v) VALUES (?, ?)",
                           [900000 + index * 1000000 + i, i])
                client.value(f"SELECT COUNT(*) FROM {TABLE} WHERE k = ?",
                             [900000 + index * 1000000 + i])
            except Exception as err:  # 会话忙等可重试错误：不中断统计
                if getattr(err, "retryable", False):
                    time.sleep(0.01)
                    continue
                raise
            i += 1
        with lock:
            counters[index] = i

    threads = [threading.Thread(target=worker, args=(w,)) for w in range(WORKERS)]
    for thread in threads:
        thread.start()
    time.sleep(SECONDS)
    stop_flag.set()
    for thread in threads:
        thread.join()
    mixed = sum(counters)
    report("混合并发", f"{WORKERS} 会话 × {SECONDS:.0f}s 完成 {mixed} 组插入+查询 = "
                       f"{mixed / SECONDS:,.0f} ops/s")

    # ---------- 数据规模参考
    health = db.health()
    stats = health.get("stats", {})
    print()
    print(f"  数据规模：{stats.get('tables', '?')} 表 · {stats.get('pages', '?')} 页 · "
          f"{stats.get('bytes', 0) / 1048576:.1f} MB · 总行数约 {total + mixed}")
    print("  说明：sync=full（默认）每笔提交 fsync，逐条插入/更新的数字主要反映磁盘；")
    print("        --sync normal 可显著提高写入吞吐（掉电可能丢最近提交）；HTTP 开销已计入。")

    with open(os.path.join(os.path.dirname(__file__), "bench-result.txt"), "w",
              encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
