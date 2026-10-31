"""Python SDK 集成测试：需要一个正在运行的服务实例。

    set LDB_BASE=http://127.0.0.1:7802
    set LDB_TOKEN=xxx
    python sdk/test_littledb.py

由 tools/verify.ps1 在冒烟阶段自动调用（没有 python 就跳过）。
"""

from __future__ import annotations

import datetime
import decimal
import os
import sys

from littledb import LittleDBError, connect, quote_ident

BASE = os.environ.get("LDB_BASE", "http://127.0.0.1:7802")
TOKEN = os.environ.get("LDB_TOKEN") or None
TABLE = "sdkpy_" + hex(abs(hash(datetime.datetime.now())) % 0xFFFFFF)[2:]

checks = 0


def ok(label: str) -> None:
    global checks
    checks += 1
    print(f"  [OK] {label}")


def expect_error(code: str, status: int, fn) -> LittleDBError:
    try:
        fn()
    except LittleDBError as err:
        assert err.code == code, f"期望 {code}，实际 {err.code}（{err.message}）"
        assert err.status == status, f"期望 HTTP {status}，实际 {err.status}"
        return err
    raise AssertionError(f"期望抛出 {code}，但没有异常")


def main() -> int:
    db = connect(BASE, token=TOKEN)
    t = quote_ident(TABLE)

    # 1. 健康检查
    health = db.health()
    assert health["stats"], "health 应带 stats"
    ok("health")

    # 2. 建表 + 批量插入（自增 id）
    db.sql(f"CREATE TABLE {t} (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT,"
           f" price DECIMAL(10,2), ok BOOL, at DATETIME)")
    ins = db.insert(TABLE, [
        {"name": "a", "price": decimal.Decimal("1.50"), "ok": True,
         "at": datetime.datetime(2026, 10, 31, 12, 0, 0)},
        {"name": "b", "price": "2.25", "ok": False},
    ])
    assert ins.inserted == 2, ins
    assert ins.ids == [1, 2], f"应回报自增 id，实际 {ins.ids}"
    assert ins.last_id == 2, ins
    ok("insert 批量 + 自增 id")

    # 3. 行字典：DECIMAL 是字符串，BOOL 是布尔，DATETIME 是文本
    rows = db.rows(f"SELECT id, name, price, ok, at FROM {t} ORDER BY id")
    assert rows[0] == {"id": 1, "name": "a", "price": "1.50", "ok": True,
                       "at": "2026-10-31 12:00:00"}, rows[0]
    assert rows[1]["price"] == "2.25", rows[1]
    ok("rows() 行字典 + DECIMAL 保真")

    # 4. 列类型
    result = db.query(f"SELECT id, price FROM {t} ORDER BY id")
    assert result.types == ["INT", "DECIMAL"], result.types
    ok("query() types")

    # 5. 标量与单行
    assert db.value(f"SELECT COUNT(*) AS n FROM {t}") == 2
    assert db.row(f"SELECT name FROM {t} WHERE id = 2")["name"] == "b"
    ok("value() / row()")

    # 6. UPSERT：只更新指定列
    up = db.upsert(TABLE, [{"id": 1, "name": "a2"}], ["id"], ["name"])
    assert up.updated == 1 and up.inserted == 0, up
    after = db.row(f"SELECT name, price FROM {t} WHERE id = 1")
    assert after["name"] == "a2" and after["price"] == "1.50", after
    # update_columns=[] → DO NOTHING
    nothing = db.upsert(TABLE, [{"id": 1, "name": "zzz"}], ["id"], [])
    assert nothing.skipped == 1, nothing
    ok("upsert / DO NOTHING")

    # 7. 事务回滚（with 形式）
    def boom() -> None:
        with db.transaction():
            db.sql(f"INSERT INTO {t} (name) VALUES ('rollback')")
            raise RuntimeError("boom")

    try:
        boom()
    except RuntimeError:
        pass
    assert db.value(f"SELECT COUNT(*) AS n FROM {t}") == 2, "回滚后行数不变"
    ok("transaction 上下文回滚")

    # 8. 事务提交（函数形式）
    last_id = db.transaction(
        lambda c: c.sql(f"INSERT INTO {t} (name) VALUES ('tx2')")["lastId"]
    )
    assert isinstance(last_id, int), last_id
    assert db.value(f"SELECT COUNT(*) AS n FROM {t}") == 3
    ok("transaction 函数形式提交")

    # 9. 错误码 / 状态码映射
    err = expect_error("TYPE_ERROR", 400, lambda: db.sql(f"SELECT * FROM {t} WHERE id = '1'"))
    assert err.retryable is False
    expect_error("NOT_FOUND", 404, lambda: db.sql("SELECT * FROM no_such_table_xyz"))
    ok("错误码/状态码映射")

    # 10. schema / tables / 分页
    schema = db.schema()
    assert any(tb["name"] == TABLE for tb in schema["tables"]), "schema 应包含新表"
    page = db.table(TABLE, limit=1, offset=1)
    assert len(page.rows) == 1, page
    ok("schema / table 分页")

    # 11. update_by_id
    assert db.update_by_id(TABLE, 2, {"name": "b2"}) == 1
    assert db.row(f"SELECT name FROM {t} WHERE id = 2")["name"] == "b2"
    ok("update_by_id")

    # 12. 清理
    db.drop_table(TABLE)
    expect_error("NOT_FOUND", 404, lambda: db.sql(f"SELECT * FROM {t}"))
    ok("drop_table")

    print(f"Python SDK 测试通过：{checks} 项")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as err:
        print(f"Python SDK 测试失败：{err}")
        sys.exit(1)
    except LittleDBError as err:
        print(f"Python SDK 测试失败：[{err.code}] {err.message}")
        sys.exit(1)
