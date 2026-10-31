"""LittleDB Python 客户端（只用标准库，Python 3.8+）。

服务端只提供 HTTP API，裸用 `urllib` 需要自己处理：`X-Session-Id` 跨请求保持、
错误码 → HTTP 状态码映射、`429/503` 的 `Retry-After`、DECIMAL 是字符串而非数字、
结果集只有列名没有类型。这些在这里都封装好了。

用法：

    from littledb import connect

    db = connect("http://127.0.0.1:7802", token="...")

    db.sql("CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT, price DECIMAL(10,2))")

    ins = db.insert("t", [{"name": "a", "price": "1.50"}])
    ins.ids          # [1]  —— 自增列分配到的 id
    ins.last_id      # 1

    rows = db.rows("SELECT id, name, price FROM t")
    rows[0]          # {'id': 1, 'name': 'a', 'price': '1.50'}  DECIMAL 是字符串

    with db.transaction():          # 抛异常自动 ROLLBACK
        db.sql("INSERT INTO t (name) VALUES ('b')")

    db.upsert("t", [{"id": 1, "name": "a2"}], ["id"], ["name"])

约定：
- DECIMAL 一律以字符串返回（金额不经过浮点），`types` 会告诉你是 "DECIMAL"；
- 没有自增列时 `insert()` 的 `ids` 是空列表、`last_id` 是 None；
- 服务端错误码原样挂在 `LittleDBError.code`，HTTP 状态在 `.status`，
  429/503 建议等待秒数在 `.retry_after`。
"""

from __future__ import annotations

import datetime as _dt
import decimal as _decimal
import json as _json
import time as _time
import urllib.error
import urllib.parse
import urllib.request

__all__ = [
    "Client",
    "LittleDBError",
    "InsertResult",
    "QueryResult",
    "connect",
    "quote_ident",
    "to_param",
]

DEFAULT_BASE = "http://127.0.0.1:7802"


class LittleDBError(Exception):
    """服务端返回的错误（`ok: false`）或网络层错误。"""

    def __init__(self, message: str, code: str = "CLIENT_ERROR", status: int = 0,
                 retry_after: float = 0.0):
        super().__init__(message)
        self.message = message
        self.code = code
        self.status = status
        self.retry_after = retry_after

    @property
    def retryable(self) -> bool:
        """是否值得重试（写冲突 / 限速 / 会话忙）。"""
        return self.code in ("WRITE_CONFLICT", "RATE_LIMITED", "SESSION_BUSY")

    def __str__(self) -> str:  # pragma: no cover - 便于日志
        return f"[{self.code}] {self.message}"


class InsertResult:
    """INSERT / UPSERT 的结果。"""

    def __init__(self, inserted: int, updated: int, skipped: int,
                 ids: list[int], last_id):
        self.inserted = inserted
        self.updated = updated
        self.skipped = skipped
        self.ids = ids
        self.last_id = last_id

    def __repr__(self) -> str:
        return (f"InsertResult(inserted={self.inserted}, updated={self.updated}, "
                f"skipped={self.skipped}, ids={self.ids}, last_id={self.last_id})")


class QueryResult:
    """结果集：既有二维行，也有按列名索引的字典行。"""

    def __init__(self, columns: list[str], types: list[str], rows: list[list]):
        self.columns = columns
        self.types = types
        self.rows = rows

    @property
    def objects(self) -> list[dict]:
        return [dict(zip(self.columns, row)) for row in self.rows]

    def __len__(self) -> int:
        return len(self.rows)

    def __iter__(self):
        return iter(self.objects)

    def __repr__(self) -> str:
        return f"QueryResult(columns={self.columns}, row_count={len(self.rows)})"


def quote_ident(name: str) -> str:
    """标识符加双引号（内部 " 翻倍），与服务端词法器一致。"""
    return '"' + str(name).replace('"', '""') + '"'


def to_param(value):
    """SQL 参数：Date/Datetime 转服务端认的文本，Decimal 转字符串（保精度）。"""
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, _dt.datetime):
        return value.strftime("%Y-%m-%d %H:%M:%S")
    if isinstance(value, _dt.date):
        return value.strftime("%Y-%m-%d")
    if isinstance(value, _decimal.Decimal):
        return str(value)
    return value


class Client:
    def __init__(self, base_url: str = DEFAULT_BASE, token: str | None = None,
                 session_id: str | None = None, timeout: float = 30.0):
        self.base_url = str(base_url or DEFAULT_BASE).rstrip("/")
        self.token = token
        # 服务端回填；带上它才能让 BEGIN 的事务跨请求延续
        self.session_id = session_id
        self.timeout = timeout

    # ------------------------------------------------------------ 底层请求

    def _request(self, method: str, path: str, body=None):
        url = self.base_url + path
        headers = {"Accept": "application/json"}
        data = None
        if body is not None:
            data = _json.dumps(body, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        if self.session_id:
            headers["X-Session-Id"] = self.session_id

        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read()
                sid = response.headers.get("X-Session-Id")
                if sid:
                    self.session_id = sid
                payload = _parse_json(raw, response.status)
        except urllib.error.HTTPError as err:
            raw = err.read()
            sid = err.headers.get("X-Session-Id") if err.headers else None
            if sid:
                self.session_id = sid
            payload = _parse_json(raw, err.code)
            raise _error_from(payload, err.code, err.headers)
        except urllib.error.URLError as err:
            raise LittleDBError(f"无法连接 LittleDB：{err.reason}", code="NETWORK")

        if not isinstance(payload, dict) or payload.get("ok") is not True:
            raise _error_from(payload, 200, None)
        return payload.get("result")

    # ------------------------------------------------------------ 查询

    def sql(self, sql: str, params: list | None = None):
        """执行任意 SQL，返回服务端原始结果。

        单语句直接返回该语句的结果；多语句返回 `{"type": "batch", "results": [...]}`
        （与旧版服务端的单元素 batch 也兼容）。
        """
        result = self._request("POST", "/api/sql",
                               {"sql": sql, "params": [_to_param(p) for p in (params or [])]})
        if isinstance(result, dict) and result.get("type") == "batch":
            results = result.get("results") or []
            if len(results) == 1:
                return results[0]
        return result

    def sql_all(self, sql: str, params: list | None = None) -> list:
        """多语句执行，永远返回结果列表。"""
        result = self._request("POST", "/api/sql",
                               {"sql": sql, "params": [_to_param(p) for p in (params or [])]})
        if isinstance(result, dict) and result.get("type") == "batch":
            return result.get("results") or []
        return [result] if result is not None else []

    def query(self, sql: str, params: list | None = None) -> QueryResult:
        result = self.sql(sql, params)
        if not result or result.get("type") != "select":
            kind = result.get("type") if isinstance(result, dict) else "unknown"
            raise LittleDBError(f"该语句没有结果集（type={kind}）", code="NOT_A_SELECT")
        return QueryResult(result.get("columns") or [],
                           result.get("types") or [],
                           result.get("rows") or [])

    def rows(self, sql: str, params: list | None = None) -> list[dict]:
        """只要「行字典」列表，最常用。"""
        return self.query(sql, params).objects

    def row(self, sql: str, params: list | None = None) -> dict | None:
        result = self.rows(sql, params)
        return result[0] if result else None

    def value(self, sql: str, params: list | None = None):
        """第一行第一列的标量。"""
        result = self.query(sql, params)
        return result.rows[0][0] if result.rows else None

    # ------------------------------------------------------------ 便捷写入

    def insert(self, table: str, rows, on_conflict: dict | None = None) -> InsertResult:
        """批量插入（字典或字典列表），自动拼列并走 `?` 参数。"""
        items = _normalize_rows(rows)
        if not items:
            return InsertResult(0, 0, 0, [], None)
        columns = _union_columns(items)
        sql, params = _insert_sql(table, columns, items)
        if on_conflict:
            sql += _conflict_clause(on_conflict.get("columns") or [],
                                    on_conflict.get("update") or [])
        return _insert_result(self.sql(sql, params))

    def upsert(self, table: str, rows, conflict_columns: list[str],
               update_columns: list[str] | None = None) -> InsertResult:
        """UPSERT：冲突时更新。

        update_columns 省略 = 除冲突列外全部列；传 `[]` = DO NOTHING。
        """
        items = _normalize_rows(rows)
        if not items:
            return InsertResult(0, 0, 0, [], None)
        columns = _union_columns(items)
        conflict = list(conflict_columns or [])
        targets = update_columns
        if targets is None:
            targets = [c for c in columns
                       if c.lower() not in {k.lower() for k in conflict}]
        sql, params = _insert_sql(table, columns, items)
        sql += _conflict_clause(conflict, list(targets))
        return _insert_result(self.sql(sql, params))

    def update_by_id(self, table: str, row_id, patch: dict) -> int:
        columns = list(patch.keys())
        if not columns:
            return 0
        assignments = ", ".join(f"{quote_ident(c)} = ?" for c in columns)
        sql = f"UPDATE {quote_ident(table)} SET {assignments} WHERE id = ?"
        params = [_to_param(patch[c]) for c in columns] + [_to_param(row_id)]
        result = self.sql(sql, params)
        return int(result.get("updated", 0)) if isinstance(result, dict) else 0

    # ------------------------------------------------------------ 事务

    def transaction(self, fn=None, retries: int = 0):
        """事务。

        两种用法：
            with db.transaction():
                db.sql(...)
            db.transaction(lambda db: db.sql(...), retries=2)

        回调里必须用同一个 Client（事务状态绑在会话上）。
        """
        if fn is None:
            return _Transaction(self)
        attempt = 0
        while True:
            try:
                with _Transaction(self):
                    return fn(self)
            except LittleDBError as err:
                if attempt >= retries or not err.retryable:
                    raise
                attempt += 1
                _time.sleep(err.retry_after or min(0.05 * attempt, 0.5))

    # ------------------------------------------------------------ 元数据

    def health(self) -> dict:
        return self._request("GET", "/api/health")

    def tables(self) -> dict:
        return self._request("GET", "/api/tables")

    def schema(self) -> dict:
        return self._request("GET", "/api/schema")

    def table(self, name: str, limit: int | None = None,
              offset: int | None = None) -> QueryResult:
        query = []
        if limit is not None:
            query.append("limit=" + urllib.parse.quote(str(limit)))
        if offset is not None:
            query.append("offset=" + urllib.parse.quote(str(offset)))
        path = "/api/tables/" + urllib.parse.quote(name)
        if query:
            path += "?" + "&".join(query)
        result = self._request("GET", path)
        return QueryResult(result.get("columns") or [],
                           result.get("types") or [],
                           result.get("rows") or [])

    def drop_table(self, name: str):
        return self._request("DELETE", "/api/tables/" + urllib.parse.quote(name))


class _Transaction:
    """`with db.transaction():` 的实现：正常提交，异常回滚后原样抛出。"""

    def __init__(self, client: Client):
        self.client = client
        self.active = False

    def __enter__(self) -> Client:
        self.client.sql("BEGIN")
        self.active = True
        return self.client

    def __exit__(self, exc_type, exc, tb) -> bool:
        if not self.active:
            return False
        self.active = False
        if exc_type is None:
            self.client.sql("COMMIT")
            return False
        try:
            self.client.sql("ROLLBACK")
        except LittleDBError:
            pass  # 回滚失败不掩盖原始异常
        return False


def connect(base_url: str = DEFAULT_BASE, token: str | None = None, **kwargs) -> Client:
    return Client(base_url, token=token, **kwargs)


# ---------------------------------------------------------------- 内部工具

def _to_param(value):
    return to_param(value)


def _parse_json(raw: bytes, status: int):
    try:
        return _json.loads(raw.decode("utf-8")) if raw else {}
    except (ValueError, UnicodeDecodeError):
        raise LittleDBError(f"服务端返回了非 JSON 响应（HTTP {status}）",
                            code="PROTOCOL_ERROR", status=status)


def _error_from(payload, status: int, headers) -> LittleDBError:
    error = (payload or {}).get("error") if isinstance(payload, dict) else None
    error = error or {}
    retry_after = 0.0
    if headers is not None:
        raw = headers.get("Retry-After")
        if raw:
            try:
                retry_after = float(raw)
            except ValueError:
                retry_after = 0.0
    return LittleDBError(error.get("message") or "请求失败",
                         code=error.get("code") or "INTERNAL_ERROR",
                         status=status,
                         retry_after=retry_after)


def _normalize_rows(rows) -> list[dict]:
    if isinstance(rows, dict):
        return [rows]
    return [row for row in (rows or []) if isinstance(row, dict)]


def _union_columns(rows: list[dict]) -> list[str]:
    seen, columns = set(), []
    for row in rows:
        for key in row:
            lower = key.lower()
            if lower in seen:
                continue
            seen.add(lower)
            columns.append(key)
    return columns


def _insert_sql(table: str, columns: list[str], rows: list[dict]) -> tuple[str, list]:
    placeholders = ",".join("(" + ",".join("?" * len(columns)) + ")" for _ in rows)
    params = [_to_param(row.get(column)) for row in rows for column in columns]
    sql = (f"INSERT INTO {quote_ident(table)} "
           f"({', '.join(quote_ident(c) for c in columns)}) VALUES {placeholders}")
    return sql, params


def _conflict_clause(conflict: list[str], update: list[str]) -> str:
    clause = " ON CONFLICT"
    if conflict:
        clause += " (" + ", ".join(quote_ident(c) for c in conflict) + ")"
    if update:
        assignments = ", ".join(
            f"{quote_ident(c)} = EXCLUDED.{quote_ident(c)}" for c in update
        )
        clause += " DO UPDATE SET " + assignments
    else:
        clause += " DO NOTHING"
    return clause


def _insert_result(result) -> InsertResult:
    if not isinstance(result, dict):
        return InsertResult(0, 0, 0, [], None)
    return InsertResult(
        int(result.get("inserted", 0) or 0),
        int(result.get("updated", 0) or 0),
        int(result.get("skipped", 0) or 0),
        [int(i) for i in (result.get("ids") or [])],
        result.get("lastId"),
    )
