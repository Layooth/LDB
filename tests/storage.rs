//! 存储层集成测试：从建表到重开，覆盖事务、约束、索引与恢复。

use std::path::PathBuf;

use littledb::sql::Session;
use littledb::{Column, ColumnType, Engine, Result, Value};

fn temp_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("littledb-it-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn players_columns() -> Vec<Column> {
    let mut id = Column::new("id", ColumnType::Int);
    id.primary_key = true;
    id.not_null = true;
    let mut name = Column::new("name", ColumnType::Text);
    name.unique = true;
    vec![id, name, Column::new("score", ColumnType::Int)]
}

fn setup(engine: &mut Engine) -> Result<()> {
    engine.write(|e| -> Result<()> {
        e.create_table("players", players_columns(), false)
    })
}

#[test]
fn crud_and_reopen() {
    let dir = temp_dir("crud");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();

        engine
            .write(|e| -> Result<()> {
                for i in 0..20 {
                    e.insert(
                        "players",
                        vec![
                            Value::Int(i),
                            Value::Text(format!("p{}", i)),
                            Value::Int(i * 10),
                        ],
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let rows = engine.scan("players").unwrap();
        assert_eq!(rows.len(), 20);

        let ids = engine.find_by_equality("players", "id", &Value::Int(7), None).unwrap();
        assert_eq!(ids.as_ref().map(|v| v.len()), Some(1));
        let row = engine.get("players", ids.unwrap()[0]).unwrap().unwrap();
        assert_eq!(row[1], Value::Text("p7".to_string()));
        engine.close().unwrap();
    }

    let mut engine = Engine::open(&dir).unwrap();
    let rows = engine.scan("players").unwrap();
    assert_eq!(rows.len(), 20, "重新打开后数据应完整");
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transaction_rollback_keeps_data() {
    let dir = temp_dir("rollback");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    engine
        .write(|e| -> Result<()> {
            e.insert(
                "players",
                vec![Value::Int(1), Value::Text("keep".to_string()), Value::Int(0)],
            )?;
            Ok(())
        })
        .unwrap();

    let result: Result<()> = engine.write(|e| -> Result<()> {
        e.insert(
            "players",
            vec![Value::Int(2), Value::Text("temp".to_string()), Value::Int(0)],
        )?;
        Err(littledb::DbError::internal("故意失败"))
    });
    assert!(result.is_err());

    let rows = engine.scan("players").unwrap();
    assert_eq!(rows.len(), 1, "失败的事务必须整体回滚");
    assert_eq!(rows[0].1[1], Value::Text("keep".to_string()));

    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn constraints_enforced() {
    let dir = temp_dir("constraint");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    engine
        .write(|e| -> Result<()> {
            e.insert(
                "players",
                vec![Value::Int(1), Value::Text("a".to_string()), Value::Int(0)],
            )?;
            Ok(())
        })
        .unwrap();

    let dup_pk: Result<()> = engine.write(|e| -> Result<()> {
        e.insert(
            "players",
            vec![Value::Int(1), Value::Text("b".to_string()), Value::Int(0)],
        )?;
        Ok(())
    });
    assert!(dup_pk.is_err());

    let dup_unique: Result<()> = engine.write(|e| -> Result<()> {
        e.insert(
            "players",
            vec![Value::Int(2), Value::Text("a".to_string()), Value::Int(0)],
        )?;
        Ok(())
    });
    assert!(dup_unique.is_err());

    let null_pk: Result<()> = engine.write(|e| -> Result<()> {
        e.insert(
            "players",
            vec![Value::Null, Value::Text("c".to_string()), Value::Int(0)],
        )?;
        Ok(())
    });
    assert!(null_pk.is_err());

    assert_eq!(engine.scan("players").unwrap().len(), 1);
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_replaces_row_and_keeps_index_consistent() {
    let dir = temp_dir("update");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    let row_id = engine
        .write(|e| -> Result<u32> {
            e.insert(
                "players",
                vec![Value::Int(1), Value::Text("old".to_string()), Value::Int(10)],
            )
        })
        .unwrap();

    engine
        .write(|e| -> Result<()> {
            e.update(
                "players",
                row_id,
                vec![Value::Int(1), Value::Text("new".to_string()), Value::Int(20)],
            )?;
            Ok(())
        })
        .unwrap();

    let ids = engine.find_by_equality("players", "name", &Value::Text("new".to_string()), None).unwrap();
    assert_eq!(ids.as_ref().map(|v| v.len()), Some(1));
    assert!(engine
        .find_by_equality("players", "name", &Value::Text("old".to_string()), None)
        .unwrap()
        .map(|v| v.is_empty())
        .unwrap_or(false));

    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drop_table_removes_file() {
    let dir = temp_dir("drop");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();
    engine
        .write(|e| -> Result<()> {
            e.insert(
                "players",
                vec![Value::Int(1), Value::Text("x".to_string()), Value::Int(0)],
            )?;
            Ok(())
        })
        .unwrap();
    engine.close().unwrap();

    let data_dir = dir.join("data");
    let before = std::fs::read_dir(&data_dir).unwrap().count();
    assert!(before >= 1);

    let mut engine = Engine::open(&dir).unwrap();
    engine.write(|e| -> Result<()> { e.drop_table("players", false) }).unwrap();
    engine.close().unwrap();

    let after = std::fs::read_dir(&data_dir).unwrap().count();
    assert_eq!(after, 0, "表文件应被清理");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wal_recovery_after_crash() {
    let dir = temp_dir("recover");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine
            .write(|e| -> Result<()> {
                for i in 0..5 {
                    e.insert(
                        "players",
                        vec![
                            Value::Int(i),
                            Value::Text(format!("r{}", i)),
                            Value::Int(i),
                        ],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        // 不 close：模拟进程被强杀
    }

    let mut engine = Engine::open(&dir).unwrap();
    assert!(engine.store.recovered_txns >= 1, "应从 WAL 恢复出已提交事务");
    assert_eq!(engine.scan("players").unwrap().len(), 5);
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 快照可见性 + 多写语义：
/// - 开放事务的未提交行对其它会话不可见（不脏读）
/// - 其它会话的写不再被阻塞（P2 多写）
/// - 同页写-写冲突 → 首写者获胜，后者收到 WRITE_CONFLICT
#[test]
fn cross_session_reads_see_only_committed() {
    use littledb::error::WRITE_CONFLICT;
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("isolation");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine
            .write(|e| -> Result<()> {
                e.insert(
                    "players",
                    vec![Value::Int(1), Value::Text("committed".to_string()), Value::Int(0)],
                )?;
                Ok(())
            })
            .unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut writer = ConnSession::new(Arc::clone(&engine));
    let mut reader = ConnSession::new(Arc::clone(&engine));

    writer.execute("BEGIN", &[]).unwrap();
    writer
        .execute("INSERT INTO players VALUES (2, 'uncommitted', 0)", &[])
        .unwrap();

    // 未提交的行必须不可见
    let seen = reader.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // P2：其它会话的写不再被阻塞（隐式事务立即提交）
    reader
        .execute("INSERT INTO players VALUES (3, 'by-reader', 0)", &[])
        .unwrap_or_else(|e| panic!("多写下其它会话的写不应被阻塞: {}", e));
    // 但写者的未提交数据依然不可见
    let seen = reader.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // 写者提交：与读者的插入落在同一页 → 页级写冲突，首写者（读者）获胜
    let commit_err = writer.execute("COMMIT", &[]).unwrap_err();
    assert_eq!(commit_err.code, WRITE_CONFLICT, "{}", commit_err.message);

    // 冲突方数据已回滚：读者提交的数据完好，写者的未提交行消失
    let seen = reader.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // 回滚路径：未提交的行消失，且不会残留在索引/缓存里
    writer.execute("BEGIN", &[]).unwrap();
    writer
        .execute("INSERT INTO players VALUES (4, 'rolled-back', 0)", &[])
        .unwrap();
    writer.execute("ROLLBACK", &[]).unwrap();
    let seen = reader.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    drop(writer);
    drop(reader);
    let engine = Arc::into_inner(engine).unwrap();
    let mut engine = RwLock::into_inner(engine).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// P2 主菜：两个会话同时各持开放事务，写不同的表互不阻塞、都提交成功
#[test]
fn multi_writer_disjoint_tables_commit() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("multiwriter");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();
    engine
        .write(|e| -> Result<()> {
            e.create_table("notes", vec![Column::new("id", ColumnType::Int)], false)
        })
        .unwrap();
    engine.close().unwrap();

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut a = ConnSession::new(Arc::clone(&engine));
    let mut b = ConnSession::new(Arc::clone(&engine));

    a.execute("BEGIN", &[]).unwrap();
    b.execute("BEGIN", &[]).unwrap();
    a.execute("INSERT INTO players VALUES (10, 'from-a', 0)", &[]).unwrap();
    b.execute("INSERT INTO notes VALUES (1)", &[]).unwrap();
    a.execute("COMMIT", &[]).unwrap();
    b.execute("COMMIT", &[]).unwrap();

    let seen = b.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    let seen = a.execute("SELECT COUNT(*) FROM notes", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    drop(a);
    drop(b);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 回滚的索引精确撤销：A 回滚后，B 的唯一索引查询不受污染
#[test]
fn rollback_keeps_index_consistent_for_others() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("rollback-index");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap(); // name 列 UNIQUE
        engine
            .write(|e| -> Result<()> {
                e.insert("players", vec![Value::Int(1), Value::Text("kept".into()), Value::Int(0)])?;
                Ok(())
            })
            .unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut a = ConnSession::new(Arc::clone(&engine));
    let mut b = ConnSession::new(Arc::clone(&engine));

    a.execute("BEGIN", &[]).unwrap();
    a.execute("INSERT INTO players VALUES (2, 'ghost', 0)", &[]).unwrap();
    b.execute("INSERT INTO players VALUES (3, 'real', 0)", &[]).unwrap();
    a.execute("ROLLBACK", &[]).unwrap();

    // A 的幽灵行不在索引里：同 key 再插入应成功
    b.execute("INSERT INTO players VALUES (4, 'ghost', 0)", &[]).unwrap();
    let seen = b.execute("SELECT name FROM players WHERE name = 'ghost'", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    // B 自己的未提交行在自己眼里可见
    let seen = b.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(3)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    b.execute("COMMIT", &[]).unwrap();

    drop(a);
    drop(b);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// DDL 独占门禁：其它会话有开放事务时，DDL 拒绝执行
#[test]
fn ddl_gate_with_open_txn() {
    use littledb::sql::executor::ConnSession;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("ddlgate");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut a = ConnSession::new(Arc::clone(&engine));
    let mut b = ConnSession::new(Arc::clone(&engine));

    a.execute("BEGIN", &[]).unwrap();
    let err = b
        .execute("CREATE TABLE gated (id INT PRIMARY KEY)", &[])
        .unwrap_err();
    assert_eq!(err.code, "DATABASE_LOCKED", "{}", err.message);
    // A 提交后 DDL 恢复可用
    a.execute("COMMIT", &[]).unwrap();
    b.execute("CREATE TABLE gated (id INT PRIMARY KEY)", &[]).unwrap();

    drop(a);
    drop(b);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn many_rows_cross_pages() {
    let dir = temp_dir("pages");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    // 每行约 40 字节，500 行必然跨多页
    engine
        .write(|e| -> Result<()> {
            for i in 0..500 {
                e.insert(
                    "players",
                    vec![
                        Value::Int(i),
                        Value::Text(format!("user-{:04}", i)),
                        Value::Int(i),
                    ],
                )?;
            }
            Ok(())
        })
        .unwrap();

    let rows = engine.scan("players").unwrap();
    assert_eq!(rows.len(), 500);

    let stats = engine.stats();
    assert!(stats.pages > 1, "500 行应占用多页，实际 {}", stats.pages);

    // 删除一半再验证
    engine
        .write(|e| -> Result<()> {
            let ids: Vec<u32> = e.scan("players")?.iter().map(|item| item.0).take(250).collect();
            for id in ids {
                e.delete("players", id)?;
            }
            Ok(())
        })
        .unwrap();

    assert_eq!(engine.scan("players").unwrap().len(), 250);
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// JOIN / UNION 端到端语义
#[test]
fn joins_and_unions() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("joins");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)", &[])
        .unwrap();
    session
        .execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, item TEXT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO users VALUES (1,'alice'), (2,'bob'), (3,'carol')",
            &[],
        )
        .unwrap();
    session
        .execute(
            "INSERT INTO orders VALUES (10,1,'book'), (11,1,'pen'), (12,2,'cup')",
            &[],
        )
        .unwrap();

    macro_rules! rows {
        ($sql:expr $(,)?) => {
            match session.execute($sql, &[]).unwrap() {
                QueryResult::Select { rows, .. } => rows,
                other => panic!("期望 SELECT，得到 {:?}", other),
            }
        };
    }
    macro_rules! must_err {
        ($sql:expr $(,)?) => {
            assert!(session.execute($sql, &[]).is_err(), "应当报错: {}", $sql)
        };
    }

    // INNER JOIN：等值连接 + 限定列名
    let got = rows!("SELECT u.name, o.item FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name, o.item");
    assert_eq!(
        got,
        vec![
            vec![Value::Text("alice".into()), Value::Text("book".into())],
            vec![Value::Text("alice".into()), Value::Text("pen".into())],
            vec![Value::Text("bob".into()), Value::Text("cup".into())],
        ]
    );

    // LEFT JOIN：无匹配的左行补 NULL
    let got = rows!("SELECT u.name, o.item FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY u.name, o.item");
    assert_eq!(
        got,
        vec![
            vec![Value::Text("alice".into()), Value::Text("book".into())],
            vec![Value::Text("alice".into()), Value::Text("pen".into())],
            vec![Value::Text("bob".into()), Value::Text("cup".into())],
            vec![Value::Text("carol".into()), Value::Null],
        ]
    );

    // 逗号连接 = CROSS JOIN：3 × 3 = 9 行
    let got = rows!("SELECT COUNT(*) FROM users, orders");
    assert_eq!(got, vec![vec![Value::Int(9)]]);

    // JOIN + WHERE + 聚合
    let got = rows!(
        "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id \
         GROUP BY u.name ORDER BY u.name",
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Text("alice".into()), Value::Int(2)],
            vec![Value::Text("bob".into()), Value::Int(1)],
        ]
    );

    // 歧义裸列名必须报错并提示限定
    must_err!("SELECT id FROM users u JOIN orders o ON u.id = o.user_id");

    // UNION 去重 / UNION ALL 保留
    let got = rows!("SELECT user_id FROM orders UNION SELECT id FROM users ORDER BY id");
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]);
    let got = rows!("SELECT user_id FROM orders UNION ALL SELECT id FROM users");
    assert_eq!(got.len(), 6);
    must_err!("SELECT id, name FROM users UNION SELECT id FROM users");

    // JOIN 走索引路径（user_id 上建索引后结果一致）
    session.execute("CREATE INDEX idx_o_user ON orders(user_id)", &[]).unwrap();
    let got = rows!("SELECT u.name, o.item FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name, o.item");
    assert_eq!(got.len(), 3);
    let got = rows!("SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.item IS NULL");
    assert_eq!(got, vec![vec![Value::Text("carol".into())]]);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// RwLock 并发读：写事务开放期间，其它会话的读不被阻塞、只见已提交数据
#[test]
fn concurrent_readers_during_open_txn() {
    use littledb::sql::executor::ConnSession;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    let dir = temp_dir("concurrent");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();
    engine
        .write(|e| -> Result<()> {
            e.insert("players", vec![Value::Int(1), Value::Text("base".into()), Value::Int(0)])?;
            Ok(())
        })
        .unwrap();
    engine.close().unwrap();

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut writer = ConnSession::new(Arc::clone(&engine));

    writer.execute("BEGIN", &[]).unwrap();
    writer
        .execute("INSERT INTO players VALUES (2, 'uncommitted', 0)", &[])
        .unwrap();

    // 4 个读会话并发 SELECT：必须立即返回且只见已提交的 1 行
    let reads_total = 400;
    let success = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let engine = Arc::clone(&engine);
        let success = Arc::clone(&success);
        handles.push(std::thread::spawn(move || {
            let mut reader = ConnSession::new(engine);
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut done = 0usize;
            while done < reads_total / 4 && Instant::now() < deadline {
                match reader.execute("SELECT COUNT(*) FROM players", &[]) {
                    Ok(littledb::sql::QueryResult::Select { rows, .. }) => {
                        assert_eq!(rows, vec![vec![Value::Int(1)]], "未提交数据不可见");
                        done += 1;
                    }
                    _ => panic!("并发读失败"),
                }
            }
            success.fetch_add(done, std::sync::atomic::Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        success.load(std::sync::atomic::Ordering::SeqCst),
        reads_total,
        "读会话应在写事务开放期间完成全部读"
    );

    // 提交后可见
    writer.execute("COMMIT", &[]).unwrap();
    let mut reader = ConnSession::new(Arc::clone(&engine));
    match reader.execute("SELECT COUNT(*) FROM players", &[]).unwrap() {
        littledb::sql::QueryResult::Select { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(2)]])
        }
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    drop(writer);
    drop(reader);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 组提交：并发写会话全部成功且数据完整（fsync 已移出引擎写锁）
#[test]
fn concurrent_writers_group_commit() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("groupcommit");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();
    engine.close().unwrap();

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut handles = Vec::new();
    for tid in 0..8 {
        let engine = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let mut session = ConnSession::new(engine);
            for i in 0..25 {
                let id = tid * 25 + i;
                session
                    .execute(
                        &format!("INSERT INTO players VALUES ({}, 'w{}', {})", id, id, i),
                        &[],
                    )
                    .unwrap_or_else(|e| panic!("并发写失败: {}", e));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut check = ConnSession::new(Arc::clone(&engine));
    match check.execute("SELECT COUNT(*) FROM players", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(200)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    // 全部 200 行按主键可查（索引一致）
    match check
        .execute("SELECT COUNT(*) FROM players WHERE id >= 0", &[])
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(200)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    drop(check);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 子查询：WHERE 列 IN (SELECT ...)
#[test]
fn subquery_in_where() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("subquery");
    let mut session = Session::open(&dir).unwrap();
    session.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)", &[]).unwrap();
    session.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, item TEXT)", &[]).unwrap();
    session.execute("INSERT INTO users VALUES (1,'alice'), (2,'bob'), (3,'carol')", &[]).unwrap();
    session.execute("INSERT INTO orders VALUES (10,1,'book'), (11,2,'cup')", &[]).unwrap();

    macro_rules! rows {
        ($sql:expr) => {
            match session.execute($sql, &[]).unwrap() {
                QueryResult::Select { rows, .. } => rows,
                other => panic!("期望 SELECT，得到 {:?}", other),
            }
        };
    }

    // 基本语义
    let got = rows!("SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY name");
    assert_eq!(got, vec![vec![Value::Text("alice".into())], vec![Value::Text("bob".into())]]);
    // NOT IN
    let got = rows!("SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders) ORDER BY name");
    assert_eq!(got, vec![vec![Value::Text("carol".into())]]);
    // 子查询带 WHERE / LIMIT
    let got = rows!("SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE item = 'cup') ");
    assert_eq!(got, vec![vec![Value::Text("bob".into())]]);
    // 空子查询 → 无命中
    let got = rows!("SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE id > 999)");
    assert!(got.is_empty());
    // 多列子查询报错
    assert!(session.execute("SELECT name FROM users WHERE id IN (SELECT id, user_id FROM orders)", &[]).is_err());
    // 与 AND/OR 组合
    let got = rows!("SELECT name FROM users WHERE (id IN (SELECT user_id FROM orders) AND id > 1) OR name = 'carol' ORDER BY name");
    assert_eq!(got, vec![vec![Value::Text("bob".into())], vec![Value::Text("carol".into())]]);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 复刻 HTTP 冲突场景：A BEGIN → A UPDATE → B BEGIN → B UPDATE → B COMMIT → A COMMIT
#[test]
fn http_conflict_sequence_repro() {
    use littledb::sql::executor::ConnSession;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("httpconflict");
    {
        let mut engine = Engine::open(&dir).unwrap();
        engine
            .write(|e| -> Result<()> {
                e.create_table(
                    "acc",
                    vec![{
                        let mut c = Column::new("id", ColumnType::Int);
                        c.primary_key = true;
                        c
                    }],
                    false,
                )?;
                e.insert("acc", vec![Value::Int(1)])?;
                Ok(())
            })
            .unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut a = ConnSession::new(Arc::clone(&engine));
    let mut b = ConnSession::new(Arc::clone(&engine));

    a.execute("BEGIN", &[]).unwrap();
    let r = a.execute("UPDATE acc SET id = 999 WHERE id = 1", &[]);
    println!("A update: {:?}", r.as_ref().map(|x| x.summary()));
    b.execute("BEGIN", &[]).unwrap();
    let r = b.execute("UPDATE acc SET id = 777 WHERE id = 1", &[]);
    println!("B update: {:?}", r.as_ref().map(|x| x.summary()));
    let r = b.execute("COMMIT", &[]);
    println!("B commit: {:?}", r.as_ref().map(|x| x.summary()));
    let r = a.execute("COMMIT", &[]);
    println!("A commit: {:?}", r);
    let r = b.execute("SELECT id FROM acc", &[]).unwrap();
    match r {
        littledb::sql::QueryResult::Select { rows, .. } => println!("final rows: {:?}", rows),
        other => println!("other: {:?}", other),
    }

    drop(a);
    drop(b);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 跨会话连续 UPDATE：每个语句在不同会话的隐式事务里提交，
/// 索引的语句期更新 + 提交/回滚撤销必须保持一致
#[test]
fn cross_session_sequential_updates() {
    use littledb::sql::executor::ConnSession;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("sequpdate");
    {
        let mut engine = Engine::open(&dir).unwrap();
        engine
            .write(|e| -> Result<()> {
                e.create_table(
                    "acc",
                    vec![{
                        let mut c = Column::new("id", ColumnType::Int);
                        c.primary_key = true;
                        c
                    }],
                    false,
                )?;
                e.insert("acc", vec![Value::Int(1)])?;
                Ok(())
            })
            .unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    for round in 0..4 {
        let mut session = ConnSession::new(Arc::clone(&engine));
        session
            .execute(
                &format!(
                    "UPDATE acc SET id = {} WHERE id = {}",
                    100 + round,
                    if round == 0 { 1 } else { 99 + round }
                ),
                &[],
            )
            .unwrap_or_else(|e| panic!("round {} update 失败: {}", round, e));
        let mut probe = ConnSession::new(Arc::clone(&engine));
        match probe.execute("SELECT id FROM acc", &[]).unwrap() {
            littledb::sql::QueryResult::Select { rows, .. } => {
                println!("after round {}: rows = {:?}", round, rows)
            }
            other => println!("after round {}: {:?}", round, other.summary()),
        }
        {
            let g = engine.read().unwrap();
            let s = g.store.stats();
            println!(
                "  stats: commit_seq(私有) pending_pages={} checkpoints={}",
                s.pending_pages, s.checkpoints
            );
        }
    }
    let mut check = ConnSession::new(Arc::clone(&engine));
    let seen = check.execute("SELECT COUNT(*) FROM acc", &[]).unwrap();
    match seen {
        littledb::sql::QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    // 主键更新后旧值不可查、新值可查
    let seen = check.execute("SELECT id FROM acc WHERE id = 103", &[]).unwrap();
    match seen {
        littledb::sql::QueryResult::Select { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    drop(check);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tier1：长事务开放期间 WAL 超限，不得把库标记为待恢复；
/// 事务关闭后 checkpoint 排空；期间所有已提交数据可查且崩溃可恢复
#[test]
fn long_txn_does_not_brick_on_wal_overflow() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("longtxn");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine.store.set_wal_limit(64 * 1024); // 64KB，制造频繁触发
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));
    let mut a = ConnSession::new(Arc::clone(&engine));

    // A 开启长事务（不做任何写，占住水位线）
    a.execute("BEGIN", &[]).unwrap();

    // 其它会话持续写入：超过 64KB 的 WAL（约 100 行 × 4KB）
    let mut b = ConnSession::new(Arc::clone(&engine));
    for i in 0..100 {
        b.execute(
            &format!("INSERT INTO players VALUES ({}, 'bulk{}', {})", i, i, i),
            &[],
        )
        .unwrap_or_else(|e| panic!("WAL 超限期间的写入失败: {}", e));
    }

    // 关键断言：库没有进入 needs_recovery（旧实现这里会整库不可写）
    let still_writable = b
        .execute("INSERT INTO players VALUES (500, 'after-overflow', 0)", &[])
        .is_ok();
    assert!(still_writable, "WAL 超限后写入应继续可用");

    // A 的长事务回滚释放水位线
    a.execute("ROLLBACK", &[]).unwrap();

    // 之后的写入触发 checkpoint 排空
    b.execute("INSERT INTO players VALUES (501, 'drain', 0)", &[]).unwrap();

    // 数据完整
    let seen = b.execute("SELECT COUNT(*) FROM players", &[]).unwrap();
    match seen {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(102)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // kill -9 级恢复：强杀后重开，数据完整（未排空的 WAL 不会被错误截断）
    drop(a);
    drop(b);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    drop(engine);

    let mut engine = Engine::open(&dir).unwrap();
    let rows = engine.scan("players").unwrap();
    assert_eq!(rows.len(), 102, "重启后数据完整");
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 增量 checkpoint：单次落盘受页数上限约束，多次后全部排空
#[test]
fn incremental_checkpoint_drains_over_multiple_calls() {
    let dir = temp_dir("incrck");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    engine
        .write(|e| -> Result<()> {
            for i in 0..30 {
                e.insert(
                    "players",
                    vec![Value::Int(i), Value::Text(format!("r{}", i)), Value::Int(i)],
                )?;
            }
            Ok(())
        })
        .unwrap();

    // 手动多次增量：首次受限（用小表测不出上限，但验证多调用收敛）
    let mut calls = 0;
    while calls < 10 {
        engine.checkpoint().unwrap();
        calls += 1;
        if engine.store.stats().pending_pages == 0 {
            break;
        }
    }
    assert!(
        engine.store.stats().pending_pages == 0,
        "{} 次增量后应全部落盘",
        calls
    );

    // 全部落盘后数据完好
    let rows = engine.scan("players").unwrap();
    assert_eq!(rows.len(), 30);
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 多会话并发写：语句级由引擎写锁串行（见 mvcc-design.md §3.9），
/// 这里验证的是「并发写不丢行、不报错」，耗时只做诊断输出。
#[test]
fn concurrent_writes_stay_correct() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};
    use std::time::Instant;

    let dir = temp_dir("parwrite");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine
            .write(|e| -> Result<()> {
                e.create_table("notes", vec![Column::new("id", ColumnType::Int)], false)
            })
            .unwrap();
        engine.close().unwrap();
    }

    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));

    // 4 会话 × 200 行：2 个写 players，2 个写 notes（不同表 → 并行）
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for tid in 0..4 {
        let engine = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let mut s = ConnSession::new(engine);
            for i in 0..200 {
                let sql = if tid % 2 == 0 {
                    format!("INSERT INTO players VALUES ({}, 'p{}-{}', 0)", tid * 1000 + i, tid, i)
                } else {
                    format!("INSERT INTO notes VALUES ({})", tid * 1000 + i)
                };
                s.execute(&sql, &[]).unwrap_or_else(|e| panic!("并发写失败: {}", e));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let multi_elapsed = t0.elapsed();

    let mut check = ConnSession::new(Arc::clone(&engine));
    match check.execute("SELECT COUNT(*) FROM players", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(400)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    match check.execute("SELECT COUNT(*) FROM notes", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(400)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // 对照：同样的 400 行全部写同一张表（表锁串行）
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for tid in 0..4 {
        let engine = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            let mut s = ConnSession::new(engine);
            for i in 0..200 {
                s.execute(
                    &format!("INSERT INTO notes VALUES ({})", 10000 + tid * 1000 + i),
                    &[],
                )
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let same_elapsed = t0.elapsed();

    // 只做诊断输出，不断言耗时：写语句整条持有引擎全局写锁
    // （见 docs/mvcc-design.md §3.9），跨表写并没有真并行，
    // 两组耗时接近才是当前架构的预期。本用例断言的是正确性：并发写不丢行。
    println!(
        "multi-table(2 tables): {:?}, same-table: {:?}",
        multi_elapsed, same_elapsed
    );

    drop(check);
    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap()).unwrap();
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// P3 补充：范围索引扫描
#[test]
fn range_index_scan() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("rangescan");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE scores (id INT PRIMARY KEY, score INT, tag TEXT)", &[])
        .unwrap();
    // 0..50，tag 交替
    for i in 0..50 {
        session
            .execute(
                &format!(
                    "INSERT INTO scores VALUES ({}, {}, '{}')",
                    i,
                    i,
                    if i % 2 == 0 { "even" } else { "odd" }
                ),
                &[],
            )
            .unwrap();
    }
    session.execute("CREATE INDEX idx_score ON scores(score)", &[]).unwrap();

    let mut rows = |sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 范围：>45（走索引后行集应与全表扫描+过滤一致）
    let got = rows("SELECT COUNT(*) FROM scores WHERE score > 45");
    assert_eq!(got, vec![vec![Value::Int(4)]], "> 45");
    let got = rows("SELECT COUNT(*) FROM scores WHERE score >= 45");
    assert_eq!(got, vec![vec![Value::Int(5)]], ">= 45");
    let got = rows("SELECT COUNT(*) FROM scores WHERE score < 5");
    assert_eq!(got, vec![vec![Value::Int(5)]], "< 5");
    let got = rows("SELECT COUNT(*) FROM scores WHERE score <= 5");
    assert_eq!(got, vec![vec![Value::Int(6)]], "<= 5");
    let got = rows("SELECT COUNT(*) FROM scores WHERE score BETWEEN 10 AND 19");
    assert_eq!(got, vec![vec![Value::Int(10)]], "BETWEEN");
    // 组合范围 + 额外条件
    let got = rows("SELECT score FROM scores WHERE score BETWEEN 10 AND 19 AND tag = 'even' ORDER BY score");
    assert_eq!(
        got,
        vec![
            vec![Value::Int(10)],
            vec![Value::Int(12)],
            vec![Value::Int(14)],
            vec![Value::Int(16)],
            vec![Value::Int(18)],
        ]
    );
    // 反向形式（常量在左）
    let got = rows("SELECT COUNT(*) FROM scores WHERE 45 < score");
    assert_eq!(got, vec![vec![Value::Int(4)]], "45 < score");

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- HTTP 层

/// 起一个真实 HTTP 服务实例（随机端口），用于协议级用例
fn start_http(dir: &std::path::Path) -> u16 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, RwLock};

    let engine = Arc::new(RwLock::new(Engine::open(dir).unwrap()));
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let state = littledb::http::HttpState::new(
        engine,
        None,
        littledb::http::find_web_dir(),
        Arc::new(AtomicUsize::new(0)),
        8,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    std::thread::spawn(move || {
        let _ = littledb::http::serve(state, "127.0.0.1", port);
    });
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    port
}

fn http_roundtrip(port: u16, request: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

/// 畸形请求必须显式报错，而不是「静默当空 body 执行」或「直接重置连接」
#[test]
fn http_rejects_malformed_requests() {
    let dir = temp_dir("httpbad");
    let port = start_http(&dir);

    // chunked：不支持，必须 411 而不是把请求体当空
    let response = http_roundtrip(
        port,
        "POST /api/sql HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
         Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 411"), "{}", response);
    assert!(response.contains("LENGTH_REQUIRED"), "{}", response);

    // 非法 Content-Length
    let response = http_roundtrip(
        port,
        "POST /api/sql HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
         Content-Length: -5\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "{}", response);

    // 重复的关键头部（请求走私手法）
    let response = http_roundtrip(
        port,
        "POST /api/sql HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
         Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(response.starts_with("HTTP/1.1 400"), "{}", response);

    // 正常请求仍然工作
    let body = r#"{"sql":"SELECT 1 AS ok"}"#;
    let response = http_roundtrip(
        port,
        &format!(
            "POST /api/sql HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 静态文件：只有根路径映射 index.html，其余缺失文件必须 404
#[test]
fn http_static_404_instead_of_html_fallback() {
    let dir = temp_dir("httpstatic");
    let port = start_http(&dir);

    let response = http_roundtrip(
        port,
        "GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response);
    assert!(response.contains("<!DOCTYPE html>"), "首页应是控制台");

    let response = http_roundtrip(
        port,
        "GET /missing.js HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "缺失静态文件不应回退成 HTML：{}",
        response
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 锁中毒不能让整个服务不可用：旧实现里 `unwrap()` 会把一次 panic
/// 放大成「之后每个请求都 500」
#[test]
fn lock_poisoning_does_not_brick_service() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, RwLock};

    let dir = temp_dir("poison");
    {
        let mut engine = Engine::open(&dir).unwrap();
        setup(&mut engine).unwrap();
        engine.close().unwrap();
    }
    let engine = Arc::new(RwLock::new(Engine::open(&dir).unwrap()));

    // 持写锁时 panic → 锁进入中毒状态
    let poisoner = Arc::clone(&engine);
    let join = std::thread::spawn(move || {
        let _guard = poisoner.write().unwrap();
        panic!("故意制造锁中毒（测试）");
    });
    assert!(join.join().is_err(), "线程应 panic");
    assert!(engine.read().is_err(), "锁应处于中毒状态");

    // 之后依然能读、能写、能提交
    let mut session = ConnSession::new(Arc::clone(&engine));
    match session.execute("SELECT COUNT(*) FROM players", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(0)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    session
        .execute("INSERT INTO players VALUES (1,'after-poison',0)", &[])
        .unwrap();
    match session.execute("SELECT COUNT(*) FROM players", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    drop(session);

    let mut engine = RwLock::into_inner(Arc::into_inner(engine).unwrap())
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(engine.scan("players").unwrap().len(), 1, "写入应已持久化");
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 单语句 50 万行上限的**边界**验证（很慢，默认跳过）。
///
/// 跑法：`build.cmd test --release -- --ignored row_cap`
#[test]
#[ignore = "需要插入 50 万行，手动用 --release --ignored 跑"]
fn row_cap_rejects_full_scan_but_allows_limit() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("rowcap");
    const ROWS: i64 = 500_050; // 略超 MAX_SCAN_ROWS(500_000)
    {
        let mut engine = Engine::open(&dir).unwrap();
        engine
            .write(|e| -> Result<()> {
                // id 做主键：这样「索引查找不受上限影响」也有得测
                let mut id = Column::new("id", ColumnType::Int);
                id.primary_key = true;
                id.not_null = true;
                e.create_table(
                    "big",
                    vec![id, Column::new("v", ColumnType::Int)],
                    false,
                )
            })
            .unwrap();
        // 分批提交，避免单个事务撑爆
        let mut inserted = 0i64;
        while inserted < ROWS {
            let start = inserted;
            let end = (start + 50_000).min(ROWS);
            engine
                .write(|e| -> Result<()> {
                    for i in start..end {
                        e.insert("big", vec![Value::Int(i), Value::Int(0)])?;
                    }
                    Ok(())
                })
                .unwrap();
            inserted = end;
        }
        engine.close().unwrap();
    }

    let mut session = Session::open(&dir).unwrap();

    // 全表物化（聚合无法下推）→ 超上限报错
    let err = session.execute("SELECT COUNT(*) FROM big", &[]).unwrap_err();
    assert_eq!(err.code, "LIMIT_EXCEEDED", "{}", err.message);

    // 无条件的 DML 仍受上限保护
    let err = session.execute("UPDATE big SET v = 1", &[]).unwrap_err();
    assert_eq!(err.code, "LIMIT_EXCEEDED", "{}", err.message);
    let err = session.execute("DELETE FROM big", &[]).unwrap_err();
    assert_eq!(err.code, "LIMIT_EXCEEDED", "{}", err.message);

    // 但带主键条件的 DML 走索引定位，不受上限影响（E2 的核心收益）
    match session.execute("UPDATE big SET v = 1 WHERE id = 42", &[]).unwrap() {
        QueryResult::Update { updated } => assert_eq!(updated, 1),
        other => panic!("期望 UPDATE，得到 {:?}", other),
    }
    match session.execute("DELETE FROM big WHERE id = 43", &[]).unwrap() {
        QueryResult::Delete { deleted } => assert_eq!(deleted, 1),
        other => panic!("期望 DELETE，得到 {:?}", other),
    }

    // LIMIT 下推 / 索引查找都不受影响
    match session.execute("SELECT id FROM big LIMIT 5", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows.len(), 5),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    match session.execute("SELECT v FROM big WHERE id = 499999", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(0)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除只打标记、不改 freeOffset：不整理页内死空间的话，
/// 反复「插入-删除」会让表文件一路涨（旧实现里 compact_page 是死代码）
#[test]
fn deleted_space_is_reclaimed() {
    let dir = temp_dir("reclaim");
    let mut engine = Engine::open(&dir).unwrap();
    setup(&mut engine).unwrap();

    for round in 0..30 {
        engine
            .write(|e| -> Result<()> {
                for i in 0..50 {
                    e.insert(
                        "players",
                        vec![
                            Value::Int(round * 100 + i),
                            Value::Text(format!("n{}-{}", round, i)),
                            Value::Int(0),
                        ],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        engine
            .write(|e| -> Result<()> {
                let rows = e.scan("players")?;
                for (row_id, _) in rows {
                    e.delete("players", row_id)?;
                }
                Ok(())
            })
            .unwrap();
    }

    let stats = engine.stats();
    // 不回收时每轮 50 行记录（≈31B/行）都会永久占位，30 轮要十几页；
    // 回收后只剩 slot 目录（6B/行）的增长，约 3 页
    assert!(
        stats.pages <= 4,
        "删除后的空间应被复用，实际 {} 页",
        stats.pages
    );
    assert_eq!(engine.scan("players").unwrap().len(), 0);
    engine.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// UPDATE / DELETE 走索引定位：只取候选行，WHERE 仍逐行复检
#[test]
fn update_delete_use_index() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("writeidx");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE w (id INT PRIMARY KEY, v INT)", &[])
        .unwrap();
    session.execute("INSERT INTO w VALUES (1,0),(2,0),(3,0),(4,0),(42,0),(43,0)", &[]).unwrap();

    let mut affected = |session: &mut Session, sql: &str| -> usize {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Update { updated } => updated,
            QueryResult::Delete { deleted } => deleted,
            other => panic!("期望 UPDATE/DELETE，得到 {:?}", other),
        }
    };

    // 主键定位
    assert_eq!(affected(&mut session, "UPDATE w SET v = 9 WHERE id = 42"), 1);
    // 索引候选 + 复检：条件只命中一部分时不能多改
    assert_eq!(affected(&mut session, "UPDATE w SET v = 8 WHERE id = 42 AND v = 9"), 1);
    assert_eq!(affected(&mut session, "UPDATE w SET v = 7 WHERE id = 42 AND v = 0"), 0);
    // 复检通过后确实落盘
    match session.execute("SELECT v FROM w WHERE id = 42", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(8)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // 不存在的键：候选为空，一行也不动
    assert_eq!(affected(&mut session, "DELETE FROM w WHERE id = 999"), 0);
    assert_eq!(affected(&mut session, "DELETE FROM w WHERE id = 43"), 1);

    // DELETE 的 WHERE 里带 IN 子查询（E2 顺带打通：写语句现在也会展开子查询）
    assert_eq!(
        affected(&mut session, "DELETE FROM w WHERE id IN (SELECT id FROM w WHERE id < 3)"),
        2
    );

    // 访问方式要如实反映：主键等值 → 索引定位；无索引列 → 全表扫描
    fn plan(session: &mut Session, sql: &str) -> Vec<(String, String)> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Explain { rows, .. } => rows
                .into_iter()
                .map(|row| (row[0].as_text(), row[1].as_text()))
                .collect(),
            other => panic!("期望 EXPLAIN，得到 {:?}", other),
        }
    }
    let rows = plan(&mut session, "EXPLAIN UPDATE w SET v = 1 WHERE id = 2");
    let way = rows.iter().find(|(k, _)| k == "访问方式").map(|(_, v)| v.clone()).unwrap();
    assert!(way.contains("索引定位"), "{}", way);

    let rows = plan(&mut session, "EXPLAIN DELETE FROM w WHERE v = 0");
    let way = rows.iter().find(|(k, _)| k == "访问方式").map(|(_, v)| v.clone()).unwrap();
    assert!(way.contains("全表扫描"), "{}", way);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// EXPLAIN 必须反映真实决策：范围索引 / 复合索引前缀 / LIMIT 下推
#[test]
fn explain_matches_real_access_path() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("explainpath");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE e (id INT PRIMARY KEY, a INT, b INT, c TEXT)", &[])
        .unwrap();
    session
        .execute("INSERT INTO e VALUES (1,1,1,'x'),(2,1,2,'y'),(3,2,1,'z'),(4,3,3,'w')", &[])
        .unwrap();
    session.execute("CREATE INDEX ix_ab ON e(a, b)", &[]).unwrap();
    session.execute("CREATE INDEX ix_a ON e(a)", &[]).unwrap();

    fn plan(session: &mut Session, sql: &str) -> Vec<(String, String)> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Explain { rows, .. } => rows
                .into_iter()
                .map(|row| (row[0].as_text(), row[1].as_text()))
                .collect(),
            other => panic!("期望 EXPLAIN，得到 {:?}", other),
        }
    }
    fn field(rows: &[(String, String)], key: &str) -> String {
        rows.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    // 等值：两张索引都命中前导列 a，应选前导最长的复合索引
    let rows = plan(&mut session, "EXPLAIN SELECT * FROM e WHERE a = 1 AND b = 2");
    let way = field(&rows, "访问方式");
    assert!(way.contains("索引查找"), "{}", way);
    assert!(way.contains("ix_ab"), "应选前导列更长的复合索引：{}", way);

    // 只给前导列 → 复合索引前缀查找
    let rows = plan(&mut session, "EXPLAIN SELECT * FROM e WHERE a = 2");
    let way = field(&rows, "访问方式");
    assert!(way.contains("复合索引前缀查找"), "{}", way);

    // 范围条件（无等值可用）→ 范围索引
    let rows = plan(&mut session, "EXPLAIN SELECT * FROM e WHERE a > 2");
    let way = field(&rows, "访问方式");
    assert!(way.contains("范围索引"), "{}", way);

    // 无 ORDER BY 的 LIMIT → 下推提示
    let rows = plan(&mut session, "EXPLAIN SELECT * FROM e LIMIT 2");
    assert!(
        field(&rows, "LIMIT 下推").contains("2 行"),
        "应报告 LIMIT 下推：{:?}",
        field(&rows, "LIMIT 下推")
    );

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 复合索引：完整键精确命中，前导列前缀命中，结果与全表扫描一致
#[test]
fn composite_index_prefix_lookup() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("composite");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE m (id INT PRIMARY KEY, a INT, b INT, c TEXT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO m VALUES (1,1,1,'x'),(2,1,2,'y'),(3,1,2,'z'),(4,2,1,'w'),(5,2,2,'v')",
            &[],
        )
        .unwrap();
    session.execute("CREATE INDEX ix_ab ON m(a, b)", &[]).unwrap();

    fn ids(session: &mut Session, sql: &str) -> Vec<i64> {
        let mut out: Vec<i64> = match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows
                .into_iter()
                .map(|row| match &row[0] {
                    Value::Int(n) => *n,
                    other => panic!("期望 INT，得到 {:?}", other),
                })
                .collect(),
            other => panic!("期望 SELECT，得到 {:?}", other),
        };
        out.sort_unstable();
        out
    }

    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a = 1 AND b = 2"),
        vec![2, 3],
        "完整键（a=1,b=2）"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a = 1"),
        vec![1, 2, 3],
        "前缀（a=1）"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a = 2 AND b = 1"),
        vec![4]
    );

    // 对照：表达式用不上索引，走全表扫描，结果应一致
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a + 0 = 1"),
        vec![1, 2, 3],
        "a+0 用不上索引，走全表扫描"
    );

    // 前缀查找也要能看到本事务未提交的行（读己之写）
    session.execute("BEGIN", &[]).unwrap();
    session
        .execute("INSERT INTO m VALUES (6,1,9,'new')", &[])
        .unwrap();
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a = 1"),
        vec![1, 2, 3, 6],
        "未提交行应在前缀查找中可见"
    );
    session.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a = 1"),
        vec![1, 2, 3],
        "回滚后不可见"
    );

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ALTER TABLE：加列/删列/改名/改表名，数据与行数都要保住
#[test]
fn alter_table_add_drop_rename() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("alter");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE a (id INT PRIMARY KEY, n TEXT)", &[])
        .unwrap();
    session
        .execute("INSERT INTO a VALUES (1,'one'),(2,'two'),(3,'three')", &[])
        .unwrap();
    session.execute("CREATE INDEX ix_n ON a(n)", &[]).unwrap();

    // 加列：有默认值 → 旧行取默认值
    session
        .execute("ALTER TABLE a ADD COLUMN score INT DEFAULT 7", &[])
        .unwrap();
    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    assert_eq!(
        rows(&mut session, "SELECT DISTINCT score FROM a"),
        vec![vec![Value::Int(7)]]
    );

    // 加列：无默认值 → NULL
    session.execute("ALTER TABLE a ADD tag TEXT", &[]).unwrap();
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM a WHERE tag IS NULL"),
        vec![vec![Value::Int(3)]]
    );

    // 列改名：数据跟着走
    session.execute("ALTER TABLE a RENAME COLUMN n TO name", &[]).unwrap();
    let names: Vec<String> = rows(&mut session, "SELECT name FROM a ORDER BY id")
        .into_iter()
        .map(|row| row[0].as_text())
        .collect();
    assert_eq!(names, vec!["one", "two", "three"]);
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM a WHERE name = 'two'"),
        vec![vec![Value::Int(1)]],
        "改名后原索引仍然可用（按列名迁移）"
    );

    // 删列
    session.execute("ALTER TABLE a DROP COLUMN tag", &[]).unwrap();
    let columns: Vec<String> = match session.execute("DESCRIBE a", &[]).unwrap() {
        QueryResult::Describe { columns, .. } => {
            columns.into_iter().map(|c| c.name.clone()).collect()
        }
        other => panic!("期望 DESCRIBE，得到 {:?}", other),
    };
    assert!(!columns.iter().any(|c| c == "tag"), "tag 应已删除：{:?}", columns);
    assert_eq!(rows(&mut session, "SELECT COUNT(*) FROM a"), vec![vec![Value::Int(3)]]);

    // 表改名：数据在，旧名没了
    session.execute("ALTER TABLE a RENAME TO b", &[]).unwrap();
    assert!(session.execute("SELECT * FROM a", &[]).is_err(), "旧表名应失效");
    assert_eq!(rows(&mut session, "SELECT COUNT(*) FROM b"), vec![vec![Value::Int(3)]]);

    session.close().unwrap();

    // 重开后元数据与数据都要一致
    let mut reopened = Session::open(&dir).unwrap();
    assert_eq!(rows(&mut reopened, "SELECT COUNT(*) FROM b"), vec![vec![Value::Int(3)]]);
    assert_eq!(reopened.engine.table("b").unwrap().row_count, 3);
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ALTER TABLE 的拒绝路径：主键列、重名、NOT NULL 无默认值
#[test]
fn alter_table_rejects_unsafe_changes() {
    let dir = temp_dir("alterbad");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE c (id INT PRIMARY KEY, v INT)", &[])
        .unwrap();
    session.execute("INSERT INTO c VALUES (1,1)", &[]).unwrap();

    let drop_pk = session.execute("ALTER TABLE c DROP COLUMN id", &[]).unwrap_err();
    assert_eq!(drop_pk.code, "SYNTAX_ERROR", "{}", drop_pk.message);

    let dup = session.execute("ALTER TABLE c ADD COLUMN v INT", &[]).unwrap_err();
    assert_eq!(dup.code, "ALREADY_EXISTS", "{}", dup.message);

    let rename_dup = session
        .execute("ALTER TABLE c RENAME COLUMN v TO id", &[])
        .unwrap_err();
    assert_eq!(rename_dup.code, "ALREADY_EXISTS", "{}", rename_dup.message);

    // 已有数据时加 NOT NULL 且无默认值 → 拒绝（旧行的 NULL 无法满足约束）
    let not_null = session
        .execute("ALTER TABLE c ADD COLUMN must INT NOT NULL", &[])
        .unwrap_err();
    assert_eq!(not_null.code, "CONSTRAINT_VIOLATION", "{}", not_null.message);

    // 失败的 ALTER 不能破坏原表
    assert_eq!(
        match session.execute("SELECT COUNT(*) FROM c", &[]).unwrap() {
            littledb::sql::QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        },
        vec![vec![Value::Int(1)]]
    );

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 行数增量必须在提交后计入共享 row_count：
/// 旧实现是在 store.commit 之后才取 delta，而那时事务已被移除，
/// 拿到永远是空表 → 运行期 row_count 恒为 0（控制台 total 显示 0）。
#[test]
fn row_count_tracks_commits_and_rollbacks() {
    let dir = temp_dir("rowcount");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE c (id INT PRIMARY KEY, v INT)", &[])
        .unwrap();
    fn count(session: &Session, name: &str) -> u32 {
        session.engine.table(name).unwrap().row_count
    }
    assert_eq!(count(&session, "c"), 0);

    session.execute("INSERT INTO c VALUES (1,10),(2,20)", &[]).unwrap();
    assert_eq!(count(&session, "c"), 2, "隐式事务提交后行数应更新");

    session.execute("UPDATE c SET v = 99 WHERE id = 1", &[]).unwrap();
    assert_eq!(count(&session, "c"), 2, "UPDATE = 删 + 插，行数不变");

    session.execute("DELETE FROM c WHERE id = 2", &[]).unwrap();
    assert_eq!(count(&session, "c"), 1);

    session.execute("BEGIN", &[]).unwrap();
    session.execute("INSERT INTO c VALUES (3,30),(4,40)", &[]).unwrap();
    assert_eq!(count(&session, "c"), 1, "未提交的行不计入共享行数");
    session.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(count(&session, "c"), 1, "回滚不影响行数");

    session.execute("BEGIN", &[]).unwrap();
    session.execute("INSERT INTO c VALUES (5,50)", &[]).unwrap();
    session.execute("COMMIT", &[]).unwrap();
    assert_eq!(count(&session, "c"), 2);
    session.close().unwrap();

    // 重开后由扫描重建，必须与提交结果一致
    let mut reopened = Session::open(&dir).unwrap();
    assert_eq!(count(&reopened, "c"), 2);
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// LIMIT 下推（无 ORDER BY 时边扫边停）必须与「全量扫描后再裁剪」结果一致
#[test]
fn limit_pushdown_matches_full_scan() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("limitpush");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE nums (id INT PRIMARY KEY, score INT, tag TEXT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO nums VALUES (0,0,'t'),(1,1,'t'),(2,2,'t'),(3,3,'t'),(4,4,'t'),\
             (5,5,'t'),(6,6,'t'),(7,7,'t'),(8,8,'t'),(9,9,'t'),(10,10,'u'),(11,11,'u'),\
             (12,12,'u'),(13,13,'u'),(14,14,'u'),(15,15,'u'),(16,16,'u'),(17,17,'u')",
            &[],
        )
        .unwrap();

    let mut rows = |sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 全量结果作为基准，再手算「跳过 3 行取 5 行」
    let full = rows("SELECT id FROM nums WHERE score >= 10");
    assert_eq!(full.len(), 8);
    let expected: Vec<Vec<Value>> = full.into_iter().skip(3).take(5).collect();
    assert_eq!(rows("SELECT id FROM nums WHERE score >= 10 LIMIT 5 OFFSET 3"), expected);

    // 边界：LIMIT 0 / OFFSET 越界
    assert!(rows("SELECT id FROM nums LIMIT 0").is_empty());
    assert!(rows("SELECT id FROM nums OFFSET 100").is_empty());
    // LIMIT 大于剩余行数
    assert_eq!(rows("SELECT id FROM nums WHERE score >= 16 LIMIT 99").len(), 2);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非 NULL 但类型不可比 → 明确报 TYPE_ERROR（旧实现静默变 NULL，看起来像「查不到」）
#[test]
fn cross_type_comparison_reports_error() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("typecmp");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)", &[])
        .unwrap();
    session.execute("INSERT INTO items VALUES (5, 'five')", &[]).unwrap();

    let err = session.execute("SELECT * FROM items WHERE id = '5'", &[]).unwrap_err();
    assert_eq!(err.code, "TYPE_ERROR", "{}", err.message);
    assert!(err.message.contains("CAST"), "错误信息应提示加 CAST：{}", err.message);

    // 正确写法照常工作
    match session.execute("SELECT name FROM items WHERE id = 5", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Text("five".into())]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    match session.execute("SELECT name FROM items WHERE id = CAST('5' AS INT)", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    // NULL 参与比较仍是 NULL（三值逻辑不受影响）
    assert!(matches!(
        session.execute("SELECT * FROM items WHERE id = NULL", &[]).unwrap(),
        QueryResult::Select { rows, .. } if rows.is_empty()
    ));

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `IN (SELECT ...)` 对 DECIMAL 列必须命中（旧实现把 DECIMAL 转成 TEXT，恒定不命中）
#[test]
fn in_subquery_with_decimal() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("indec");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE goods (id INT PRIMARY KEY, price DECIMAL(10,2))", &[])
        .unwrap();
    session
        .execute("CREATE TABLE picks (id INT PRIMARY KEY, price DECIMAL(10,2))", &[])
        .unwrap();
    session
        .execute("INSERT INTO goods VALUES (1,'19.99'),(2,'5.00'),(3,'0.01')", &[])
        .unwrap();
    session
        .execute("INSERT INTO picks VALUES (1,'19.99')", &[])
        .unwrap();

    match session
        .execute("SELECT id FROM goods WHERE price IN (SELECT price FROM picks)", &[])
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// CAST 语法
#[test]
fn cast_syntax() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("cast");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE items (id INT PRIMARY KEY, price DECIMAL(10,2), qty INT)", &[])
        .unwrap();
    session
        .execute("INSERT INTO items VALUES (1, '19.99', 3)", &[])
        .unwrap();

    let mut first = |sql: &str| -> Value {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows[0][0].clone(),
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    // DECIMAL 精确转换（类型化值，非文本）
    assert_eq!(
        first("SELECT CAST('123.45' AS DECIMAL(10,2))"),
        Value::Decimal { v: 12345, scale: 2 }
    );
    // INT → TEXT
    assert_eq!(first("SELECT CAST(42 AS TEXT)"), Value::Text("42".into()));
    // REAL → INT
    assert_eq!(first("SELECT CAST(3.7 AS INT)"), Value::Int(3));
    // 表列上的 CAST
    assert_eq!(
        first("SELECT CAST(price * qty AS TEXT) FROM items WHERE id = 1"),
        Value::Text("59.97".into())
    );
    // NULL 传播
    assert_eq!(first("SELECT CAST(NULL AS INT)"), Value::Null);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
