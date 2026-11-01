# LittleDB (Rust)

单机数据库引擎，从零实现：4KB slotted page 存储层 + LDBW3 预写日志（逐记录 CRC）+
快照隔离事务 + 自研 SQL 执行器，内置 LDBP/1 TCP 协议服务、HTTP API 和单文件 Web 控制台。
**运行时零第三方依赖**（只用 Rust 标准库）。

适合的场景：单机应用的数据持久化、小型服务的数据层、当作学习数据库内核的完整参考实现。

```text
❯ littledb.exe sql .\data "SELECT tag, COUNT(*) AS n FROM events GROUP BY tag ORDER BY n DESC"
  tag       | n
  ----------+------
  error     | 2
  warn      | 1
```

## 特性一览

| 方面 | 内容 |
|---|---|
| SQL | 增删改查、`UPSERT`、`AUTO_INCREMENT`、`ALTER TABLE`、事务（快照隔离）、`JOIN`、CTE、派生表、`EXISTS`/标量子查询、日期时间函数、JSON 函数、`TRUNCATE`、`VACUUM` |
| 存储 | 4KB slotted page，单表容量约 **16 TiB**；WAL 逐记录 CRC 校验，崩溃恢复不丢已确认提交 |
| 接入 | HTTP API + LDBP/1 TCP 协议 + 零依赖 JS / Python 客户端 SDK |
| 安全 | 令牌认证（常量时间比较）、默认只听本机、认证失败指数退避、CORS 默认同源 |
| 运维 | CLI（`sql/dump/load/verify/start/stop/status`）、配置文件、日志落盘+轮转、Web 控制台 |

## 从源码构建

前置：Rust stable（[rustup](https://rustup.rs)），无其它依赖。

```bash
git clone https://github.com/Layooth/LDB.git
cd LDB
cargo build --release
# 产物：target/release/littledb.exe（Linux/macOS 下同名无扩展名）
```

**启动**：建议把 `web/`（控制台前端）拷到 exe 旁边——服务优先在 **exe 同目录**找 `web\index.html`，
找不到再按相对路径找（源码树内运行也行）。没有 `web/` 时 API 照常工作，只是浏览器控制台不可用。

```bash
# 任意数据目录；协议 7801 + 控制台 7802，自动打开浏览器
./target/release/littledb.exe serve ./data

# 启用 Bearer 认证（对外监听时必须；详见「安全默认值」）
./target/release/littledb.exe serve ./data --token <一串随机字符>
```

CLI 速览：

```bash
littledb sql <数据目录> "SELECT ..."   # 执行 SQL（独占模式，无 HTTP 层）
littledb verify <数据目录> --deep      # 深度一致性校验（扫描 + 索引重建比对）
littledb dump  <数据目录> -o backup.sql # 导出自包含 SQL 脚本（备份/迁移/进版本控制）
littledb load  <新目录> backup.sql     # 从脚本导入
littledb start <数据目录>              # 后台运行（关终端不退出，日志落数据目录）
littledb status / stop <数据目录>      # 查看状态 / 停止后台实例
littledb demo  <临时目录>              # 建演示库并验证读写
```

## 快速接入（客户端）

完整约定见下文「客户端接入（各语言）」。两套零依赖 SDK 就在仓库里，拷进项目即用：

**Node.js / 浏览器（`sdk/littledb.js`）**

```js
const { createClient } = require('./littledb.js');
const db = createClient('http://127.0.0.1:7802', { token: 'dev-token' });

const a = await db.insert('tasks', [{ title: '写周报', price: '19.99' }]);
console.log(a.ids, a.lastId);                       // [1] 1 —— 自增 id 直接拿回

await db.rows('SELECT id, title, price FROM tasks'); // 行字典；DECIMAL 是字符串
await db.upsert('tasks', [{ id: 1, title: '改' }], ['id'], ['title']);
await db.transaction(async () => {                   // 抛错自动 ROLLBACK
  await db.sql('INSERT INTO tasks (title) VALUES (?)', ['b']);
});
```

**Python（`sdk/littledb.py`，纯标准库 3.8+）**

```python
from littledb import connect
db = connect("http://127.0.0.1:7802", token="dev-token")
db.rows("SELECT id, title FROM tasks")
```

## SQL 支持

| 类别 | 支持 |
|---|---|
| DDL | `CREATE TABLE`（列级 PK/NOT NULL/UNIQUE/DEFAULT/**AUTO_INCREMENT**、表级 PK、IF NOT EXISTS）、`DROP TABLE`、`CREATE/DROP [UNIQUE] INDEX`、`ALTER TABLE`（`ADD/DROP [COLUMN]`、`RENAME COLUMN a TO b`、`RENAME TO c`） |
| DML | `INSERT INTO t [(列)] VALUES ...` / `SELECT ...`、`INSERT ... ON CONFLICT [(列)] DO UPDATE SET ... / DO NOTHING`（UPSERT，可用 `EXCLUDED.列`）、`UPDATE ... SET ... [WHERE]`、`DELETE FROM ... [WHERE]`、`TRUNCATE [TABLE] t`、`VACUUM` |
| 查询 | `WHERE / GROUP BY / HAVING / ORDER BY（可用投影别名）/ LIMIT / OFFSET / DISTINCT / UNION [ALL]`、`INNER/LEFT/CROSS JOIN`、`IN (SELECT ...)`、**派生表**、**CTE**、**`EXISTS/NOT EXISTS`**（含相关子查询）、**标量子查询** |
| 事务 | `BEGIN / COMMIT / ROLLBACK`（快照隔离，写-写冲突返回 409 `WRITE_CONFLICT`） |
| 元命令 | `SHOW TABLES`、`DESCRIBE t`、`EXPLAIN <语句>` |
| 函数 | `COALESCE LENGTH UPPER LOWER ABS ROUND NOW CURDATE`；聚合 `COUNT SUM AVG MIN MAX`（含 DISTINCT）；日期时间 `DATE_ADD/DATE_SUB/DATEDIFF/YEAR/MONTH/DAY/HOUR/MINUTE/SECOND/TO_DATE/DATE_FORMAT`；JSON `JSON_EXTRACT/JSON_TYPE/JSON_VALID/JSON_ARRAY_LENGTH` |

**不支持**：`RIGHT/FULL JOIN`、`ANY/ALL`、窗口函数、视图/触发器/序列。

值得知道的语义细节（都有集成测试兜底）：

- **类型严格**：`WHERE id = '5'` 报 `TYPE_ERROR` 而不是静默空结果（`CAST` 显式转换）；
- **DECIMAL 是精确十进制**（i128 + 标度），线路层保持字符串，算钱零浮点误差；
- `DATE_ADD` 的 `month/year` 按日历推进并做月末裁剪（`01-31 + 1 month = 02-28`）；
- `ORDER BY` 可用投影别名，别名优先于同名源表列（与标准 SQL 一致）；
- `INSERT ... SELECT` 先物化再写入（自引用安全）；`TRUNCATE` 保留表结构与索引、自增归零、可回滚；
- `VACUUM` 影子重建全部表回收文件空间，列属性与索引完整保留；
- `EXPLAIN <语句>` 显示访问方式（索引查找/范围/全表扫描）、扫描行数与代价粗估。

## 数据安全

- **WAL（LDBW3）**：逐记录 IEEE CRC-32；落盘位翻转/半写在恢复期被识别为「损坏尾部」，
  损坏记录及其后内容不重放；旧版 LDBW2 日志兼容读取并自动升级。
- **恢复语义**：只有完整提交的事务被重放；`sync=full`（默认）每笔提交等 fsync 确认，
  `--sync normal` 提高吞吐但掉电可能丢最近提交（进程崩溃仍安全）。
- **一致性校验**：`verify --deep` 做全表扫描 + row_count 比对 + 索引重建逐键比对，
  能发现「索引指向不存在的行」「行存在但索引漏了」「唯一索引重复」等静默损坏。
- **catalog 版本闸门**：由更新版本引擎创建的数据目录会被拒绝打开（不以旧规则静默读写）。
- 强杀进程测试：`pwsh tools/crash-test.ps1 -Rounds 3`（写入中 `kill -9`，重启后校验已确认提交零丢失）。

## 配置 / 日志 / 后台运行

配置文件 `littledb.json`（查找顺序：`--config` → `<数据目录>/littledb.json` → `./littledb.json`）。
优先级：**命令行 > 环境变量 `LDB_TOKEN` > 配置文件 > 内置默认**；拼错的键打 WARN 不静默忽略。

```json
{
  "host": "127.0.0.1",
  "port": 7801,
  "httpPort": 7802,
  "token": "换成一串随机字符",
  "maxConnections": 64,
  "slowMs": 200,
  "sync": "full",
  "logFile": "littledb.log"
}
```

日志：`--log-file`（追加写，默认 8MB 轮转一份备份），时间戳 UTC；慢查询（`--slow-ms`）
记录 SQL 与耗时。

后台运行：

```bash
littledb start ./data    # 分离进程 + pid 文件 + 日志
littledb status ./data
littledb stop ./data     # 直接终止 = 模拟一次崩溃，已提交事务靠 WAL 重放，数据不丢
```

> 未内置 Windows SCM 服务宿主；开机自启用任务计划程序执行 `littledb start <目录>`，
> 或用 [NSSM](https://nssm.cc/) 包 `serve`。

### 安全默认值

| 项 | 默认行为 | 放开方式 |
|---|---|---|
| 监听地址 | `127.0.0.1`（仅本机） | `--host 0.0.0.0` |
| 无令牌 + 对外监听 | **拒绝启动** | 加 `--token` 或显式 `--allow-insecure` |
| 猜令牌 | 连续失败 5 次指数退避（上限 15 分钟），HTTP 回 429 | `--auth-fail-limit <n>`（0 = 关闭） |
| 跨源 | 仅同源 | `--cors-origin <列表>` 或 `*` |

## 备份 / 迁移

`dump` 产出**自包含 SQL 脚本**（建表语句含类型/标度/约束/索引，DECIMAL 走字符串字面量），
`load` 分批包事务导入。运行中直接拷 `data/` 目录不安全（页缓存与 WAL 可能不一致），
备份请用 `dump`：

```bash
littledb dump ./data -o backup.sql
littledb load ./data-restored backup.sql
```

## 客户端接入（各语言）

所有客户端走同一个 HTTP 端点，先记住这几条**语言无关**约定：

| 约定 | 说明 |
|---|---|
| 端点 | `POST /api/sql`，body：`{"sql": "...", "params": [...]}`（`?` 占位） |
| 认证 | 请求头 `Authorization: Bearer <token>`（未配 token 则不需要） |
| 会话 | 响应头 `X-Session-Id`，**后续请求原样带回**即可让事务跨请求延续 |
| select 结果 | `{type:"select", columns, types, rows, rowCount}`，`types` 与列一一对应 |
| insert 结果 | `{type:"insert", inserted, updated, skipped, ids, lastId}`（自增 id 直接拿） |
| 错误 | `{"ok":false,"error":{"code","message"}}`；`AUTH_FAILED`→401、`NOT_FOUND`→404、`WRITE_CONFLICT`→409、`RATE_LIMITED`→429、`SESSION_BUSY`→503；429/503 带 `Retry-After` |
| 值的形态 | `DECIMAL` 是**字符串**（不过浮点）；日期时间是文本 |

其它端点：`GET /api/health`（健康+统计+延迟分位数，免认证）、`GET /api/schema`、
`GET /api/tables/:name?limit&offset`（分页取行）、`DELETE /api/tables/:name`、
`POST /api/tables/:name/alter`。

**curl（事务 = 同一个 `X-Session-Id` 里 BEGIN ... COMMIT）：**

```bash
curl -s http://127.0.0.1:7802/api/sql -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sql":"INSERT INTO tasks (title) VALUES (?)","params":["hello"]}'
```

**Java / C# / Go / PHP**：任何能发 HTTP + JSON 的语言都能直接调；最小示例
（各 10 行左右，注意回传 `X-Session-Id` 做事务）见仓库内 README 历史版本或
照 `sdk/littledb.js` 封装（会话头 + 错误处理，不到一百行）。

接入核对表（最容易踩的几处）：

1. **DECIMAL 是字符串**——算钱别用浮点；
2. **事务必须带同一个 `X-Session-Id`**——不带则每条语句独立自动提交；
3. **重试类错误**（`WRITE_CONFLICT`/`RATE_LIMITED`/`SESSION_BUSY`）看 `Retry-After`；
4. **改表结构用 `ALTER TABLE`**——数据保留，应用无感知；
5. **大结果用 `LIMIT`**——结果整体物化，单帧上限 16MB。

## HTTP API 一览（默认 7802）

| 方法与路径 | 作用 |
|---|---|
| `GET /api/health` | 状态、统计（表/行/页/WAL）、最近 512 次查询的 **p50/p95/p99** 延迟、错误计数 |
| `GET /api/tables` / `GET /api/schema` | 表清单 / 全表结构（含 DECIMAL 标度与索引标记） |
| `POST /api/sql` | 执行 SQL（会话与事务见上） |
| `GET /api/tables/:name?limit&offset` | 分页取行（limit 默认 100，上限 5000） |
| `DELETE /api/tables/:name` | 删表 |
| `POST /api/tables/:name/alter` | 结构化改表 |

`GET /api/health` 响应示例：

```json
{ "status":"ok", "stats":{"tables":1,"rows":3,"pages":1,"walBytes":4462},
  "latency":{"p50Ms":0.15,"p95Ms":0.25,"p99Ms":0.31,"samples":128},
  "errors":0, "queries":128 }
```

LDBP/1 TCP 协议（默认 7801）：`uint32 长度 + JSON 报文`，`hello` 认证 / `query` 执行 /
`ping` 保活，语义与 HTTP 一致（单帧上限 16MB，空闲 5 分钟断开）。

## 性能

`tools/bench.py` 按真实部署形态（HTTP + 认证 + 参数化）测六类场景
（逐条插入 / 批量插入 / 主键点查 / 索引范围扫 / 单行更新 / 混合并发）：

```bash
./target/release/littledb.exe serve ./bench-data --http-port 7802 --no-open --token bench
LDB_BASE=http://127.0.0.1:7802 LDB_TOKEN=bench python tools/bench.py
```

参考基线（Windows，release 构建，2 万行表，SDK keep-alive 连接）：

| 场景 | sync=full（默认，每笔 fsync） | sync=normal |
|---|---|---|
| 逐条插入 | 500 ops/s | 3,505 ops/s |
| 批量插入（500 行/批） | **94,970 行/s** | 140,845 行/s |
| 主键点查 | **6,098 ops/s**（p50=0.15ms，p99=0.28ms） | 6,554 ops/s |
| 索引范围扫 | 2,838 ops/s（p50=0.33ms） | 2,787 ops/s |
| 单行更新 | 487 ops/s | 3,099 ops/s |
| 混合并发（4 会话） | 977 ops/s | 2,900 ops/s |

解读：读路径快且无长尾（p99 < 0.3ms）；写入由 fsync 主导——`sync=full` 的逐条写入
就是「每笔持久化」的代价，要吞吐用批量插入或 `--sync normal`（掉电可能丢最近提交，
进程崩溃仍安全）。`tools/diag_latency.py` 可复现「连接复用 vs 每请求新建连接」的
20 倍差距，怀疑性能问题时先跑它分层。

## 从源码开发

```bash
cargo test                    # 167 项测试（单元 + 集成：崩溃恢复/并发/索引/HTTP/故障注入）
cargo build --release
pwsh tools/verify.ps1         # 一键门禁：测试 → 构建 → 部署 → 13 项冒烟 → SDK 集成测试
pwsh tools/crash-test.ps1 -Rounds 3   # 写入中强杀进程，校验已确认提交零丢失
cargo test --release -- --ignored row_cap   # 50 万行边界用例（慢，默认忽略）
```

`tools/verify.ps1` 会把构建产物部署到**仓库旁边**的 `dist\` 目录（构建产物与源码分层的
本地开发布局；CI/纯源码使用者可以忽略这一步，直接用 `target/release/` 下的产物）。

### 目录结构

```text
.
├── src/                     服务端源码
│   ├── main.rs              CLI 入口（serve/sql/dump/load/verify/start/stop/status）
│   ├── server.rs            LDBP/1 TCP 服务（线程/连接管理）
│   ├── http.rs              HTTP API + 静态控制台 + 会话表
│   ├── protocol.rs          LDBP/1 帧 + 常量时间令牌比较
│   ├── auth.rs              认证失败限速（指数退避）
│   ├── config.rs            littledb.json 解析
│   ├── log.rs               日志（stdout / 文件 + 轮转）
│   ├── engine.rs            引擎门面（表生命周期、行读写、索引查找）
│   ├── table.rs             单表：页分配、行增删改、索引维护
│   ├── page.rs              页格式：槽、记录、碎片整理
│   ├── store.rs             存储层：WAL 提交、事务、checkpoint、页文件
│   ├── wal.rs               预写日志编解码（LDBW3，逐记录 CRC）
│   ├── catalog.rs           表结构元数据（catalog.json，原子写）
│   ├── codec.rs             值编解码 / 类型转换 / 日期算术
│   ├── json.rs / jsonpath.rs 自带 JSON 解析器 / 路径取值
│   ├── dump.rs              dump / load（SQL 脚本导出导入）
│   └── sql/                 SQL 栈：lexer → parser → ast → eval → executor
├── tests/storage.rs         集成测试（59 个用例：事务/崩溃/并发/HTTP/UPSERT/JSON/日期/VACUUM）
├── web/index.html           Web 控制台前端（单文件）
├── sdk/                     JS / Python 客户端 SDK + 集成测试 + 接入示例
├── launcher/                启动器：launcher.cs（轻量 C# 启动器）
│   └── desktop/             桌面版主界面（WinForms + WebView2 内嵌控制台，dotnet build）
├── assets/                  图标资源
├── tools/                   verify.ps1（一键门禁）/ crash-test.ps1 / bench.py / diag_latency.py
└── docs/                    设计文档（mvcc-design.md 等）
```

### 桌面版主界面（可选）

`launcher/desktop/` 是 WinForms + WebView2 的桌面壳：窗口内加载服务端自带的 Web 控制台
（与浏览器端同一前端、同一服务、同一数据，天然同步），负责服务生命周期、托盘、
令牌管理（走 `LDB_TOKEN` 环境变量）。依赖 .NET 8 桌面运行时 + WebView2 运行时。

```bash
cd launcher/desktop
dotnet publish -c Release
# 产物：bin/Release/net8.0-windows/win-x64/publish/LittleDB桌面版.exe
# 把它放到 littledb.exe 与 web/ 旁边即可使用
```

## 限制一览

| 项 | 值 |
|---|---|
| 页大小 / 单行 | 4096B / 约 4060B（无溢出页，更大值请应用层分片） |
| 单表容量 | 约 16 TiB（u32 页号 × 4KB 页） |
| 单语句物化行数 | 500,000（超出报 `LIMIT_EXCEEDED`，请加 WHERE/LIMIT） |
| JOIN 组合行对 | 2,000,000 |
| 协议帧 / HTTP body | 16MB / 2MB |
| 并发连接 | 默认 64 |
| WAL 自动 checkpoint | 8MB；每轮最多 1024 页 |
| 表达式嵌套 / token 数 / JSON 深度 | 64 / 20000 / 200 |
| `LIKE` 回溯步数 | 5,000,000（防 O(n·m) 慢查询） |

## 已知未做

- 语句级写并行：写语句整条持引擎写锁（写串行化，多写收益在事务级交错），
  原因与方向见 `docs/mvcc-design.md` §3.9；
- **无结果流式/游标**：查询整体物化后一次返回（超单帧上限报明确错误，不会静默截断）；
- 复合索引仅前导列可走索引（等值/前缀/范围），后续列靠 WHERE 复检；
- 索引为全内存 `BTreeMap`，启动时全表扫描重建（大表启动变慢）；
- `RIGHT/FULL JOIN`、`ANY/ALL`、窗口函数、视图/触发器/序列；
- `EXISTS`/标量子查询按行执行相关子查询，代价随外层行数线性增长。
