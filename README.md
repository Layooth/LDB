# LittleDB (Rust)

单机嵌入式数据库：4KB slotted page + LDBW2 预写日志 + 快照隔离事务 + 自研 SQL 执行器，
带 LDBP/1 TCP 协议服务和一个单文件 Web 控制台。**运行时零第三方依赖**（只用 std）。

```
littledb-rust-src/     源码（本目录）
  src/                 page / wal / catalog / store / table / codec / engine
  src/sql/             lexer → parser → ast → eval → executor
  src/http.rs          HTTP API + 静态控制台
  src/server.rs        LDBP/1 TCP 服务
  tests/storage.rs     集成测试（重开、崩溃恢复、并发、索引、ALTER）
web/                   Web 控制台（index.html 单文件 + sql-hints.js）
data/                  运行时数据目录（catalog.json / wal.log / data/tN.tbl / littledb.lock）
LittleDB控制台.exe      安全启动器（生成 token → 拉起 littledb.exe → 打开带 token 的控制台）
```

## 快速开始

```powershell
# 双击 LittleDB控制台.exe：自动生成访问令牌、启动服务、打开控制台
# 或者手动：
littledb.exe serve .\data                 # 协议 7801 + 控制台 7802，并自动开浏览器
littledb.exe serve .\data --token secret  # 启用 Bearer 认证
littledb.exe sql .\data "SELECT * FROM t LIMIT 5"
littledb.exe verify .\data                # 输出数据目录概览（表/列/索引/行数/恢复情况）
littledb.exe demo .\tmp-demo              # 建演示库并验证读写
littledb.exe dump .\data -o backup.sql    # 导出为 SQL 脚本（可进版本控制/能改）
littledb.exe load .\new-data backup.sql   # 从脚本导入（--batch 200 控制每批事务）
```

### 配置、日志与后台运行

**配置**：`littledb.json`（默认找 `<数据目录>/littledb.json`，其次 `./littledb.json`，也可 `--config` 指定）。
优先级：**命令行 > 环境变量 `LDB_TOKEN` > 配置文件 > 内置默认**。

```json
{
  "host": "127.0.0.1",
  "port": 7801,
  "httpPort": 7802,
  "token": "换成一串随机字符",
  "maxConnections": 64,
  "slowMs": 200,
  "sync": "full",
  "corsOrigin": "https://app.example.com",
  "authFailLimit": 5,
  "logFile": "littledb.log",
  "logMaxBytes": 8388608
}
```

拼错的键会打一条 `WARN` 而不是静默忽略。

**日志**：`--log-file <文件>`（相对路径按数据目录解析）或配置里的 `logFile`。
不配就写 stdout（保持原行为）。文件为**追加**写，超过 `--log-max-bytes`（默认 8MB）
轮转到 `<文件>.1`（只留一份备份）。时间戳是 UTC（带 `Z`），与 `catalog.json` 一致。

```
2026-10-31 15:05:33Z [INFO] 启动：数据目录 ./data（2 张表 · 1 页 · 4096 字节 · 恢复 0 个事务）
2026-10-31 15:05:33Z [INFO] 协议服务 (LDBP/1) 已监听 tcp://127.0.0.1:7801
2026-10-31 15:05:33Z [WARN] 慢查询 312.4ms: SELECT ...
```

**后台运行**（关掉终端不退出）：

```powershell
littledb start .\data --port 7801 --http-port 7802   # 分离进程 + pid 文件 + 日志
littledb status .\data                               # 是否在跑、pid、日志位置与大小
littledb stop .\data                                 # 结束进程
```

`start` 会在数据目录写 `littledb.pid` 与 `littledb.log`；`stop` 是**直接终止进程**
（等价于一次崩溃），已提交事务靠 WAL 在下次启动时重放，数据不丢，但会走一遍恢复流程。

> 这不是 Windows 服务：真正的 SCM 服务宿主需要 `StartServiceCtrlDispatcher`，
> 要额外 FFI 和管理员权限。开机自启建议用「任务计划程序」在登录时运行
> `littledb start <目录>`，或用 [NSSM](https://nssm.cc/) 把 `serve` 包成服务。

### 备份 / 导出

`dump` 产出**自包含 SQL 脚本**：`CREATE TABLE`（含类型、DECIMAL 标度、NOT NULL / UNIQUE /
DEFAULT / 主键，复合主键用表级写法）、显式 `CREATE INDEX`、以及每 100 行一条的 `INSERT`。
`load` 按 `--batch`（默认 200 条语句）分批包事务导入，任一批失败即回滚并报出错语句。

```sql
CREATE TABLE "players" (
  "id" INT PRIMARY KEY,
  "name" TEXT NOT NULL UNIQUE,
  "score" INT,
  "joined" DATETIME
);
INSERT INTO "players" ("id","name","score","joined") VALUES (1,'player1',100,'2026-10-30 08:00:00'),…;
```

为什么要走 SQL 而不是拷目录：运行中直接拷 `data/` 不安全（页缓存 + WAL 可能不一致）；
SQL 脚本既能当备份、又能进版本控制、还能跨版本导入。DECIMAL 以**字符串**字面量保存
（`'1.50'`），不会经过浮点。

> 控制台的「备份 / 恢复」也已修复两处保真问题：恢复时会重放显式索引（旧版只建表，
> 索引静默消失），并按 `/api/schema` 的 `scale` 字段重建 `DECIMAL(p,s)`（旧版会退回默认标度）。
```

`serve` 选项：`--host`、`-p/--port`、`--http-port`、`--no-http`、`--no-open`、
`--web-dir`、`--token`（或环境变量 `LDB_TOKEN`）、`--cors-origin <列表>`、
`--allow-insecure`、`--max-connections`（默认 64）、`--slow-ms`、
`--sync full|normal`（默认 full：每次提交 fsync；normal：只写 OS 缓存）、`--verbose`。

### 安全默认值

| 项 | 默认行为 | 如何放开 |
|---|---|---|
| 监听地址 | `127.0.0.1`（仅本机） | `--host 0.0.0.0` |
| 无令牌 + 对外监听 | **拒绝启动**（远程无认证 = 谁都能读写） | 加 `--token`，或显式 `--allow-insecure` |
| 令牌传递 | 环境变量 `LDB_TOKEN` 优先于命令行（命令行对其它进程可见） | 启动器已改用环境变量 |
| 猜令牌 | 同一来源连续失败 5 次后指数退避（30s→60s→…上限 15min），HTTP 回 429 + `Retry-After`，LDBP 回 `RATE_LIMITED` | `--auth-fail-limit <n>`（0 = 关闭） |
| 跨源访问 | 只允许同源（不回 `Access-Control-Allow-Origin`） | `--cors-origin "https://a.example,https://b.example"` 或 `*` |
| 控制台令牌存储 | `sessionStorage`（关标签即失效）+ 落地后清掉 URL 里的 `?token=` | — |
| 响应头 | 带 `X-Content-Type-Options: nosniff`、`Cache-Control: no-store` | — |

## 构建与测试

```powershell
build.cmd test            # cargo test（100 个用例）
build.cmd build --release # 发布构建
```

`build.cmd` 只是个包装：本机（Windows Server 2016 + VS2026）的 `link.exe` 缺系统 API，
直接链接会返回 `0xc0000139`，脚本改用 Rust 自带的 `rust-lld`。链接器正常时直接用 cargo 即可。

### 一键门禁与专项脚本

```powershell
pwsh -File tools\verify.ps1              # 测试 → 发布构建 → 部署 → 冒烟（10 项 HTTP/会话/并发/畸形请求）
pwsh -File tools\verify.ps1 -SmokeOnly   # 只跑冒烟（用现有 littledb.exe）
pwsh -File tools\crash-test.ps1 -Rounds 3   # 写入过程中强杀进程，重启后校验「已 ACK 的提交不丢」
build.cmd test --release -- --ignored row_cap
                                         # 50 万行上限的边界用例（慢，默认忽略）
```

## 磁盘格式

| 文件 | 内容 |
|---|---|
| `catalog.json` | 表结构/索引元数据。tmp + fsync + rename 原子写 |
| `wal.log` | LDBW2 二进制预写日志（见下） |
| `data/tN.tbl` | 表 N 的页文件，每页 4096B，按页号顺序排列 |
| `littledb.lock` | 目录独占锁（操作系统级文件锁，第二个进程会被拒绝） |

**页**（slotted page，小端）：`pageType(1) | 保留(3) | pageNo(4) | slotCount(2) | freeOffset(2) |
nextPage(4) | flags(4) | 保留(4)` = 24B 头，之后是每槽 6B 的 slot 目录（offset/len/flags），
记录从页尾向前生长。行号 `row_id = (pageNo << 16) | slot`，因此**单表上限 65535 页 ≈ 256MB**。

**行编码**：`null bitmap(⌈列数/8⌉)` + 非空列依次拼接：INT=i64、REAL=f64、BOOL=1B、
TEXT/DATE/DATETIME/JSON=u32 长度+UTF-8、DECIMAL=i128（标度存 catalog）。单行上限 4060B。

**WAL（LDBW2）**：`"LDBW2" + 版本(1) + 保留(3)`，之后是 `kind(1) | txn(4) | len(4) | payload`：
`1=页镜像`、`2=catalog JSON`、`3=commit`。恢复时只重放带 commit 的记录，尾部残缺字节直接丢弃。

**索引**：全内存 `BTreeMap<key, Vec<rowId>>`，启动时全表扫描重建。key 是**保序编码**
（INT 符号位翻转 + 大端 hex、REAL 同法、TEXT 前缀 `s`），所以 BTreeMap 的字典序 == 值序，
等值与范围扫描都能用。

## 事务与并发

- 每个会话可持有一个开放事务；隔离级别 = **快照隔离**。
  读路径 = 自己的未提交页 → `commit_seq ≤ 快照` 的已提交页 → 磁盘。
- 写-写冲突**页级**检测、**首提交者获胜**，失败方收到 `WRITE_CONFLICT`（HTTP 409），需重试整个事务。
- 唯一约束延迟到提交期检查：语句期只记 delta，提交时顺序应用并检查，撞键返回
  `CONSTRAINT_VIOLATION`（键已被占用，重试无用）。
- 组提交：提交只在临界区内写 WAL + 发布序号，fsync 由后台 `ldb-wal-sync` 线程批量完成，
  调用方在**释放写锁之后**等待确认。
- checkpoint 增量落盘：只写 `seq ≤ 最老活跃快照` 的条目，每轮最多 1024 页，全部落盘才截断 WAL。
- 并发度现状（重要）：**非事务 SELECT 之间真正并行**；写语句整条持有引擎写锁，
  所以写是串行的，多写的收益在「事务级交错」。原因与后续方向见
  `docs/mvcc-design.md` §3.9。

## 类型与比较

`INT / REAL / TEXT / BOOL / DATE / DATETIME / JSON / DECIMAL(p,s)`。
DATE/DATETIME 以文本 `YYYY-MM-DD[ HH:MM:SS]` 存储（无时区，字典序即时间序）。
DECIMAL 是 i128 + 标度的**精确**十进制（金额场景零浮点误差，标度上限 9）。

比较遵循三值逻辑：任一侧为 NULL → 结果 NULL；**非 NULL 但类型不可比 → 报 `TYPE_ERROR`**
（例如 `WHERE id = '5'`，请写 `CAST('5' AS INT)`）。旧版本这种情况会静默返回空集。

## SQL 支持

| 类别 | 支持 |
|---|---|
| DDL | `CREATE TABLE`（列级 PK/NOT NULL/UNIQUE/DEFAULT/**AUTO_INCREMENT**、表级 PK、IF NOT EXISTS）、`DROP TABLE`、`CREATE/DROP [UNIQUE] INDEX`、`ALTER TABLE`（`ADD [COLUMN]` / `DROP [COLUMN]` / `RENAME COLUMN a TO b` / `RENAME TO c`） |
| DML | `INSERT INTO t [(列)] VALUES (...),(...)`、`INSERT ... ON CONFLICT [(列)] DO UPDATE SET ... / DO NOTHING`（UPSERT，可用 `EXCLUDED.列` 引用待插入行）、`UPDATE ... SET ... [WHERE]`、`DELETE FROM ... [WHERE]` |
| 查询 | `WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`/`UNION [ALL]`、`INNER/LEFT/CROSS JOIN`（左深树，嵌套循环）、`IN (SELECT ...)`（非关联单列） |
| 事务 | `BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` |
| 元命令 | `SHOW TABLES`、`DESCRIBE t`、`EXPLAIN <SELECT/INSERT/UPDATE/DELETE>` |
| 表达式 | 算术、比较、`AND/OR/NOT`、`IS [NOT] NULL`、`IN (...)`、`BETWEEN`、`LIKE`、`CAST(x AS 类型)`、`?` 参数 |
| 函数 | `COALESCE LENGTH UPPER LOWER ABS ROUND NOW CURDATE`；聚合 `COUNT SUM AVG MIN MAX`（含 DISTINCT / `COUNT(*)`） |

**不支持**：`RIGHT/FULL JOIN`、派生表、CTE、`EXISTS`、标量子查询、窗口函数、
`INSERT ... SELECT`、`TRUNCATE`、视图/触发器/序列。`ALTER TABLE` 的列变更通过
「影子表重建」实现：**表 id 会变、行号会重排、数据完整保留**，索引按列名迁移。

## LDBP/1 协议（TCP，默认 7801）

```
+------------------+---------------------------+
| uint32 长度 (BE) | UTF-8 JSON 报文体        |
+------------------+---------------------------+
```

| 报文 | 说明 |
|---|---|
| `{"type":"hello","token":"..."}` | 认证（未配置 `--token` 时直接通过）。回 `{"event":"ready"}` |
| `{"type":"ping"}` | 保活 |
| `{"type":"query","id":1,"sql":"...","params":[...]}` | 执行 SQL，回 `{"id":1,"ok":true,"result":{...},"elapsedMs":..}` |

单语句直接返回该语句的结果；多语句（`a;b;c`）返回 `{type:"batch",results:[...]}`。
HTTP 的 `/api/sql` 语义完全一致（客户端可据此做兼容）。

`select` 结果的字段（与 HTTP 的 `/api/sql` 完全一致，客户端按此解析）：

```json
{ "type": "select",
  "columns": ["id", "price"],       // 列名
  "types":   ["INT", "DECIMAL"],    // 每列类型，与 columns 一一对应；推不出为 "UNKNOWN"
  "rows":    [[1, "19.99"]],        // DECIMAL 是字符串；INT/REAL/BOOL 是 JSON 数值/布尔
  "rowCount": 1 }
```

`insert` 结果会回报自增列分配出的值，省掉再查一次：

```json
{ "type": "insert", "inserted": 2, "updated": 0, "skipped": 0, "ids": [1, 2], "lastId": 2 }
```

没有 `AUTO_INCREMENT` 列时 `ids` 为空数组、`lastId` 为 `null`；`updated`/`skipped`
是 UPSERT 命中的更新/跳过行数（普通 INSERT 恒为 0）。
自增列语义：插入时该列为 `NULL`（或列在 `INSERT` 列表里被省略）就分配下一个值；
显式写入更大的值会把游标顶到 `值+1`，所以手工指定 id、`dump`/`load` 都不会撞车。
游标（`nextAutoId`）随 catalog 持久化，重启不丢。

`/api/schema` 的列定义同样带 `type` 与 `scale`（DECIMAL 的标度），索引带
`primary` / `implicit` 标记，供客户端或备份工具原样重建表。
| `{"type":"close"}` | 关闭连接 |

连接建立时服务端先发一帧 `{"event":"welcome"}`。单帧上限 16MB，空闲 5 分钟断开。

## HTTP API（默认 7802）

认证：`Authorization: Bearer <token>`（只认这个头，query 里的 token 不认）。
令牌来自 `--token` 或环境变量 `LDB_TOKEN`；两者都没设时**只允许回环地址**，
对外监听会被拒绝启动（见「安全默认值」）。

| 方法与路径 | 作用 |
|---|---|
| `GET /api/health` | 状态与统计（表/行/页/字节/WAL/连接/查询数） |
| `GET /api/tables` | 表清单 |
| `GET /api/schema` | 全表结构（列类型、约束、索引） |
| `POST /api/sql` | `{"sql":"...","params":[...],"session":"..."}`，回 `X-Session-Id` 头 |
| `GET /api/tables/:name?limit&offset` | 分页取行（limit 默认 100，上限 5000） |
| `DELETE /api/tables/:name` | 删表 |
| `POST /api/tables/:name/alter` | 结构化改表（重建） |

会话：响应头 `X-Session-Id` 会话 id，后续请求带上它即可让 `BEGIN` 的事务跨请求延续。
同一 id 的并发请求在服务端串行，**等待上限 30 秒**（超时回 503 `SESSION_BUSY` + `Retry-After`）；
不同会话互不阻塞。空闲 30 分钟（事务中 120 秒）回收，最多同时保留 512 个会话。

**错误码**（响应体 `{"ok":false,"error":{"code":"..","message":".."}}`）：

| code | HTTP | 含义 |
|---|---|---|
| `SYNTAX_ERROR` | 400 | SQL 语法错误 |
| `TYPE_ERROR` | 400 | 类型不兼容 / 转换失败 |
| `NOT_FOUND` | 404 | 表、列、索引不存在 |
| `ALREADY_EXISTS` | 400 | 表/索引同名 |
| `CONSTRAINT_VIOLATION` | 400 | NOT NULL / UNIQUE / 主键冲突 |
| `WRITE_CONFLICT` | 409 | 页级写冲突（首提交者获胜），重试事务 |
| `DATABASE_LOCKED` | 400 | 会话已有事务 / 其它会话持有事务时执行 DDL |
| `LIMIT_EXCEEDED` | 400 | 超过单表页数、连接数或单语句行数上限 |
| `ROW_TOO_LARGE` | 400 | 单行超过 4060B |
| `AUTH_FAILED` | 401 | 令牌缺失或错误 |
| `PROTOCOL_ERROR` | 400 | 报文非法（含重复的关键头部、非法 Content-Length） |
| `LENGTH_REQUIRED` | 411 | 客户端用了 `Transfer-Encoding: chunked`，请改用 `Content-Length` |
| `RATE_LIMITED` | 429 | 认证失败次数过多，正在退避（响应带 `Retry-After`） |
| `SESSION_BUSY` | 503 | 同一 `X-Session-Id` 还有请求在执行，等待超时（带 `Retry-After`） |
| `INTERNAL_ERROR` | 400 | 内部错误（含待恢复状态） |

### JS 客户端 SDK

`sdk/littledb.js`：零依赖，浏览器（`window.LittleDB.createClient`）与 Node 18+ 通用。

```js
const { createClient } = require('./sdk/littledb.js');
const db = createClient('http://127.0.0.1:7802', { token: '...' });

await db.sql('CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT, price DECIMAL(10,2))');

const ins = await db.insert('t', [{ name: 'a', price: '1.50' }]);   // {inserted, ids, lastId}
const rows = await db.rows('SELECT id, name, price FROM t');        // [{id, name, price:'1.50'}]
const { types } = await db.query('SELECT id, price FROM t');        // ['INT','DECIMAL']

await db.upsert('t', [{ id: 1, name: 'a2' }], ['id'], ['name']);    // ON CONFLICT DO UPDATE
await db.transaction(async () => {                                  // 抛错自动 ROLLBACK
  await db.sql("INSERT INTO t (name) VALUES ('c')");
});
```

封装掉的东西：`X-Session-Id` 自动续传（事务跨请求）、错误码映射到
`LittleDBError#code / #status / #retryAfterMs`、`DECIMAL` 保持字符串、
`insert`/`upsert` 自动拼 `?` 参数、`transaction()` 失败自动回滚（可配 `{retries}` 重试写冲突）。

测试：`LDB_BASE=http://127.0.0.1:7802 LDB_TOKEN=xxx node sdk/test.js`（`tools/verify.ps1` 已包含这一项）。

## 限制一览

| 项 | 值 |
|---|---|
| 页大小 / 单行 | 4096B / 4060B |
| 单表 | 65535 页 ≈ 256MB |
| 单语句物化行数 | 500,000（超出报 `LIMIT_EXCEEDED`，请加 WHERE/LIMIT） |
| JOIN 组合行对 | 2,000,000 |
| 协议帧 / HTTP body | 16MB / 2MB |
| 并发连接 | 默认 64（协议与 HTTP 共用配额） |
| WAL 自动 checkpoint | 8MB；每轮最多 1024 页 |
| 表达式嵌套 / token 数 / JSON 深度 | 64 / 20000 / 200 |
| `LIKE` 回溯步数 | 5,000,000（超出报 `LIMIT_EXCEEDED`，防 O(n·m) 慢查询） |
| `ROUND` 小数位数 | -30 ..= 30 |

## 已知未做

- 语句级写并行（需要存储层内部可变性改造，见 `docs/mvcc-design.md` §3.9）。
- 复合索引支持**前导列**的等值与前缀/范围查找；后续列的条件只能作为候选行的 WHERE 复检，
  不能单独走索引（也不支持「第二列等值」直接定位）。
- `UPDATE`/`DELETE` 的 WHERE 能用单列索引定位候选行（等值/范围），但命中后仍逐行复检；
  无索引可用时退回全表扫描并受 50 万行上限保护。
- **没有结果流式/游标**：查询在服务端整体物化后一次性返回（LDBP 单帧上限 16MB，
  超限会回一个明确的错误帧提示加 `LIMIT`，不会静默丢弃让客户端干等）；
  真正流式需要把执行器从「全物化」改成「迭代器」。
- `EXPLAIN` 的「代价粗估」只按读取行数占比给量级，没有直方图/NDV 统计，不做代价模型。
- 视图、触发器、外键、分布式、TLS。
- 只有 JS 客户端（`sdk/littledb.js`）；Python / C# 等需按 HTTP 约定自行封装。
- 未内置 Windows 服务（SCM）宿主：`start/stop` 是分离进程 + pid 文件；
  开机自启请用任务计划程序或 NSSM（见「配置、日志与后台运行」）。
- `Transfer-Encoding: chunked` 请求体不支持（会显式返回 411，不会静默当空 body）。
