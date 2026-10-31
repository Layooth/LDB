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
# 双击 dist\LittleDB控制台.exe：自动生成访问令牌、启动服务、打开控制台
# 或者手动（先 cd 到 dist）：
.\littledb.exe serve .\data               # 协议 7801 + 控制台 7802，并自动开浏览器
.\littledb.exe serve .\data --token secret  # 启用 Bearer 认证
.\littledb.exe sql .\data "SELECT * FROM t LIMIT 5"
.\littledb.exe verify .\data              # 输出数据目录概览（表/列/索引/行数/恢复情况）
.\littledb.exe demo .\tmp-demo            # 建演示库并验证读写
.\littledb.exe dump .\data -o backup.sql  # 导出为 SQL 脚本（可进版本控制/能改）
.\littledb.exe load .\new-data backup.sql # 从脚本导入（--batch 200 控制每批事务）
```

### 目录结构

```
littledb-rust\
├── dist\                          ← 运行层：拿这一目录就能在任何机器上跑
│   ├── littledb.exe               服务本体（verify.ps1 自动部署到这里）
│   ├── LittleDB控制台.exe          图形启动器（token 走环境变量传给服务）
│   ├── .ldb-token                 启动器生成的令牌文件
│   ├── web\                       控制台前端（单文件 index.html）
│   └── data\                      数据目录（catalog.json + t<N>.tbl + *.wal）
│
└── littledb-rust-src\             ← 源码仓库（git）
    ├── src\                       服务端源码（main/server/http/engine/table/store/wal/sql/...）
    ├── web\index.html             控制台前端源码（部署时拷到 dist\web\）
    ├── launcher\launcher.cs       图形启动器源码
    ├── assets\                    图标资源
    ├── sdk\                       JS / Python 客户端 SDK 与其集成测试、接入示例
    ├── tests\storage.rs           集成测试
    ├── tools\verify.ps1           一键门禁（测试→构建→部署到 dist→冒烟）
    ├── tools\crash-test.ps1       强杀恢复验证
    └── docs\                      设计文档
```

控制台前端改完要同步：`Copy-Item littledb-rust-src\web\index.html dist\web\`。
启动器重新编译：`csc /nologo /optimize /out:dist\LittleDB控制台.exe littledb-rust-src\launcher\launcher.cs`。

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
| DML | `INSERT INTO t [(列)] VALUES (...),(...)`、`INSERT INTO t [(列)] SELECT ...`、`INSERT ... ON CONFLICT [(列)] DO UPDATE SET ... / DO NOTHING`（UPSERT，可用 `EXCLUDED.列` 引用待插入行）、`UPDATE ... SET ... [WHERE]`、`DELETE FROM ... [WHERE]`、`TRUNCATE [TABLE] t` |
| 查询 | `WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`/`UNION [ALL]`、`INNER/LEFT/CROSS JOIN`（左深树，嵌套循环）、`IN (SELECT ...)`（非关联单列）、**派生表** `FROM (SELECT ...) AS t`、**CTE** `WITH x AS (...) SELECT ... FROM x`、**`EXISTS` / `NOT EXISTS`**（支持相关子查询）、**标量子查询** `(SELECT ...)` |
| 事务 | `BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` |
| 元命令 | `SHOW TABLES`、`DESCRIBE t`、`EXPLAIN <SELECT/INSERT/UPDATE/DELETE>` |
| 表达式 | 算术、比较、`AND/OR/NOT`、`IS [NOT] NULL`、`IN (...)`、`BETWEEN`、`LIKE`、`CAST(x AS 类型)`、`?` 参数 |
| 函数 | `COALESCE LENGTH UPPER LOWER ABS ROUND`、`NOW CURDATE`；聚合 `COUNT SUM AVG MIN MAX`（含 DISTINCT / `COUNT(*)`） |
| 日期时间 | `DATE_ADD(d, n, unit)` / `DATE_SUB(d, n, unit)`（unit: `day/week/hour/minute/second/month/year`）、`DATEDIFF(a, b)`、`YEAR MONTH DAY HOUR MINUTE SECOND`、`TO_DATE(d)`、`DATE_FORMAT(d, '%Y-%m-%d %H:%M:%S')` |
| JSON | `JSON_EXTRACT(j, '$.a.b[0]')`、`JSON_TYPE(j)`、`JSON_VALID(j)`、`JSON_ARRAY_LENGTH(j)` |

**不支持**：`RIGHT/FULL JOIN`、`ANY/ALL`、窗口函数、视图/触发器/序列。
标量子查询 `(SELECT ...)` 必须返回**一行一列**：无行 → `NULL`，多行/多列 → 明确报错
（提示加 `LIMIT 1` 或改用聚合）；可用在 `WHERE`、`JOIN ... ON`、`SELECT` 列表与
`UPDATE/DELETE` 的 `WHERE` 里，支持相关子查询（外层行作为内层上下文）。
`ALTER TABLE` 的列变更通过「影子表重建」实现：**表 id 会变、行号会重排、数据完整保留**，
索引按列名迁移。

`INSERT ... SELECT` 先**物化**子查询再逐行插入（因此 `INSERT INTO t SELECT ... FROM t`
是安全的：不会边读边写），受 50 万行上限保护；不支持与 `ON CONFLICT` 组合（会明确报错）。
`TRUNCATE` 逐行删除而不是重建文件：**保留表结构与索引定义**、自增游标归零，
并且是可回滚的（`BEGIN; TRUNCATE ...; ROLLBACK` 数据会回来）。

JSON 列在存储层就是 TEXT，`JSON_*` 在求值期解析：路径语法 `$` / `$.键` / `[下标]`
（可混写 `$.a.b[0]`），**路径不存在返回 NULL**（不是错误，方便做「有这个字段吗」）；
标量原样返回（2^53 以上的整数仍是 `INT`，不经过 f64），对象/数组返回 JSON 文本；
非法 JSON 报 `TYPE_ERROR` 并提示先用 `JSON_VALID` 检查，路径语法错报 `SYNTAX_ERROR`。

日期时间统一按 **UTC** 的 `YYYY-MM-DD[ HH:MM:SS]` 解析（`NOW()` 返回 UTC）；`DATE_ADD`/`DATE_SUB`
的 `month`/`year` 按日历推进并对月末做裁剪（`2026-01-31 + 1 month = 2026-02-28`，
闰年 `2024-01-31 + 1 month = 2024-02-29`），结果形态跟随输入（只有日期就只返回日期）。

`ORDER BY` 可以直接用**投影别名**（`SELECT TO_DATE(at) AS d ... ORDER BY d`），
别名优先于同名源表列（与标准 SQL / MySQL 一致）；限定名（`ORDER BY t.v`）与
表达式的结果排序都不受影响。

`EXISTS` 只在 **WHERE / JOIN ... ON** 里求值：执行时把子查询按当前行跑一遍（相关子查询靠
`RowContext` 的 parent 链解析外层列），结果换成布尔字面量，所以它在 `SELECT` 列表、
`ORDER BY`、`HAVING` 里会明确报错。每行一次子查询，代价随外层行数线性增长。

派生表与 CTE 都是**整体物化**（先跑子查询把行读进内存，再当临时表用），没有索引下推；
子查询结果超过 50 万行直接报 `LIMIT_EXCEEDED`。CTE 只在**当前语句**内可见，
按顺序展开（后一个可引用前一个），不支持 `WITH RECURSIVE`，也不支持
`WHERE ... IN (SELECT ... FROM cte)` 这类内层引用。

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

## 客户端接入（各语言）

所有客户端都走同一个 HTTP 端点。先记住这几条**语言无关**的约定，后面每种语言都一样：

| 约定 | 说明 |
|---|---|
| 端点 | `POST /api/sql`，body：`{"sql": "...", "params": [...]}`（`?` 占位） |
| 认证 | 请求头 `Authorization: Bearer <token>`（未配 token 则不需要） |
| 会话 | 响应头 `X-Session-Id`，**后续请求原样带回**即可让 `BEGIN` 的事务跨请求延续 |
| 响应 | 成功 `{"ok":true,"result":{...}}`；失败 `{"ok":false,"error":{"code","message"}}` |
| select 结果 | `{type:"select", columns, types, rows, rowCount}`，`types` 与列一一对应 |
| insert 结果 | `{type:"insert", inserted, updated, skipped, ids, lastId}`（自增 id 直接拿） |
| 错误 → 状态码 | `AUTH_FAILED`→401、`NOT_FOUND`→404、`WRITE_CONFLICT`→409、`RATE_LIMITED`→429、`SESSION_BUSY`→503、其余 400；429/503 带 `Retry-After` |
| 值的形态 | `DECIMAL` 是**字符串**（不过浮点）；`INT/REAL/BOOL` 是 JSON 数值/布尔；日期时间是文本 |

其它 REST 端点：`GET /api/health`（健康，免认证）、`GET /api/schema`（全表结构，含 DECIMAL 标度）、
`GET /api/tables`、`GET /api/tables/<名>?limit&offset`（分页取行）、`DELETE /api/tables/<名>`（删表）。

### Node.js / 浏览器（SDK）

`sdk/littledb.js`：零依赖，Node 18+ 与浏览器（`window.LittleDB.createClient`）通用。
完整可运行的示例见 `sdk/demo-app.js`。

```js
const { createClient } = require('./littledb.js');
const db = createClient('http://127.0.0.1:7802', { token: 'dev-token' });

// 建表（启动时跑一次，IF NOT EXISTS 幂等）
await db.sql(`CREATE TABLE IF NOT EXISTS tasks (
    id INT PRIMARY KEY AUTO_INCREMENT, title TEXT NOT NULL,
    price DECIMAL(10,2), status TEXT DEFAULT 'todo')`);

// 插入 → 直接拿回自增 id
const a = await db.insert('tasks', [{ title: '写周报', price: '19.99' }]);
// → { inserted: 1, ids: [1], lastId: 1 }

// 查询 → 行字典；要列类型用 query()
await db.rows('SELECT id, title, price FROM tasks ORDER BY id');
(await db.query('SELECT id, price FROM tasks')).types;   // ['INT', 'DECIMAL']

// UPSERT：按主键有则更新（只改指定列）
await db.upsert('tasks', [{ id: 1, title: '改' }], ['id'], ['title']);

// 事务：抛错自动 ROLLBACK（回调里必须用同一个 db）
await db.transaction(async () => {
  await db.sql('INSERT INTO tasks (title) VALUES (?)', ['b']);
});

// 分页 / 元数据 / 删表
const page = await db.table('tasks', { limit: 20, offset: 0 });  // total/objects/schema
await db.schema();  await db.dropTable('tasks');
```

SDK 封装掉的：会话头自动续传、错误 → `LittleDBError#code/#status/#retryAfterMs`、
`insert/upsert` 自动拼参数、事务失败回滚（`{retries}` 可重试写冲突）。
测试：`node sdk/test.js`（需 `LDB_BASE`/`LDB_TOKEN` 环境变量）。

### Python（SDK）

`sdk/littledb.py`：只用标准库（3.8+），能力与 JS 版对齐。

```python
from littledb import connect
import datetime, decimal

db = connect("http://127.0.0.1:7802", token="dev-token")

db.sql("CREATE TABLE IF NOT EXISTS tasks (id INT PRIMARY KEY AUTO_INCREMENT,"
       " title TEXT NOT NULL, price DECIMAL(10,2), at DATETIME)")

ins = db.insert("tasks", [{"title": "写周报",
                           "price": decimal.Decimal("19.99"),
                           "at": datetime.datetime(2026, 11, 1, 9, 0, 0)}])
print(ins.ids, ins.last_id)            # [1] 1

rows = db.rows("SELECT id, title, price FROM tasks")   # DECIMAL 是字符串
print(db.query("SELECT id, price FROM tasks").types)   # ['INT', 'DECIMAL']

db.upsert("tasks", [{"id": 1, "title": "改"}], ["id"], ["title"])

with db.transaction():                  # 抛异常自动 ROLLBACK
    db.sql("INSERT INTO tasks (title) VALUES (?)", ["b"])

db.transaction(lambda c: c.sql("INSERT INTO tasks (title) VALUES ('c')"), retries=2)
```

`datetime`/`date`/`Decimal` 参数自动转成服务端接受的文本；错误抛
`LittleDBError`（`.code` / `.status` / `.retry_after` / `.retryable`）。
测试：`python sdk/test_littledb.py`（同样读 `LDB_BASE`/`LDB_TOKEN`）。

### 原生 HTTP：curl / PowerShell

不想带 SDK（或写脚本、连 CI）时直接打端点：

```bash
# 执行 SQL
curl -s http://127.0.0.1:7802/api/sql \
  -H "Authorization: Bearer dev-token" \
  -H "Content-Type: application/json" \
  -d '{"sql":"INSERT INTO tasks (title) VALUES (?)","params":["curl 插入"]}'
# → {"ok":true,"result":{"type":"insert","inserted":1,"ids":[4],"lastId":4,...}}

# 事务 = 同一个 X-Session-Id 里 BEGIN ... COMMIT
SID=$(curl -s -D - -o /dev/null http://127.0.0.1:7802/api/health | grep -i x-session-id | tr -d '\r' | cut -d' ' -f2)
curl -s http://127.0.0.1:7802/api/sql -H "Authorization: Bearer dev-token" -H "X-Session-Id: $SID" \
  -d '{"sql":"BEGIN"}'
curl -s http://127.0.0.1:7802/api/sql -H "Authorization: Bearer dev-token" -H "X-Session-Id: $SID" \
  -d '{"sql":"INSERT INTO tasks (title) VALUES ('\''tx'\'')"}'
curl -s http://127.0.0.1:7802/api/sql -H "Authorization: Bearer dev-token" -H "X-Session-Id: $SID" \
  -d '{"sql":"COMMIT"}'
```

```powershell
$h = @{ Authorization = "Bearer dev-token" }
$r = Invoke-RestMethod -Uri "http://127.0.0.1:7802/api/sql" -Method Post `
     -Headers $h -ContentType "application/json" `
     -Body '{"sql":"SELECT id, price FROM tasks ORDER BY id"}'
$r.result.rows          # [[1, '19.99'], ...]
$r.result.types         # ['INT', 'DECIMAL']
```

### 其它语言：照 HTTP 约定直接调

任何能发 HTTP + JSON 的语言都能用；下面是最小示例（生产使用建议照
`sdk/littledb.js` 的做法把会话头与错误处理封一层，代码不到一百行）：

**Java（11+ HttpClient）**

```java
HttpClient http = HttpClient.newHttpClient();
var request = HttpRequest.newBuilder(URI.create("http://127.0.0.1:7802/api/sql"))
    .header("Authorization", "Bearer dev-token")
    .header("Content-Type", "application/json")
    .POST(HttpRequest.BodyPublishers.ofString(
        "{\"sql\":\"SELECT id, price FROM tasks\",\"params\":[]}"))
    .build();
var response = http.send(request, HttpResponse.BodyHandlers.ofString());
// response.headers().firstValue("X-Session-Id") → 事务要回传的会话 id
// response.body() → {"ok":true,"result":{...}}
```

**C#（HttpClient）**

```csharp
using var http = new HttpClient();
http.DefaultRequestHeaders.Add("Authorization", "Bearer dev-token");
var content = new StringContent(
    "{\"sql\":\"SELECT id, price FROM tasks\",\"params\":[]}",
    Encoding.UTF8, "application/json");
var response = await http.PostAsync("http://127.0.0.1:7802/api/sql", content);
if (response.Headers.TryGetValues("X-Session-Id", out var sid)) { /* 存起来 */ }
var body = await response.Content.ReadAsStringAsync();   // {"ok":true,"result":{...}}
```

**Go（net/http）**

```go
req, _ := http.NewRequest("POST", "http://127.0.0.1:7802/api/sql",
    strings.NewReader(`{"sql":"SELECT 1 AS one","params":[]}`))
req.Header.Set("Authorization", "Bearer dev-token")
req.Header.Set("Content-Type", "application/json")
resp, err := http.DefaultClient.Do(req)
// sid := resp.Header.Get("X-Session-Id")
// json.Unmarshal(body, &payload)  → payload["result"]
```

**PHP（curl 扩展）**

```php
$ch = curl_init('http://127.0.0.1:7802/api/sql');
curl_setopt_array($ch, [
    CURLOPT_POST => true,
    CURLOPT_HTTPHEADER => ['Authorization: Bearer dev-token', 'Content-Type: application/json'],
    CURLOPT_POSTFIELDS => '{"sql":"SELECT 1 AS one","params":[]}',
    CURLOPT_RETURNTRANSFER => true,
    CURLOPT_HEADERFUNCTION => function ($ch, $line) {   // 记下 X-Session-Id
        if (stripos($line, 'x-session-id:') === 0) { $GLOBALS['sid'] = trim(explode(':', $line)[1]); }
        return strlen($line);
    },
]);
$payload = json_decode(curl_exec($ch), true);   // $payload['result']
```

### 接入核对表（最容易踩的几处）

1. **DECIMAL 是字符串**：算钱别用浮点，客户端直接按字符串展示或用 `Decimal` 类型；
2. **事务必须带同一个 `X-Session-Id`**：不带的话每条语句是独立自动提交；
3. **错误先看 `error.code` 再看 HTTP 状态**：`WRITE_CONFLICT`/`RATE_LIMITED`/`SESSION_BUSY`
   都是「等会儿重试」，响应里有 `Retry-After`；
4. **改表结构用 `ALTER TABLE`**：列变更走影子表重建，数据保留但表 id 会变（应用无感知）；
5. **大结果用 `LIMIT`/分页**：结果整体物化，单帧上限 16MB。

## 性能测试

`tools/bench.py` 按真实部署形态（HTTP + 认证 + 参数化）测六类场景：

```powershell
# 起一个服务（建议用 release 版 dist\littledb.exe）
.\dist\littledb.exe serve .\bench-data --http-port 7802 --no-open --token bench

# 跑基准（约 30 秒，规模可用 BENCH_* 环境变量调）
$env:LDB_BASE = "http://127.0.0.1:7802"; $env:LDB_TOKEN = "bench"
python tools\bench.py
```

本机基线（Windows，release 版，2 万行表，走 HTTP）：

| 场景 | sync=full（默认） | sync=normal |
|---|---|---|
| 逐条插入（每行一次提交） | 102 ops/s | 279 ops/s |
| 批量插入（500 行/批） | 27,920 行/s | 36,792 行/s |
| 主键点查 | 309 ops/s（p50=0.6ms） | 246 ops/s（p50=0.6ms） |
| 索引范围扫 | 238 ops/s（p50=0.8ms） | 182 ops/s（p50=0.9ms） |
| 单行更新 | 108 ops/s | 202 ops/s |
| 混合并发（4 会话 插入+查询） | 454 ops/s | 1,258 ops/s |

解读：

- **写入路径基本由 fsync 主导**：`sync=full` 每笔提交刷盘，逐条插入/更新 ≈100 ops/s
  是「最坏情况下的持久写」；要吞吐就用批量插入（**27,900 行/s**）或 `--sync normal`
  （掉电可能丢最近提交，进程崩溃仍安全）。
- **读路径快且稳**：点查 p50 ≈ 0.6ms。
- **已知尾巴**：读延迟 p95 ≈ 15ms（两种 sync 模式都有）——与 fsync 无关，怀疑是后台
  checkpoint 周期性持写锁所致，尚未深挖；对吞吐无影响，介意尾延迟可作为第一个调查项。
- 批量写入会把 WAL 撑大，checkpoint 后回落；持续写入场景关注 `data\` 目录体积。
- HTTP 开销已计入这些数字；想测引擎极限可用 CLI（`littledb.exe sql`，独占模式，无 HTTP 层）。

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
