"""LittleDB 性能测试台：一键启动（自动起库 + 可视化界面）。

    python bench/app.py          # 或双击 bench/启动性能测试.bat

- 自动找一个空闲端口启动 LittleDB（数据放在 bench\data\，隔离于真实数据）；
- 打开浏览器进入测试界面：配置规模 → 点「开始测试」→ 看进度和结果；
- 关掉本进程（Ctrl+C / 关窗口）会自动停掉数据库子进程。

只依赖标准库与 sdk/littledb.py。
"""

from __future__ import annotations

import atexit
import json
import os
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "sdk"))

from littledb import connect, LittleDBError  # noqa: E402

EXE = os.environ.get(
    "LDB_EXE",
    # bench\ 在仓库里，dist\ 在仓库的上一级（运行层与源码分层）
    os.path.abspath(os.path.join(HERE, "..", "..", "dist", "littledb.exe")),
)
UI_FILE = os.path.join(HERE, "index.html")
DATA_DIR = os.path.join(HERE, "data")
TABLE = "bench"

# ------------------------------------------------------------------ 全局状态

state_lock = threading.Lock()
state = {
    "running": False,
    "stage": "",          # 当前阶段名
    "progress": 0.0,      # 0..1
    "log": [],            # 界面日志
    "results": None,      # 上一轮结果
    "db_url": None,
    "db_pid": None,
}
db_proc: subprocess.Popen | None = None
stop_event = threading.Event()


def log(message: str) -> None:
    with state_lock:
        state["log"].append(f"[{time.strftime('%H:%M:%S')}] {message}")
        state["log"] = state["log"][-200:]


def set_stage(name: str, progress: float) -> None:
    with state_lock:
        state["stage"] = name
        state["progress"] = progress


# ------------------------------------------------------------------ 数据库生命周期

def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def start_db(sync: str) -> str:
    """启动 LittleDB 子进程，返回 http://127.0.0.1:<port>。"""
    global db_proc
    if not os.path.isfile(EXE):
        raise RuntimeError(f"找不到 {EXE}：请先跑 tools\\verify.ps1 构建，或设置环境变量 LDB_EXE")
    os.makedirs(DATA_DIR, exist_ok=True)

    proto_port, http_port = free_port(), free_port()
    args = [
        EXE, "serve", DATA_DIR,
        "--port", str(proto_port), "--http-port", str(http_port),
        "--no-open", "--token", "bench", "--sync", sync,
    ]
    log("启动数据库：littledb serve（--sync " + sync + "）")
    db_proc = subprocess.Popen(
        args,
        creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    base = f"http://127.0.0.1:{http_port}"

    db = connect(base, token="bench", timeout=3)
    for _ in range(100):  # 最多等 10 秒
        try:
            db.health()
            with state_lock:
                state["db_url"] = base
                state["db_pid"] = db_proc.pid
            log(f"数据库就绪：{base}（数据目录 bench\\data）")
            return base
        except Exception:
            if db_proc.poll() is not None:
                raise RuntimeError("数据库进程启动后立即退出，请检查端口占用")
            time.sleep(0.1)
    raise RuntimeError("数据库 10 秒内未就绪")


def stop_db() -> None:
    global db_proc
    if db_proc and db_proc.poll() is None:
        db_proc.terminate()
        try:
            db_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            db_proc.kill()
        log("数据库已停止")
    db_proc = None
    with state_lock:
        state["db_url"] = None
        state["db_pid"] = None


atexit.register(stop_db)

# ------------------------------------------------------------------ 基准场景

def percentiles(samples: list[float]) -> dict:
    ordered = sorted(samples)
    def pct(p: float) -> float:
        return ordered[min(len(ordered) - 1, int(len(ordered) * p))]
    return {"p50": round(pct(0.50), 2), "p95": round(pct(0.95), 2),
            "p99": round(pct(0.99), 2), "max": round(ordered[-1], 2)}


def timed(client, sql: str, params: list, samples: list[float]):
    t0 = time.perf_counter()
    client.sql(sql, params)
    samples.append((time.perf_counter() - t0) * 1000)


def run_benchmark(cfg: dict) -> dict:
    """按配置跑六类场景，返回结果字典（界面上直接渲染）。"""
    results: dict = {"scenarios": [], "meta": {}}

    def scenario(label: str, ops: int, seconds: float, latency=None, extra="") -> None:
        item = {"label": label, "ops": ops, "seconds": round(seconds, 2),
                "ops_per_s": int(ops / seconds) if seconds > 0 else 0, "extra": extra}
        if latency:
            item["latency"] = latency
        results["scenarios"].append(item)
        log(f"{label}：{item['ops_per_s']:,} ops/s" + (f"（{extra}）" if extra else ""))

    base = state["db_url"]
    db = connect(base, token="bench")
    n_single = int(cfg["single"])
    n_batch = int(cfg["batch"])
    batch_size = max(1, int(cfg["batch_size"]))
    n_point = int(cfg["point"])
    n_range = int(cfg["range"])
    n_update = int(cfg["update"])
    workers = max(1, int(cfg["workers"]))
    seconds = float(cfg["seconds"])
    stages = 6

    # 建表 + 索引（每次运行前重建，保证起点一致）
    try:
        db.drop_table(TABLE)
    except Exception:
        pass
    db.sql(f"CREATE TABLE {TABLE} (id INT PRIMARY KEY AUTO_INCREMENT,"
           f" k INT NOT NULL, v INT DEFAULT 0, pad TEXT)")
    db.sql(f"CREATE INDEX ix_bench_k ON {TABLE}(k)")
    log("表与索引已就绪")

    # 1. 逐条插入
    set_stage("逐条插入", 0 / stages)
    samples: list[float] = []
    started = time.perf_counter()
    for i in range(n_single):
        if stop_event.is_set():
            raise RuntimeError("已取消")
        timed(db, f"INSERT INTO {TABLE} (k, v, pad) VALUES (?, ?, ?)",
              [i, 0, "x" * 64], samples)
        if i % 50 == 0:
            set_stage("逐条插入", (0 + i / max(1, n_single)) / stages)
    scenario("逐条插入", n_single, time.perf_counter() - started, percentiles(samples),
             "每行一次提交（fsync 主导）")
    total = n_single

    # 2. 批量插入
    set_stage("批量插入", 1 / stages)
    started = time.perf_counter()
    done = 0
    while done < n_batch:
        if stop_event.is_set():
            raise RuntimeError("已取消")
        size = min(batch_size, n_batch - done)
        chunk = [{"k": (total + done + j) % 100000, "v": j, "pad": "y" * 64}
                 for j in range(size)]
        db.insert(TABLE, chunk)
        done += size
        set_stage("批量插入", (1 + done / max(1, n_batch)) / stages)
    elapsed = time.perf_counter() - started
    total += n_batch
    scenario("批量插入", n_batch, elapsed, extra=f"{batch_size} 行/批")

    # 3. 主键点查
    set_stage("主键点查", 2 / stages)
    samples = []
    started = time.perf_counter()
    for i in range(n_point):
        if stop_event.is_set():
            raise RuntimeError("已取消")
        timed(db, f"SELECT id, k, v FROM {TABLE} WHERE id = ?",
              [(i * 7 % total) + 1], samples)
        if i % 100 == 0:
            set_stage("主键点查", (2 + i / max(1, n_point)) / stages)
    scenario("主键点查", n_point, time.perf_counter() - started, percentiles(samples))

    # 4. 索引范围扫描
    set_stage("索引范围扫", 3 / stages)
    samples = []
    started = time.perf_counter()
    for i in range(n_range):
        if stop_event.is_set():
            raise RuntimeError("已取消")
        low = (i * 13) % 90000
        timed(db, f"SELECT COUNT(*) FROM {TABLE} WHERE k BETWEEN ? AND ?",
              [low, low + 100], samples)
        if i % 50 == 0:
            set_stage("索引范围扫", (3 + i / max(1, n_range)) / stages)
    scenario("索引范围扫", n_range, time.perf_counter() - started, percentiles(samples))

    # 5. 单行更新
    set_stage("单行更新", 4 / stages)
    samples = []
    started = time.perf_counter()
    for i in range(n_update):
        if stop_event.is_set():
            raise RuntimeError("已取消")
        timed(db, f"UPDATE {TABLE} SET v = v + 1 WHERE id = ?",
              [(i * 11 % total) + 1], samples)
        if i % 50 == 0:
            set_stage("单行更新", (4 + i / max(1, n_update)) / stages)
    scenario("单行更新", n_update, time.perf_counter() - started, percentiles(samples))

    # 6. 混合并发（多会话）
    set_stage("混合并发", 5 / stages)
    counters = [0] * workers
    stop_event.clear()

    def worker(index: int) -> None:
        client = connect(base, token=bench_token())  # 每会话独立连接
        i = 0
        while not stop_event.is_set():
            try:
                client.sql(f"INSERT INTO {TABLE} (k, v) VALUES (?, ?)",
                           [900000 + index * 1000000 + i, i])
                client.value(f"SELECT COUNT(*) FROM {TABLE} WHERE k = ?",
                             [900000 + index * 1000000 + i])
            except LittleDBError as err:
                if err.retryable:
                    time.sleep(0.01)
                    continue
                raise
            i += 1
        counters[index] = i

    threads = [threading.Thread(target=worker, args=(w,)) for w in range(workers)]
    for thread in threads:
        thread.start()
    time.sleep(seconds)
    stop_event.set()
    for thread in threads:
        thread.join()
    stop_event.clear()
    mixed = sum(counters)
    scenario("混合并发", mixed, seconds, extra=f"{workers} 会话 × {seconds:.0f}s 插入+查询")
    set_stage("完成", 1.0)

    health = db.health().get("stats", {})
    results["meta"] = {
        "total_rows": total + mixed,
        "pages": health.get("pages"),
        "mb": round(health.get("bytes", 0) / 1048576, 1),
        "sync": cfg.get("sync", "full"),
    }
    return results


def bench_token() -> str:
    return "bench"


# ------------------------------------------------------------------ 基准线程管理

def start_benchmark(cfg: dict) -> tuple[bool, str]:
    with state_lock:
        if state["running"]:
            return False, "已有测试在跑"
        state["running"] = True
        state["results"] = None
        state["log"] = []
        state["progress"] = 0.0
        state["stage"] = "启动数据库"

    def runner() -> None:
        try:
            stop_event.clear()
            start_db(cfg.get("sync", "full"))
            results = run_benchmark(cfg)
            with state_lock:
                state["results"] = results
            log("全部场景完成 ✓")
        except Exception as err:
            log(f"出错：{err}")
            with state_lock:
                state["stage"] = f"出错：{err}"
        finally:
            stop_db()
            with state_lock:
                state["running"] = False
                state["progress"] = 1.0

    threading.Thread(target=runner, daemon=True).start()
    return True, "已开始"


# ------------------------------------------------------------------ HTTP 服务

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # 静默访问日志
        pass

    def _json(self, payload, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path in ("/", "/index.html"):
            with open(UI_FILE, "rb") as handle:
                body = handle.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == "/api/status":
            with state_lock:
                self._json({key: state[key] for key in
                            ("running", "stage", "progress", "log", "results",
                             "db_url", "db_pid")})
        else:
            self._json({"ok": False, "error": {"code": "NOT_FOUND", "message": "未知路径"}}, 404)

    def do_POST(self):
        if self.path == "/api/start":
            try:
                length = int(self.headers.get("Content-Length") or 0)
                cfg = json.loads(self.rfile.read(length) or "{}")
                ok, message = start_benchmark(cfg)
                self._json({"ok": ok, "message": message}, 200 if ok else 409)
            except Exception as err:
                self._json({"ok": False, "message": str(err)}, 400)
        elif self.path == "/api/stop":
            stop_event.set()
            log("收到停止请求：当前场景结束后终止")
            self._json({"ok": True})
        elif self.path == "/api/stop-db":
            stop_db()
            self._json({"ok": True})
        else:
            self._json({"ok": False, "error": {"code": "NOT_FOUND"}}, 404)


def main() -> int:
    port = free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    url = f"http://127.0.0.1:{port}"
    print("=" * 62)
    print("  LittleDB 性能测试台")
    print(f"  界面地址 : {url}")
    print(f"  数据目录 : {DATA_DIR}")
    print(f"  数据库   : {EXE}")
    print("  关闭本窗口（或 Ctrl+C）会自动停止数据库子进程")
    print("=" * 62)
    if not os.environ.get("NO_BROWSER"):
        threading.Timer(0.5, lambda: webbrowser.open(url)).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n正在停止……")
    finally:
        stop_db()
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
