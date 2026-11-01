//! LittleDB (Rust) 命令行入口。
//!
//!   version                 版本
//!   sql <目录> <SQL>        执行一条 SQL 并输出结果
//!   demo <目录>             建演示库并验证读写
//!   verify <目录>           打开已有数据目录并输出概览

use std::path::Path;
use std::process::exit;

use littledb::server::{Server, ServerOptions};
use littledb::sql::{QueryResult, Session};
use littledb::{Engine, Result, Value};

const BANNER: &str = "LittleDB (Rust)";

fn print_help() {
    println!("{} {}", BANNER, env!("CARGO_PKG_VERSION"));
    println!();
    println!("用法:");
    println!("  littledb                      直接启动服务（自动找 ./data 并打开浏览器）");
    println!("  littledb version              显示版本");
    println!("  littledb serve <目录>         启动 LDBP/1 协议服务");
    println!("  littledb sql <目录> <SQL>     执行 SQL");
    println!("  littledb demo <目录>          生成演示库并验证读写");
    println!("  littledb verify <目录> [--deep]  概览 / 深度一致性校验（--deep 扫描+索引比对）");
    println!("  littledb dump <目录> [-o 文件] 导出为 SQL 脚本（默认输出到 stdout）");
    println!("  littledb load <目录> <脚本>   导入 SQL 脚本（--batch n 控制每批事务大小）");
    println!("  littledb start <目录>         后台启动（关终端不退出，日志写数据目录）");
    println!("  littledb stop <目录>          停止后台实例");
    println!("  littledb status <目录>        查看后台实例状态与日志位置");
    println!("  littledb help                 显示帮助");
    println!();
    println!("serve 选项:");
    println!("  --host <地址>        监听地址（默认 127.0.0.1）");
    println!("  -p, --port <端口>    协议端口（默认 7801）");
    println!("  --http-port <端口>   Web 控制台端口（默认 7802）");
    println!("  --no-http            只启动协议服务，不启动 Web 控制台");
    println!("  --no-open            启动后不自动打开浏览器");
    println!("  --web-dir <目录>     指定 Web 控制台静态文件目录");
    println!("  --token <令牌>       启用认证，客户端需携带相同 token（也可用环境变量 LDB_TOKEN）");
    println!("  --cors-origin <列表> Web 端可跨源访问的来源，逗号分隔；* 表示不限制（默认同源）");
    println!("  --allow-insecure     允许「监听非回环地址且没有 token」——仅调试用，默认拒绝启动");
    println!("  --auth-fail-limit <n> 同一来源连续认证失败多少次后开始退避（默认 5，0 = 关闭）");
    println!("  --max-connections <n> 最大并发连接数（默认 64）");
    println!("  --slow-ms <毫秒>     超过该耗时打印慢查询");
    println!("  --sync <full|normal> 提交耐久性（默认 full）：normal 在 checkpoint 时才 fsync，");
    println!("                       进程崩溃仍安全，但掉电可能丢失最近的提交");
    println!("  --config <文件>      配置文件（默认找 <数据目录>/littledb.json 或 ./littledb.json）");
    println!("  --log-file <文件>    日志写文件（相对路径按数据目录解析；默认 stdout）");
    println!("  --log-max-bytes <n>  单个日志文件上限（默认 8MB，超出轮转到 .1）");
    println!("  --verbose            输出每条查询耗时");
    println!();
    println!("示例:");
    println!("  littledb serve ./data");
    println!("  littledb serve ./data --port 7801 --token secret");
    println!("  littledb sql ./data \"SHOW TABLES\"");
}

fn main() {
    // SQL/JSON 解析和表达式求值都是递归的：给主逻辑一条 8MB 栈的线程，
    // 避免极端输入在默认 1MB 的主线程栈上直接爆栈
    let worker = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(real_main)
        .expect("无法创建主线程");
    if worker.join().is_err() {
        eprintln!("错误: 发生内部崩溃");
        std::process::exit(1);
    }
}

fn real_main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("");

    let result = match command {
        // 不带参数：直接启动服务（双击 exe 的场景）
        "" => {
            let dir = detect_data_dir();
            run_serve(&dir, &[])
        }
        "version" | "--version" | "-v" => {
            println!("{} {}", BANNER, env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "sql" => {
            if args.len() < 4 {
                eprintln!("用法: littledb sql <目录> <SQL>");
                exit(1);
            }
            let dir = args[2].clone();
            let sql = args[3..].join(" ");
            run_sql(&dir, &sql)
        }
        "serve" => {
            if args.len() < 3 {
                eprintln!(
                    "用法: littledb serve <目录> [--host 127.0.0.1] [--port 7801] [--token 令牌] \
                     [--max-connections 64] [--slow-ms 10] [--verbose]"
                );
                exit(1);
            }
            let dir = args[2].clone();
            run_serve(&dir, &args[3..])
        }
        "demo" => match args.get(2) {
            Some(dir) => run_demo(dir),
            None => {
                eprintln!("用法: littledb demo <目录>");
                exit(1);
            }
        },
        "verify" => match args.get(2) {
            Some(dir) => run_verify(dir, args.iter().any(|a| a == "--deep")),
            None => {
                eprintln!("用法: littledb verify <目录> [--deep]");
                exit(1);
            }
        },
        "start" => match args.get(2) {
            Some(dir) => run_start(dir, &args[3..]),
            None => {
                eprintln!("用法: littledb start <目录> [serve 选项]");
                exit(1);
            }
        },
        "stop" => match args.get(2) {
            Some(dir) => run_stop(dir),
            None => {
                eprintln!("用法: littledb stop <目录>");
                exit(1);
            }
        },
        "status" => match args.get(2) {
            Some(dir) => run_status(dir),
            None => {
                eprintln!("用法: littledb status <目录>");
                exit(1);
            }
        },
        "dump" => match args.get(2) {
            Some(dir) => {
                let output = args
                    .iter()
                    .position(|arg| arg == "-o" || arg == "--out")
                    .and_then(|pos| args.get(pos + 1).cloned());
                run_dump(dir, output.as_deref())
            }
            None => {
                eprintln!("用法: littledb dump <目录> [-o 输出文件]");
                exit(1);
            }
        },
        "load" => match (args.get(2), args.get(3)) {
            (Some(dir), Some(file)) => {
                let batch = args
                    .iter()
                    .position(|arg| arg == "--batch")
                    .and_then(|pos| args.get(pos + 1))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(200);
                run_load(dir, file, batch)
            }
            _ => {
                eprintln!("用法: littledb load <目录> <脚本文件> [--batch 200]");
                exit(1);
            }
        },
        other => {
            eprintln!("未知命令: {}", other);
            print_help();
            exit(1);
        }
    };

    if let Err(err) = result {
        eprintln!("错误: {}", err);
        exit(1);
    }
}

fn run_sql(dir: &str, sql: &str) -> Result<()> {
    let mut session = Session::open(Path::new(dir))?;
    let outcome = session.execute(sql, &[]);
    session.close()?;
    let result = outcome?;
    render(&result);
    Ok(())
}

fn render(result: &QueryResult) {
    match result.to_grid() {
        Some((columns, rows)) => {
            print_table(&columns, &rows);
            println!("{} 行", rows.len());
        }
        None => println!("{}", result.summary()),
    }
}

fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    let mut widths: Vec<usize> = columns.iter().map(|c| display_width(c)).collect();
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut line = Vec::with_capacity(row.len());
        for (i, value) in row.iter().enumerate() {
            let text = display_text(value);
            let width = display_width(&text);
            if i < widths.len() && width > widths[i] {
                widths[i] = width;
            }
            line.push(text);
        }
        cells.push(line);
    }

    let mut header = String::from("|");
    for (i, column) in columns.iter().enumerate() {
        header.push(' ');
        header.push_str(column);
        header.push_str(&" ".repeat(widths[i].saturating_sub(display_width(column))));
        header.push_str(" |");
    }
    println!("{}", header);

    let mut divider = String::from("|");
    for width in &widths {
        divider.push_str(&"-".repeat(width + 2));
        divider.push('|');
    }
    println!("{}", divider);

    for line in &cells {
        let mut text = String::from("|");
        for (i, cell) in line.iter().enumerate() {
            text.push(' ');
            text.push_str(cell);
            text.push_str(&" ".repeat(widths[i].saturating_sub(display_width(cell))));
            text.push_str(" |");
        }
        println!("{}", text);
    }
}

fn display_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        other => other.as_text(),
    }
}

/// 中文按两列宽计算，保证终端对齐
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| {
            let code = ch as u32;
            if code < 0x1100 {
                1
            } else if code <= 0x115f
                || (0x2e80..=0xa4cf).contains(&code)
                || (0xac00..=0xd7a3).contains(&code)
                || (0xf900..=0xfaff).contains(&code)
                || (0xfe30..=0xfe6f).contains(&code)
                || (0xff00..=0xff60).contains(&code)
                || (0xffe0..=0xffe6).contains(&code)
            {
                2
            } else {
                1
            }
        })
        .sum()
}

/// 在没有显式指定数据目录时，按常见布局自动挑一个：
/// ./data → ../data → 当前目录（含 catalog.json 时）
fn detect_data_dir() -> String {
    for candidate in ["./data", "../data"] {
        if Path::new(candidate).is_dir() {
            return candidate.to_string();
        }
    }
    if Path::new("catalog.json").exists() {
        return ".".to_string();
    }
    "./data".to_string()
}

fn run_serve(dir: &str, extra: &[String]) -> Result<()> {
    let mut options = ServerOptions::default();
    // 与 launcher 约定：设置 LDB_NO_OPEN=1 时不自动打开浏览器
    if std::env::var("LDB_NO_OPEN")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        options.open_browser = false;
    }

    // ---- 配置文件先加载：命令行在下面覆盖，形成「CLI > 配置 > 默认」 ----
    let config_arg = value_of(extra, "--config");
    let config = match littledb::config::discover(
        config_arg.as_deref().map(std::path::Path::new),
        Path::new(dir),
    ) {
        Some(path) => {
            let loaded = littledb::config::FileConfig::load(&path)?;
            loaded.apply_to(&mut options);
            Some((path, loaded))
        }
        None => None,
    };

    // 日志尽早初始化：后面的启动信息、慢查询、连接错误都要进文件
    let log_file = value_of(extra, "--log-file")
        .map(std::path::PathBuf::from)
        .or_else(|| config.as_ref().and_then(|(_, cfg)| cfg.log_path()));
    let log_max_bytes = value_of(extra, "--log-max-bytes")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| config.as_ref().map(|(_, cfg)| cfg.log_max_bytes()))
        .unwrap_or(littledb::log::DEFAULT_MAX_BYTES);
    let mut log_path: Option<std::path::PathBuf> = None;
    if let Some(path) = log_file {
        // 相对路径按数据目录解析，避免「从别的目录启动日志跑到别处」
        let resolved = if path.is_absolute() {
            path
        } else {
            Path::new(dir).join(path)
        };
        littledb::log::init_file(&resolved, log_max_bytes)?;
        log_path = Some(resolved);
    }

    let mut i = 0;
    while i < extra.len() {
        match extra[i].as_str() {
            "-p" | "--port" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.port = value.parse().map_err(|_| {
                        littledb::DbError::internal(format!("端口不合法：{}", value))
                    })?;
                }
            }
            "--host" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.host = value.clone();
                }
            }
            "--http-port" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.http_port = value.parse().ok();
                }
            }
            "--no-http" => options.http_port = None,
            "--no-open" => options.open_browser = false,
            "--web-dir" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.web_dir = std::path::PathBuf::from(value);
                }
            }
            "--token" => {
                i += 1;
                options.token = extra.get(i).cloned();
            }
            "--cors-origin" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.cors_origin = Some(value.clone());
                }
            }
            "--allow-insecure" => options.allow_insecure = true,
            "--auth-fail-limit" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.auth_fail_limit = value.parse().unwrap_or(5);
                }
            }
            "--max-connections" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.max_connections = value.parse().unwrap_or(64);
                }
            }
            "--slow-ms" => {
                i += 1;
                if let Some(value) = extra.get(i) {
                    options.slow_ms = value.parse().unwrap_or(0.0);
                }
            }
            "--sync" => {
                i += 1;
                match extra.get(i).map(|s| s.as_str()) {
                    Some("full") => options.durable_commits = true,
                    Some("normal") => options.durable_commits = false,
                    other => {
                        eprintln!("--sync 需要 full 或 normal，实际是 {:?}", other);
                        exit(1);
                    }
                }
            }
            "--verbose" => options.verbose = true,
            // 下面三个在前面已经消费过（配置要先于其它参数生效）
            "--config" | "--log-file" | "--log-max-bytes" => {
                i += 1;
            }
            other => {
                eprintln!("未知参数: {}", other);
                exit(1);
            }
        }
        i += 1;
    }

    // 环境变量兜底：避免令牌出现在命令行（进程列表、PowerShell 历史都能看到）
    if options.token.is_none() {
        if let Ok(value) = std::env::var("LDB_TOKEN") {
            let value = value.trim().to_string();
            if !value.is_empty() {
                options.token = Some(value);
            }
        }
    }

    // 安全默认：对外监听却没有令牌 → 拒绝启动（--allow-insecure 可显式放行）
    if let Err(err) = littledb::server::validate_bind(&options) {
        eprintln!("[LittleDB] {}", err.message);
        exit(1);
    }

    if let Some(path) = &log_path {
        // 控制台上留一句「日志写到哪」，文件里也有同样内容
        println!("[LittleDB] 日志文件: {}", path.display());
    }

    let engine = Engine::open(Path::new(dir))?;
    let stats = engine.stats();
    let recovered = engine.store.recovered_txns;
    littledb::log::info(&format!(
        "启动：数据目录 {}（{} 张表 · {} 页 · {} 字节 · 重放 WAL {} 条记录 / {} 个显式事务）",
        dir, stats.tables, stats.pages, stats.bytes,
        engine.store.recovered_records, recovered
    ));
    if options.token.is_some() {
        littledb::log::info("已启用 token 认证");
    }
    if !options.durable_commits {
        littledb::log::warn("同步模式 normal：提交不 fsync，掉电可能丢失最近的提交");
    }

    let server = Server::new(engine, options);
    server.run()
}

// ---------------------------------------------------------------- 后台运行
//
// 目标：关掉终端/窗口后服务继续跑，并且日志落到文件里可查。
// 实现：把 `serve` 作为**分离进程**再拉起来，pid 写进数据目录。
// 注意：这不是 Windows 服务（SCM 服务宿主需要 StartServiceCtrlDispatcher，
// 要额外 FFI + 管理员权限）；开机自启用「任务计划程序」或 NSSM 包一层即可。

fn pid_path(dir: &str) -> std::path::PathBuf {
    Path::new(dir).join("littledb.pid")
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::process::Command;
    match Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    use std::process::Command;
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    use std::process::Command;
    // 用 output() 而不是 status()：taskkill 会往控制台打 "SUCCESS: ..." 之类的噪音
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(littledb::DbError::internal(format!(
            "taskkill 失败：PID {}（{}）",
            pid,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(not(windows))]
fn terminate(pid: u32) -> Result<()> {
    use std::process::Command;
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    std::thread::sleep(std::time::Duration::from_millis(300));
    if process_alive(pid) {
        let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
    }
    Ok(())
}

fn run_start(dir: &str, extra: &[String]) -> Result<()> {
    let path = pid_path(dir);
    if let Some(pid) = read_pid(&path) {
        if process_alive(pid) {
            println!("已在后台运行（PID {}）。要重启请先：littledb stop {}", pid, dir);
            return Ok(());
        }
        let _ = std::fs::remove_file(&path);
    }

    let mut args: Vec<String> = vec![
        "serve".to_string(),
        dir.to_string(),
        "--no-open".to_string(),
    ];
    args.extend(extra.iter().cloned());
    // 没指定日志就默认写 <数据目录>/littledb.log——
    // 分离进程的输出没有终端可去，不落文件就相当于丢了
    if !extra.iter().any(|arg| arg == "--log-file") {
        args.push("--log-file".to_string());
        args.push("littledb.log".to_string());
    }

    let log_file = Path::new(dir).join("littledb.log");
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?;
    let stderr = stdout.try_clone()?;

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP：父进程退出后继续跑
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    let child = command.spawn()?;
    std::fs::write(&path, child.id().to_string())?;
    println!("已后台启动（PID {}）", child.id());
    println!("日志：{}", log_file.display());
    println!("状态：littledb status {}    停止：littledb stop {}", dir, dir);
    Ok(())
}

fn run_stop(dir: &str) -> Result<()> {
    let path = pid_path(dir);
    let Some(pid) = read_pid(&path) else {
        println!("没有找到 {}（可能未用 start 启动）", path.display());
        return Ok(());
    };
    if !process_alive(pid) {
        let _ = std::fs::remove_file(&path);
        println!("进程 {} 已退出，已清理 pid 文件", pid);
        return Ok(());
    }
    terminate(pid)?;
    for _ in 0..50 {
        if !process_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&path);
    if process_alive(pid) {
        return Err(littledb::DbError::internal(format!(
            "PID {} 仍在运行，请手动结束它",
            pid
        )));
    }
    // 直接终止进程 = 一次崩溃，WAL 会在下次启动时重放已提交的事务
    println!("已停止（PID {}）；未落盘的提交会在下次启动时按 WAL 恢复", pid);
    Ok(())
}

fn run_status(dir: &str) -> Result<()> {
    let path = pid_path(dir);
    match read_pid(&path) {
        Some(pid) if process_alive(pid) => println!("后台状态：运行中（PID {}）", pid),
        Some(pid) => println!("后台状态：未运行（PID {} 已失效，pid 文件可删）", pid),
        None => println!("后台状态：未运行（没有 {}）", path.display()),
    }
    println!("数据目录：{}", dir);
    let log_file = Path::new(dir).join("littledb.log");
    if let Ok(meta) = std::fs::metadata(&log_file) {
        println!("日志：{}（{} 字节）", log_file.display(), meta.len());
    } else {
        println!("日志：{}（尚未生成）", log_file.display());
    }
    Ok(())
}

/// 取 `--flag value` 里的值（启动参数扫描用）
fn value_of(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|pos| args.get(pos + 1))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 导出为 SQL 脚本（默认 stdout，便于重定向或接管道）
fn run_dump(dir: &str, output: Option<&str>) -> Result<()> {
    let mut engine = Engine::open(Path::new(dir))?;
    let script = littledb::dump::dump_script(&mut engine)?;
    let tables = script.matches("CREATE TABLE").count();
    engine.close()?;
    match output {
        Some(path) => {
            std::fs::write(path, script.as_bytes())?;
            println!("已导出 {} 张表 → {}", tables, path);
        }
        None => print!("{}", script),
    }
    Ok(())
}

/// 导入 SQL 脚本：每 --batch 条语句一个事务
fn run_load(dir: &str, file: &str, batch: usize) -> Result<()> {
    let script = std::fs::read_to_string(file)?;
    let mut session = Session::open(Path::new(dir))?;
    let stats = littledb::dump::load_script(&mut session, &script, batch)?;
    session.close()?;
    println!(
        "已导入 {} 条语句（{} 批，每批 {} 条）← {}",
        stats.statements, stats.batches, batch, file
    );
    Ok(())
}

fn run_demo(dir: &str) -> Result<()> {
    let path = Path::new(dir);
    let _ = std::fs::remove_dir_all(path);

    let mut session = Session::open(path)?;
    session.execute(
        "CREATE TABLE players (id INT PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
         score INT, joined DATETIME)",
        &[],
    )?;

    for i in 1..=5 {
        session.execute(
            "INSERT INTO players VALUES (?, ?, ?, ?)",
            &[
                Value::Int(i),
                Value::Text(format!("player{}", i)),
                Value::Int(i * 100),
                Value::Text("2026-10-30 08:00:00".to_string()),
            ],
        )?;
    }

    let selected = session.execute(
        "SELECT id, name, score FROM players WHERE score >= 200 ORDER BY score DESC",
        &[],
    )?;
    println!("查询结果:");
    render(&selected);

    let aggregated = session.execute(
        "SELECT COUNT(*) AS total, SUM(score) AS sum_score, AVG(score) AS avg_score FROM players",
        &[],
    )?;
    println!("\n聚合结果:");
    render(&aggregated);

    let explained = session.execute("EXPLAIN SELECT * FROM players WHERE id = 3", &[])?;
    println!("\n执行计划:");
    render(&explained);

    session.close()?;

    let mut reopened = Session::open(path)?;
    let count = reopened.execute("SELECT COUNT(*) FROM players", &[])?;
    println!("\n重新打开后:");
    render(&count);
    reopened.close()?;
    Ok(())
}

fn run_verify(dir: &str, deep: bool) -> Result<()> {
    let mut engine = Engine::open(Path::new(dir))?;
    let stats = engine.stats();

    println!("数据目录 : {}", dir);
    println!("表数量   : {}", engine.tables.len());
    println!("页 / 字节: {} 页 · {} 字节", stats.pages, stats.bytes);
    println!("WAL      : {} 字节", stats.wal_bytes);
    println!(
        "恢复     : {} 个事务 / {} 条记录 / 清理 {} 个残留文件",
        engine.store.recovered_txns,
        engine.store.recovered_records,
        engine.store.orphans_removed
    );
    println!();

    if deep {
        println!("深度校验中（全表扫描 + 索引重建比对）…");
        let problems = engine.deep_verify();
        if problems.is_empty() {
            println!("深度校验通过：row_count、索引内容、row_id 边界、空闲页全部一致");
        } else {
            println!("深度校验发现 {} 个问题：", problems.len());
            for problem in &problems {
                println!("  - {}", problem);
            }
            engine.close()?;
            exit(1);
        }
        println!();
    }

    let names = engine.table_names();
    for name in &names {
        let columns: Vec<String> = engine
            .table(name)?
            .columns
            .iter()
            .map(|c| format!("{}:{}", c.name, c.ty.as_str()))
            .collect();
        let indexes: Vec<String> = engine
            .table(name)?
            .indexes
            .iter()
            .map(|i| i.name.clone())
            .collect();
        let rows = engine.scan(name)?;
        println!("表 {}", name);
        println!("  列   : {}", columns.join(", "));
        println!(
            "  索引 : {}",
            if indexes.is_empty() {
                "无".to_string()
            } else {
                indexes.join(", ")
            }
        );
        println!("  行数 : {}", rows.len());
        for (_, values) in rows.iter().take(2) {
            let text: Vec<String> = values.iter().map(|v| v.as_text()).collect();
            println!("  示例 : {}", text.join(" | "));
        }
    }

    engine.close()?;
    Ok(())
}

