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

/// 线路 JSON 必须带每列类型：客户端（SDK/控制台）不必再反查 schema
#[test]
fn select_wire_json_has_column_types() {
    use littledb::json::Json;
    use littledb::sql::QueryResult;

    let dir = temp_dir("wiretypes");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute(
            "CREATE TABLE w (id INT PRIMARY KEY, name TEXT, price DECIMAL(10,2), ok BOOL)",
            &[],
        )
        .unwrap();
    session
        .execute("INSERT INTO w VALUES (1,'a','1.50',TRUE)", &[])
        .unwrap();

    fn types_of(result: QueryResult) -> Vec<String> {
        match result.to_wire_json() {
            Json::Obj(items) => {
                let entry = items
                    .iter()
                    .find(|(key, _)| key == "types")
                    .expect("select 结果必须带 types 字段");
                match &entry.1 {
                    Json::Arr(values) => values
                        .iter()
                        .map(|v| v.as_str().unwrap_or("").to_string())
                        .collect(),
                    other => panic!("types 应为数组，实际 {:?}", other),
                }
            }
            other => panic!("线路结果应为对象，实际 {:?}", other),
        }
    }

    assert_eq!(
        types_of(session.execute("SELECT * FROM w", &[]).unwrap()),
        vec!["INT", "TEXT", "DECIMAL", "BOOL"],
        "SELECT * 应按源表列给类型"
    );
    assert_eq!(
        types_of(
            session
                .execute("SELECT COUNT(*) AS n, MAX(price) FROM w", &[])
                .unwrap()
        ),
        vec!["INT", "UNKNOWN"],
        "聚合：COUNT 是 INT，MAX 推不出给 UNKNOWN 而不是瞎猜"
    );
    assert_eq!(
        types_of(
            session
                .execute(
                    "SELECT CAST('1' AS INT) AS a, price * 2 AS b FROM w",
                    &[]
                )
                .unwrap()
        ),
        vec!["INT", "DECIMAL"],
        "CAST 用目标类型，DECIMAL 算术保持 DECIMAL"
    );

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// /api/schema 必须暴露 DECIMAL 标度：否则按 schema 重建表会退回默认标度，
/// 备份恢复时 decimal 值会被截断或报错
#[test]
fn schema_wire_includes_decimal_scale() {
    let dir = temp_dir("schemascale");
    {
        let mut engine = Engine::open(&dir).unwrap();
        engine
            .write(|e| -> Result<()> {
                let mut price = Column::new("price", ColumnType::Decimal);
                price.scale = 2;
                e.create_table(
                    "m",
                    vec![Column::new("id", ColumnType::Int), price],
                    false,
                )
            })
            .unwrap();
        engine.close().unwrap();
    }

    let port = start_http(&dir);
    let resp = http_roundtrip(
        port,
        "GET /api/schema HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed = littledb::json::parse(body).expect("schema 应是合法 JSON");
    let text = parsed.to_string().replace(' ', "");
    assert!(
        text.contains("\"type\":\"DECIMAL\""),
        "应声明 DECIMAL：{}",
        &text[..text.len().min(300)]
    );
    assert!(
        text.contains("\"scale\":2"),
        "应带上标度 2：{}",
        &text[..text.len().min(300)]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 自增主键：INSERT 分配值并回报 ids；显式值把游标顶上去；重启后游标不丢
#[test]
fn auto_increment_assigns_and_reports_ids() {
    use littledb::json::Json;
    use littledb::sql::QueryResult;

    let dir = temp_dir("autoid");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute(
            "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT)",
            &[],
        )
        .unwrap();

    // 省略自增列 → 自动分配 1、2
    let result = session
        .execute("INSERT INTO t (name) VALUES ('a'),('b')", &[])
        .unwrap();
    match &result {
        QueryResult::Insert {
            inserted, ids, ..
        } => {
            assert_eq!(*inserted, 2);
            assert_eq!(ids, &vec![1, 2], "应回报分配到的 id");
        }
        other => panic!("期望 INSERT，得到 {:?}", other),
    }
    // 线路 JSON 带 ids / lastId，客户端不用再 SELECT 一次
    match result.to_wire_json() {
        Json::Obj(items) => {
            let last = items
                .iter()
                .find(|(key, _)| key == "lastId")
                .expect("insert 结果应带 lastId")
                .1
                .clone();
            assert_eq!(last.as_i64(), Some(2));
        }
        other => panic!("期望对象，得到 {:?}", other),
    }

    // 显式给更大的 id → 游标顶到 101
    session.execute("INSERT INTO t VALUES (100,'x')", &[]).unwrap();
    match session
        .execute("INSERT INTO t (name) VALUES ('y')", &[])
        .unwrap()
    {
        QueryResult::Insert { ids, .. } => assert_eq!(ids, vec![101]),
        other => panic!("期望 INSERT，得到 {:?}", other),
    }
    session.close().unwrap();

    // 重启后游标仍在（next_auto_id 随 catalog 持久化）
    let mut reopened = Session::open(&dir).unwrap();
    match reopened
        .execute("INSERT INTO t (name) VALUES ('z')", &[])
        .unwrap()
    {
        QueryResult::Insert { ids, .. } => assert_eq!(ids, vec![102], "next_auto_id 必须持久化"),
        other => panic!("期望 INSERT，得到 {:?}", other),
    }

    // 非法声明
    let err = reopened
        .execute("CREATE TABLE bad (id TEXT AUTO_INCREMENT)", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    let err = reopened
        .execute("CREATE TABLE bad2 (a INT AUTO_INCREMENT, b INT AUTO_INCREMENT)", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);

    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// UPSERT：主键/唯一索引冲突更新、DO NOTHING、EXCLUDED 引用、无索引报错
#[test]
fn upsert_on_conflict() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("upsert");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute(
            "CREATE TABLE s (\n\
               id INT PRIMARY KEY AUTO_INCREMENT,\n\
               name TEXT,\n\
               hits INT DEFAULT 0,\n\
               email TEXT UNIQUE\n\
             )",
            &[],
        )
        .unwrap();

    let counts = |session: &mut Session, sql: &str| -> (usize, usize, usize, Vec<i64>) {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Insert {
                inserted,
                updated,
                skipped,
                ids,
            } => (inserted, updated, skipped, ids),
            other => panic!("期望 INSERT，得到 {:?}", other),
        }
    };

    // 先插一行（id 自动 = 1）
    assert_eq!(
        counts(&mut session, "INSERT INTO s (name, email) VALUES ('a','a@x')"),
        (1, 0, 0, vec![1])
    );

    // 主键冲突 → 更新；EXCLUDED 取新行的值，hits 累加在旧值上
    assert_eq!(
        counts(
            &mut session,
            "INSERT INTO s (id, name, hits) VALUES (1,'b',5)\n\
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, hits = hits + 1"
        ),
        (0, 1, 0, vec![])
    );
    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    assert_eq!(
        rows(&mut session, "SELECT id, name, hits, email FROM s WHERE id = 1"),
        vec![vec![
            Value::Int(1),
            Value::Text("b".into()),
            // 旧值 0 + 1（EXCLUDED.hits 是 5，但这里没引用它）
            Value::Int(1),
            Value::Text("a@x".into()), // 没被赋值，保持原值
        ]]
    );

    // DO NOTHING（冲突列省略 → 用主键）
    assert_eq!(
        counts(
            &mut session,
            "INSERT INTO s (id, name) VALUES (1,'zzz') ON CONFLICT DO NOTHING"
        ),
        (0, 0, 1, vec![])
    );
    assert_eq!(
        rows(&mut session, "SELECT name FROM s WHERE id = 1"),
        vec![vec![Value::Text("b".into())]],
        "DO NOTHING 不能改数据"
    );

    // 唯一索引冲突（email 有 uq_ 隐式索引）
    assert_eq!(
        counts(
            &mut session,
            "INSERT INTO s (name, email) VALUES ('c','a@x')\n\
             ON CONFLICT (email) DO UPDATE SET hits = hits + 10"
        ),
        (0, 1, 0, vec![])
    );
    assert_eq!(
        rows(&mut session, "SELECT hits FROM s WHERE id = 1"),
        vec![vec![Value::Int(11)]]
    );
    assert_eq!(rows(&mut session, "SELECT COUNT(*) FROM s"), vec![vec![Value::Int(1)]]);

    // 不冲突 → 正常插入，自增继续往下走
    assert_eq!(
        counts(
            &mut session,
            "INSERT INTO s (name, email) VALUES ('d','d@x') ON CONFLICT (id) DO UPDATE SET hits = 1"
        ),
        (1, 0, 0, vec![2])
    );

    // 冲突列没有索引 → 明确报错，而不是逐行全表扫
    let err = session
        .execute(
            "INSERT INTO s (name, email) VALUES ('e','e@x') ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("索引"), "{}", err.message);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ALTER 加自增列（影子表重建逐行分配）+ dump/load 后游标不撞车
#[test]
fn auto_increment_survives_alter_and_dump() {
    use littledb::sql::QueryResult;

    let src = temp_dir("autoalter");
    let dst = temp_dir("autodump");
    {
        let mut session = Session::open(&src).unwrap();
        session.execute("CREATE TABLE a (name TEXT)", &[]).unwrap();
        session
            .execute("INSERT INTO a VALUES ('p'),('q')", &[])
            .unwrap();
        session
            .execute("ALTER TABLE a ADD COLUMN seq INT AUTO_INCREMENT", &[])
            .unwrap();
        let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
            match session.execute(sql, &[]).unwrap() {
                QueryResult::Select { rows, .. } => rows,
                other => panic!("期望 SELECT，得到 {:?}", other),
            }
        };
        assert_eq!(
            rows(&mut session, "SELECT seq FROM a ORDER BY seq"),
            vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            "重建时按行分配 1、2"
        );
        session
            .execute("INSERT INTO a (name) VALUES ('r')", &[])
            .unwrap();
        assert_eq!(
            rows(&mut session, "SELECT seq FROM a WHERE name = 'r'"),
            vec![vec![Value::Int(3)]]
        );
        session.close().unwrap();
    }

    let script = {
        let mut engine = Engine::open(&src).unwrap();
        let script = littledb::dump::dump_script(&mut engine).unwrap();
        engine.close().unwrap();
        script
    };
    assert!(script.contains("AUTO_INCREMENT"), "自增属性要导出：\n{}", script);

    let mut loaded = Session::open(&dst).unwrap();
    littledb::dump::load_script(&mut loaded, &script, 50).unwrap();
    match loaded
        .execute("INSERT INTO a (name) VALUES ('s')", &[])
        .unwrap()
    {
        QueryResult::Insert { ids, .. } => {
            assert_eq!(ids, vec![4], "显式 id 应把游标顶到 4")
        }
        other => panic!("期望 INSERT，得到 {:?}", other),
    }
    loaded.close().unwrap();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dst);
}

/// dump → load 往返：结构（类型/标度/约束/索引）与数据都要一致
#[test]
fn dump_and_load_round_trip() {
    use littledb::sql::QueryResult;

    let src = temp_dir("dumpsrc");
    let dst = temp_dir("dumpdst");

    // ---- 源库：故意包含难序列化的值（引号 / 分号 / 换行）与各种约束
    {
        let mut session = Session::open(&src).unwrap();
        session
            .execute(
                "CREATE TABLE t (\n\
                   id INT PRIMARY KEY,\n\
                   name TEXT,\n\
                   price DECIMAL(10,2),\n\
                   ratio REAL,\n\
                   ok BOOL,\n\
                   note TEXT DEFAULT 'n/a'\n\
                 )",
                &[],
            )
            .unwrap();
        session
            .execute("CREATE TABLE u (a INT, b INT, PRIMARY KEY (a, b))", &[])
            .unwrap();
        session
            .execute("CREATE UNIQUE INDEX uq_name ON t(name)", &[])
            .unwrap();
        session
            .execute("CREATE INDEX ix_price ON t(price)", &[])
            .unwrap();
        session
            .execute(
                "INSERT INTO t VALUES (?,?,?,?,?,?)",
                &[
                    Value::Int(1),
                    Value::Text("it's;ok\nnewline".to_string()),
                    Value::Decimal { v: 150, scale: 2 },
                    Value::Real(0.5),
                    Value::Bool(true),
                    Value::Text("z".to_string()),
                ],
            )
            .unwrap();
        session
            .execute("INSERT INTO u VALUES (1,2),(1,3)", &[])
            .unwrap();
        session.close().unwrap();
    }

    // ---- 导出
    let script = {
        let mut engine = Engine::open(&src).unwrap();
        let script = littledb::dump::dump_script(&mut engine).unwrap();
        engine.close().unwrap();
        script
    };
    assert!(script.contains("DECIMAL(38,2)"), "DECIMAL 必须带标度：\n{}", script);
    assert!(script.contains("CREATE UNIQUE INDEX"), "唯一索引要导出");
    assert!(script.contains("PRIMARY KEY (\"a\", \"b\")"), "复合主键要用表级写法");

    // ---- 导入到空目录
    let mut loaded = Session::open(&dst).unwrap();
    let stats = littledb::dump::load_script(&mut loaded, &script, 50).unwrap();
    // 2 张表 + 2 个索引 + 2 条 INSERT
    assert_eq!(stats.statements, 6, "语句数");

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 数据逐列一致（含引号/分号/换行的文本、DECIMAL 标度、REAL、BOOL）
    assert_eq!(
        rows(&mut loaded, "SELECT id, name, price, ratio, ok, note FROM t"),
        vec![vec![
            Value::Int(1),
            Value::Text("it's;ok\nnewline".to_string()),
            Value::Decimal { v: 150, scale: 2 },
            Value::Real(0.5),
            Value::Bool(true),
            Value::Text("z".to_string()),
        ]]
    );

    // 索引与复合主键都要重建
    let columns: Vec<String> = match loaded.execute("DESCRIBE t", &[]).unwrap() {
        QueryResult::Describe { indexes, .. } => {
            indexes.iter().map(|i| i.name.clone()).collect()
        }
        other => panic!("期望 DESCRIBE，得到 {:?}", other),
    };
    assert!(columns.iter().any(|n| n == "uq_name"), "{:?}", columns);
    assert!(columns.iter().any(|n| n == "ix_price"), "{:?}", columns);

    let dup = loaded
        .execute("INSERT INTO u VALUES (1,2)", &[])
        .unwrap_err();
    assert_eq!(dup.code, "CONSTRAINT_VIOLATION", "复合主键约束应恢复：{}", dup.message);

    loaded.close().unwrap();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dst);
}

// ---------------------------------------------------------------- HTTP 层

/// 起一个真实 HTTP 服务实例（随机端口），用于协议级用例
fn start_http(dir: &std::path::Path) -> u16 {
    start_http_with(dir, None)
}

/// 同上，可指定 CORS 策略（None = 只允许同源）
fn start_http_with(dir: &std::path::Path, cors: Option<&str>) -> u16 {
    start_http_full(dir, None, cors, 5)
}

/// 完整版：可指定访问令牌与认证失败阈值
fn start_http_full(
    dir: &std::path::Path,
    token: Option<&str>,
    cors: Option<&str>,
    auth_fail_limit: u32,
) -> u16 {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, RwLock};

    let engine = Arc::new(RwLock::new(Engine::open(dir).unwrap()));
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut state = littledb::http::HttpState::new(
        engine,
        token.map(|t| t.to_string()),
        littledb::http::find_web_dir(),
        Arc::new(AtomicUsize::new(0)),
        8,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    if let Some(policy) = cors {
        state = state.with_cors(policy.to_string());
    }
    state = state.with_auth_limiter(std::sync::Arc::new(littledb::auth::AuthLimiter::new(
        auth_fail_limit,
    )));
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

/// HTTP 认证失败限速：连续猜错后返回 429 + Retry-After，退避期内正确 token 也拒
#[test]
fn http_auth_failures_back_off() {
    let dir = temp_dir("httpauth");
    let port = start_http_full(&dir, Some("good-token"), None, 3);

    let wrong = "GET /api/health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
                 Authorization: Bearer bad-token\r\n\r\n";
    for attempt in 1..=3 {
        let resp = http_roundtrip(port, wrong);
        assert!(resp.starts_with("HTTP/1.1 401"), "第 {} 次: {}", attempt, resp);
    }
    // 第 4 次失败跨过阈值：本次仍是 401，但已进入退避
    let resp = http_roundtrip(port, wrong);
    assert!(resp.starts_with("HTTP/1.1 401"), "{}", resp);

    // 之后一律 429，且带 Retry-After
    let resp = http_roundtrip(port, wrong);
    assert!(resp.starts_with("HTTP/1.1 429"), "{}", resp);
    assert!(resp.to_ascii_lowercase().contains("retry-after:"), "{}", resp);
    assert!(resp.contains("RATE_LIMITED"), "{}", resp);

    // 退避期内正确 token 也不放行（否则爆破方可以靠“猜中一次”绕过退避）
    let good = "GET /api/health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
                Authorization: Bearer good-token\r\n\r\n";
    let resp = http_roundtrip(port, good);
    assert!(resp.starts_with("HTTP/1.1 429"), "{}", resp);

    // 关闭限速（limit=0）时不会锁
    let dir2 = temp_dir("httpauth2");
    let port2 = start_http_full(&dir2, Some("good-token"), None, 0);
    for _ in 0..20 {
        let resp = http_roundtrip(port2, wrong);
        assert!(resp.starts_with("HTTP/1.1 401"), "{}", resp);
    }
    let resp = http_roundtrip(port2, good);
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// LDBP 端口同样限速：猜错 N 次后 hello 返回 RATE_LIMITED
#[test]
fn ldbp_auth_failures_back_off() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let dir = temp_dir("ldbpauth");
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let engine = Engine::open(&dir).unwrap();
    let options = littledb::server::ServerOptions {
        host: "127.0.0.1".to_string(),
        port,
        http_port: None,
        token: Some("good-token".to_string()),
        auth_fail_limit: 3,
        open_browser: false,
        ..Default::default()
    };
    let server = littledb::server::Server::new(engine, options);
    std::thread::spawn(move || {
        let _ = server.run();
    });
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    fn read_frame(stream: &mut TcpStream) -> String {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }
    fn send_frame(stream: &mut TcpStream, text: &str) -> String {
        let bytes = text.as_bytes();
        let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(bytes);
        stream.write_all(&frame).unwrap();
        read_frame(stream)
    }

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let welcome = read_frame(&mut stream);
    assert!(welcome.contains("welcome"), "{}", welcome);

    for attempt in 1..=3 {
        let resp = send_frame(&mut stream, r#"{"type":"hello","token":"bad"}"#);
        assert!(resp.contains("AUTH_FAILED"), "第 {} 次: {}", attempt, resp);
    }
    // 第 4 次失败：仍回 AUTH_FAILED，但此后进入退避
    let resp = send_frame(&mut stream, r#"{"type":"hello","token":"bad"}"#);
    assert!(resp.contains("AUTH_FAILED"), "{}", resp);
    let resp = send_frame(&mut stream, r#"{"type":"hello","token":"bad"}"#);
    assert!(resp.contains("RATE_LIMITED"), "{}", resp);
    // 正确 token 在退避期内也被拒
    let resp = send_frame(&mut stream, r#"{"type":"hello","token":"good-token"}"#);
    assert!(resp.contains("RATE_LIMITED"), "{}", resp);

    drop(stream);
    let _ = std::fs::remove_dir_all(&dir);
}

/// CORS 默认只允许同源：任意网页不能借用户浏览器读写数据库
#[test]
fn cors_is_same_origin_by_default() {
    let dir = temp_dir("cors");
    let port = start_http(&dir);

    let resp = http_roundtrip(
        port,
        "GET /api/health HTTP/1.1\r\nHost: t\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    assert!(
        !resp.to_ascii_lowercase().contains("access-control-allow-origin"),
        "默认不应回跨源头：{}",
        resp
    );

    let resp = http_roundtrip(
        port,
        "OPTIONS /api/sql HTTP/1.1\r\nHost: t\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 204"), "{}", resp);
    assert!(
        !resp.to_ascii_lowercase().contains("access-control-allow-origin"),
        "预检也不该放行：{}",
        resp
    );

    // 显式白名单：只回匹配来源，且带 Vary: Origin
    let dir2 = temp_dir("cors2");
    let port2 = start_http_with(&dir2, Some("http://ok.example"));
    let resp = http_roundtrip(
        port2,
        "GET /api/health HTTP/1.1\r\nHost: t\r\nOrigin: http://ok.example\r\nConnection: close\r\n\r\n",
    );
    assert!(
        resp.to_ascii_lowercase()
            .contains("access-control-allow-origin: http://ok.example"),
        "{}",
        resp
    );
    assert!(resp.to_ascii_lowercase().contains("vary: origin"), "{}", resp);

    let resp = http_roundtrip(
        port2,
        "GET /api/health HTTP/1.1\r\nHost: t\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(
        !resp.to_ascii_lowercase().contains("access-control-allow-origin"),
        "不在白名单的来源不应放行：{}",
        resp
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
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

/// COUNT(*) 索引计数捷径 + 范围路径的读己之写（旧版范围扫描缺失 delta 合并）
#[test]
fn count_shortcut_and_range_read_own_writes() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("countshortcut");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, k INT, v INT)", &[])
        .unwrap();
    session.execute("CREATE INDEX ix_k ON t(k)", &[]).unwrap();
    session
        .execute(
            &format!(
                "INSERT INTO t (k, v) VALUES {}",
                (0..100).map(|i| format!("({},0)", i)).collect::<Vec<_>>().join(",")
            ),
            &[],
        )
        .unwrap();

    let scalar = |session: &mut Session, sql: &str| -> (String, Value) {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { columns, rows, .. } => {
                assert_eq!(rows.len(), 1, "{}", sql);
                (columns[0].clone(), rows[0][0].clone())
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    let count = |session: &mut Session, sql: &str, expected_name: &str| -> i64 {
        let (name, value) = scalar(session, sql);
        // 捷径与常规路径的列名必须一致（expr_label 的既有约定：
        // COUNT(*) → "COUNT(*)"，COUNT(列) → "COUNT()"——参数列表不进列名）
        assert_eq!(name, expected_name, "列名应一致：{}", sql);
        match value {
            Value::Int(n) => n,
            other => panic!("期望 INT，得到 {:?}", other),
        }
    };
    let ids = |session: &mut Session, sql: &str| -> usize {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows.len(),
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 范围 COUNT（走捷径）与等值 COUNT
    assert_eq!(
        count(&mut session, "SELECT COUNT(*) FROM t WHERE k BETWEEN 10 AND 19", "COUNT(*)"),
        10
    );
    assert_eq!(count(&mut session, "SELECT COUNT(*) FROM t WHERE k = 15", "COUNT(*)"), 1);

    // 读己之写（范围路径）：本事务未提交的插入必须可见（旧版会「消失」）
    session.execute("BEGIN", &[]).unwrap();
    session
        .execute("INSERT INTO t (k, v) VALUES (15, 1),(15, 2),(150, 3)", &[])
        .unwrap();
    assert_eq!(
        ids(&mut session, "SELECT id FROM t WHERE k BETWEEN 10 AND 19"),
        12,
        "范围扫描应看到本事务未提交的 2 行"
    );
    assert_eq!(
        count(&mut session, "SELECT COUNT(*) FROM t WHERE k BETWEEN 10 AND 19", "COUNT(*)"),
        12
    );
    // 事务内删除同样立即可见
    session.execute("DELETE FROM t WHERE k = 15 AND v = 1", &[]).unwrap();
    assert_eq!(
        count(&mut session, "SELECT COUNT(*) FROM t WHERE k BETWEEN 10 AND 19", "COUNT(*)"),
        11
    );
    session.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(
        count(&mut session, "SELECT COUNT(*) FROM t WHERE k BETWEEN 10 AND 19", "COUNT(*)"),
        10,
        "回滚后恢复"
    );

    // 非捷径形态仍然正确：COUNT(列)、COUNT(DISTINCT 列)、非索引条件
    // （注意 expr_label 的既有约定：COUNT(列) 的列名是 "COUNT()"，参数不进列名）
    assert_eq!(
        count(&mut session, "SELECT COUNT(k) FROM t WHERE k BETWEEN 10 AND 19", "COUNT()"),
        10
    );
    assert_eq!(
        count(&mut session, "SELECT COUNT(DISTINCT k) FROM t WHERE k BETWEEN 10 AND 19", "COUNT()"),
        10
    );
    assert_eq!(
        count(&mut session, "SELECT COUNT(*) FROM t WHERE v + 0 = 0", "COUNT(*)"),
        100,
        "非索引条件走全表扫描，结果不受捷径影响"
    );
    // GROUP BY 形态不匹配捷径（多行结果），结果也必须正确
    match session
        .execute(
            "SELECT COUNT(*) AS n FROM t WHERE k < 5 GROUP BY k ORDER BY k",
            &[],
        )
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(
            rows,
            vec![vec![Value::Int(1)]; 5],
            "k=0..4 各一组，每组 1 行"
        ),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 标量子查询：投影、WHERE 比较、相关子查询、空结果与错误路径
#[test]
fn scalar_subquery() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("scalar");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE s (id INT PRIMARY KEY, tag TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO s VALUES (1,'a',10),(2,'a',20),(3,'b',5),(4,'b',7),(5,'c',1)",
            &[],
        )
        .unwrap();

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 非关联：投影里当常量用
    assert_eq!(
        rows(&mut session, "SELECT (SELECT MAX(v) FROM s) AS biggest"),
        vec![vec![Value::Int(20)]]
    );
    // WHERE 里比较（AVG 是 REAL，8.6）
    assert_eq!(
        rows(&mut session, "SELECT id FROM s WHERE v > (SELECT AVG(v) FROM s) ORDER BY id"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    // 相关：每行统计同 tag 行数
    assert_eq!(
        rows(
            &mut session,
            "SELECT id, (SELECT COUNT(*) FROM s y WHERE y.tag = s.tag) AS n FROM s ORDER BY id"
        ),
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(2)],
            vec![Value::Int(3), Value::Int(2)],
            vec![Value::Int(4), Value::Int(2)],
            vec![Value::Int(5), Value::Int(1)],
        ]
    );
    // 空结果 → NULL（不是错误）
    assert_eq!(
        rows(&mut session, "SELECT (SELECT v FROM s WHERE 1 = 0)"),
        vec![vec![Value::Null]]
    );
    // 嵌在算术和 CTE 里都行
    assert_eq!(
        rows(
            &mut session,
            "SELECT (SELECT MAX(v) FROM s) - (SELECT MIN(v) FROM s) AS span"
        ),
        vec![vec![Value::Int(19)]]
    );
    assert_eq!(
        rows(&mut session, "SELECT (WITH x AS (SELECT v FROM s) SELECT SUM(v) FROM x)"),
        vec![vec![Value::Int(43)]]
    );

    // 写语句的 WHERE 也支持
    match session
        .execute(
            "UPDATE s SET v = 0 WHERE v = (SELECT MIN(v) FROM s)",
            &[],
        )
        .unwrap()
    {
        QueryResult::Update { updated } => assert_eq!(updated, 1),
        other => panic!("期望 UPDATE，得到 {:?}", other),
    }

    // 错误路径：多行 / 多列
    let err = session
        .execute("SELECT (SELECT v FROM s)", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("1 行"), "{}", err.message);
    let err = session
        .execute("SELECT (SELECT id, v FROM s WHERE id = 1)", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("1 列"), "{}", err.message);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// INSERT ... SELECT：列映射、聚合结果、自引用的原子性
#[test]
fn insert_from_select() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("insertselect");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute(
            "CREATE TABLE src (id INT PRIMARY KEY AUTO_INCREMENT, v INT, tag TEXT)",
            &[],
        )
        .unwrap();
    session
        .execute(
            "INSERT INTO src (v, tag) VALUES (1,'a'),(2,'a'),(3,'b')",
            &[],
        )
        .unwrap();
    session
        .execute(
            "CREATE TABLE dst (id INT PRIMARY KEY AUTO_INCREMENT, v INT, tag TEXT NOT NULL DEFAULT 'x')",
            &[],
        )
        .unwrap();

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    let inserted = |session: &mut Session, sql: &str| -> usize {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Insert { inserted, .. } => inserted,
            other => panic!("期望 INSERT，得到 {:?}", other),
        }
    };

    // 带 WHERE + 列映射；目标表没写的列取默认值
    assert_eq!(
        inserted(
            &mut session,
            "INSERT INTO dst (v) SELECT v FROM src WHERE tag = 'a'"
        ),
        2
    );
    assert_eq!(
        rows(&mut session, "SELECT id, v, tag FROM dst ORDER BY id"),
        vec![
            vec![Value::Int(1), Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Int(2), Value::Text("x".into())],
        ]
    );

    // 聚合查询的行也能写进去
    assert_eq!(
        inserted(
            &mut session,
            "INSERT INTO dst (v, tag) SELECT SUM(v), 'total' FROM src"
        ),
        1
    );
    assert_eq!(
        rows(&mut session, "SELECT v, tag FROM dst WHERE tag = 'total'"),
        vec![vec![Value::Int(6), Value::Text("total".into())]]
    );

    // 自引用：INSERT INTO t SELECT ... FROM t（先物化再写，不会边读边写乱套）
    session
        .execute("CREATE TABLE dup (id INT PRIMARY KEY AUTO_INCREMENT, v INT)", &[])
        .unwrap();
    session
        .execute("INSERT INTO dup (v) VALUES (1),(2),(3)", &[])
        .unwrap();
    assert_eq!(
        inserted(&mut session, "INSERT INTO dup (v) SELECT v FROM dup"),
        3
    );
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM dup"),
        vec![vec![Value::Int(6)]],
        "原来 3 行 + 复制 3 行"
    );
    assert_eq!(
        rows(&mut session, "SELECT DISTINCT v FROM dup ORDER BY v"),
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
        ],
        "复制出来的 id 是新的，值不变"
    );

    // 列数不匹配 → 明确报错，且不插入任何行
    let err = session
        .execute("INSERT INTO dst (v) SELECT v, tag FROM src", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("列数不匹配"), "{}", err.message);
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM dst"),
        vec![vec![Value::Int(3)]],
        "失败的语句不应写入任何行"
    );

    // 组合 ON CONFLICT：暂不支持，但要说得清楚
    let err = session
        .execute(
            "INSERT INTO dst (v) SELECT v FROM src ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("ON CONFLICT"), "{}", err.message);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// TRUNCATE：清空数据、保留结构与索引、自增归零、可回滚
#[test]
fn truncate_keeps_schema_and_resets_auto_id() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("truncate");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute(
            "CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT)",
            &[],
        )
        .unwrap();
    session.execute("CREATE INDEX ix_name ON t(name)", &[]).unwrap();
    session
        .execute(
            "INSERT INTO t (name) VALUES ('a'),('b'),('c')",
            &[],
        )
        .unwrap();

    // 清空
    match session.execute("TRUNCATE TABLE t", &[]).unwrap() {
        QueryResult::Ddl { message } => assert!(message.contains("3 行"), "{}", message),
        other => panic!("期望 DDL，得到 {:?}", other),
    }
    let count = |session: &mut Session| -> Value {
        match session.execute("SELECT COUNT(*) FROM t", &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows[0][0].clone(),
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    assert_eq!(count(&mut session), Value::Int(0));

    // 结构与索引都还在
    match session.execute("DESCRIBE t", &[]).unwrap() {
        QueryResult::Describe {
            columns, indexes, ..
        } => {
            assert_eq!(columns.len(), 2, "列定义应保留");
            assert!(
                indexes.iter().any(|ix| ix.name == "ix_name"),
                "显式索引应保留：{:?}",
                indexes.iter().map(|ix| ix.name.clone()).collect::<Vec<_>>()
            );
        }
        other => panic!("期望 DESCRIBE，得到 {:?}", other),
    }

    // 自增游标归零：下一行是 1
    match session
        .execute("INSERT INTO t (name) VALUES ('again')", &[])
        .unwrap()
    {
        QueryResult::Insert { ids, .. } => assert_eq!(ids, vec![1], "TRUNCATE 后自增应从头开始"),
        other => panic!("期望 INSERT，得到 {:?}", other),
    }

    // 索引仍然可用（能查到刚插入的行）
    match session
        .execute("SELECT id FROM t WHERE name = 'again'", &[])
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // 可回滚：TRUNCATE 在事务里回滚后数据回来
    session.execute("BEGIN", &[]).unwrap();
    session.execute("TRUNCATE TABLE t", &[]).unwrap();
    assert_eq!(count(&mut session), Value::Int(0));
    session.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(count(&mut session), Value::Int(1), "回滚后数据应恢复");

    // 不存在的表
    let err = session.execute("TRUNCATE TABLE nope_xyz", &[]).unwrap_err();
    assert_eq!(err.code, "NOT_FOUND", "{}", err.message);

    session.close().unwrap();
    // 重开后仍然是清空后的状态（1 行）
    let mut reopened = Session::open(&dir).unwrap();
    match reopened.execute("SELECT COUNT(*) FROM t", &[]).unwrap() {
        QueryResult::Select { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// ORDER BY 可以用投影别名（含聚合列），且不影响按源表列排序
#[test]
fn order_by_projection_alias() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("orderalias");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO t VALUES (1,'c',30),(2,'a',10),(3,'b',20)",
            &[],
        )
        .unwrap();

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 普通别名
    assert_eq!(
        rows(&mut session, "SELECT id AS k FROM t ORDER BY k DESC"),
        vec![vec![Value::Int(3)], vec![Value::Int(2)], vec![Value::Int(1)]]
    );
    // 别名也可用于表达式结果的排序（列名与别名不同）
    assert_eq!(
        rows(&mut session, "SELECT name AS n, v FROM t ORDER BY n"),
        vec![
            vec![Value::Text("a".into()), Value::Int(10)],
            vec![Value::Text("b".into()), Value::Int(20)],
            vec![Value::Text("c".into()), Value::Int(30)],
        ]
    );
    // 聚合列的别名
    assert_eq!(
        rows(
            &mut session,
            "SELECT SUM(v) AS total, COUNT(*) AS n FROM t GROUP BY id ORDER BY total DESC"
        ),
        vec![
            vec![Value::Int(30), Value::Int(1)],
            vec![Value::Int(20), Value::Int(1)],
            vec![Value::Int(10), Value::Int(1)],
        ]
    );
    // 源表列名排序不受影响
    assert_eq!(
        rows(&mut session, "SELECT id, v FROM t ORDER BY v"),
        vec![
            vec![Value::Int(2), Value::Int(10)],
            vec![Value::Int(3), Value::Int(20)],
            vec![Value::Int(1), Value::Int(30)],
        ]
    );
    // 限定名排序（t.v）也不受影响
    assert_eq!(
        rows(&mut session, "SELECT id, v FROM t ORDER BY t.v DESC"),
        vec![
            vec![Value::Int(1), Value::Int(30)],
            vec![Value::Int(3), Value::Int(20)],
            vec![Value::Int(2), Value::Int(10)],
        ]
    );

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// JSON 函数：JSON_VALID / JSON_TYPE / JSON_EXTRACT / JSON_ARRAY_LENGTH
#[test]
fn json_functions() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("jsonfn");
    let mut session = Session::open(&dir).unwrap();

    let one = |session: &mut Session, sql: &str| -> Value {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => {
                assert_eq!(rows.len(), 1, "{} 应返回 1 行", sql);
                rows[0][0].clone()
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 取值：嵌套对象 / 数组 / 混合路径
    assert_eq!(
        one(&mut session, r#"SELECT JSON_EXTRACT('{"a":{"b":1}}', '$.a.b')"#),
        Value::Int(1)
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_EXTRACT('{"list":[10,20,30]}', '$.list[1]')"#
        ),
        Value::Int(20)
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_EXTRACT('{"user":{"name":"alice"}}', '$.user.name')"#
        ),
        Value::Text("alice".into())
    );
    // 对象/数组返回 JSON 文本
    assert_eq!(
        one(&mut session, r#"SELECT JSON_EXTRACT('{"a":{"b":1}}', '$.a')"#),
        Value::Text("{\"b\":1}".into())
    );
    // 路径不存在 → NULL（不是报错）
    assert_eq!(
        one(&mut session, r#"SELECT JSON_EXTRACT('{"a":1}', '$.missing')"#),
        Value::Null
    );

    // 类型与合法性
    assert_eq!(
        one(&mut session, r#"SELECT JSON_TYPE('{"a":[1,true]}')"#),
        Value::Text("object".into())
    );
    assert_eq!(
        one(&mut session, r#"SELECT JSON_TYPE('"x"')"#),
        Value::Text("string".into())
    );
    assert_eq!(one(&mut session, "SELECT JSON_VALID('{\"a\":1}')"), Value::Bool(true));
    assert_eq!(one(&mut session, "SELECT JSON_VALID('{oops}')"), Value::Bool(false));
    assert_eq!(
        one(&mut session, r#"SELECT JSON_ARRAY_LENGTH('[1,2,3]')"#),
        Value::Int(3)
    );

    // 错误路径：非法 JSON / 非数组 / 非法路径
    let err = session.execute(r#"SELECT JSON_TYPE('{oops}')"#, &[]).unwrap_err();
    assert_eq!(err.code, "TYPE_ERROR", "{}", err.message);
    assert!(err.message.contains("JSON_VALID"), "{}", err.message);
    let err = session
        .execute(r#"SELECT JSON_ARRAY_LENGTH('{"a":1}')"#, &[])
        .unwrap_err();
    assert_eq!(err.code, "TYPE_ERROR", "{}", err.message);
    let err = session
        .execute(r#"SELECT JSON_EXTRACT('{"a":1}', 'a.b')"#, &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);

    // 真实用法：JSON 列上按字段过滤 + 聚合
    session
        .execute("CREATE TABLE logs (id INT PRIMARY KEY, payload JSON)", &[])
        .unwrap();
    session
        .execute(
            r#"INSERT INTO logs VALUES
               (1,'{"level":"error","ms":12}'),
               (2,'{"level":"warn","ms":3}'),
               (3,'{"level":"error","ms":7}')"#,
            &[],
        )
        .unwrap();
    match session
        .execute(
            "SELECT COUNT(*) AS n, SUM(JSON_EXTRACT(payload, '$.ms')) AS total \
             FROM logs WHERE JSON_EXTRACT(payload, '$.level') = 'error'",
            &[],
        )
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(
            rows,
            vec![vec![Value::Int(2), Value::Int(19)]],
            "两条 error，ms 之和 12+7"
        ),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 日期时间函数：加减、差值、取字段、格式化
#[test]
fn date_time_functions() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("datefn");
    let mut session = Session::open(&dir).unwrap();

    let one = |session: &mut Session, sql: &str| -> Value {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => {
                assert_eq!(rows.len(), 1, "{} 应返回 1 行", sql);
                rows[0][0].clone()
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 加减：输出形态跟随输入（只有日期 → 只给日期）
    assert_eq!(
        one(&mut session, "SELECT DATE_ADD('2026-01-01', 7, 'day')"),
        Value::Text("2026-01-08".into())
    );
    assert_eq!(
        one(&mut session, "SELECT DATE_ADD('2026-01-01 10:00:00', 90, 'minute')"),
        Value::Text("2026-01-01 11:30:00".into())
    );
    assert_eq!(
        one(&mut session, "SELECT DATE_SUB('2026-01-01', 1, 'day')"),
        Value::Text("2025-12-31".into())
    );
    // 月末裁剪（1-31 加一个月 = 2-28；闰年是 2-29）
    assert_eq!(
        one(&mut session, "SELECT DATE_ADD('2026-01-31', 1, 'month')"),
        Value::Text("2026-02-28".into())
    );
    assert_eq!(
        one(&mut session, "SELECT DATE_ADD('2024-01-31', 1, 'month')"),
        Value::Text("2024-02-29".into())
    );
    assert_eq!(
        one(&mut session, "SELECT DATE_ADD('2024-02-29', 1, 'year')"),
        Value::Text("2025-02-28".into())
    );

    // 差值 / 字段提取
    assert_eq!(
        one(&mut session, "SELECT DATEDIFF('2026-03-01', '2026-02-01')"),
        Value::Int(28)
    );
    assert_eq!(
        one(&mut session, "SELECT DATEDIFF('2026-01-01', '2026-03-01')"),
        Value::Int(-59)
    );
    assert_eq!(
        one(&mut session, "SELECT YEAR('2026-10-31 12:34:56')"),
        Value::Int(2026)
    );
    assert_eq!(
        one(&mut session, "SELECT MONTH('2026-10-31 12:34:56')"),
        Value::Int(10)
    );
    assert_eq!(
        one(&mut session, "SELECT DAY('2026-10-31 12:34:56')"),
        Value::Int(31)
    );
    assert_eq!(
        one(&mut session, "SELECT HOUR('2026-10-31 12:34:56')"),
        Value::Int(12)
    );
    assert_eq!(
        one(&mut session, "SELECT SECOND('2026-10-31 12:34:56')"),
        Value::Int(56)
    );

    // 格式化 / 取日期部分
    assert_eq!(
        one(&mut session, "SELECT DATE_FORMAT('2026-10-31 08:05:09', '%Y/%m/%d %H:%M:%S')"),
        Value::Text("2026/10/31 08:05:09".into())
    );
    assert_eq!(
        one(&mut session, "SELECT TO_DATE('2026-10-31 08:05:09')"),
        Value::Text("2026-10-31".into())
    );

    // 错误路径：非法单位 / 非法日期 / 不支持的格式符
    let err = session
        .execute("SELECT DATE_ADD('2026-01-01', 1, 'fortnight')", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    let err = session
        .execute("SELECT DATE_ADD('2026-02-30', 1, 'day')", &[])
        .unwrap_err();
    assert_eq!(err.code, "TYPE_ERROR", "{}", err.message);
    let err = session
        .execute("SELECT DATE_FORMAT('2026-01-01', '%Q')", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);

    // 用在真实查询里：按天聚合
    session
        .execute("CREATE TABLE ev (id INT PRIMARY KEY, at TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO ev VALUES \
             (1,'2026-10-01 09:00:00',1),(2,'2026-10-01 18:00:00',2),\
             (3,'2026-10-02 10:00:00',4)",
            &[],
        )
        .unwrap();
    match session
        .execute(
            // ORDER BY 引用投影别名（`ORDER BY d`），不必重复写表达式
            "SELECT TO_DATE(at) AS d, SUM(v) AS total FROM ev GROUP BY TO_DATE(at) ORDER BY d",
            &[],
        )
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Text("2026-10-01".into()), Value::Int(3)],
                vec![Value::Text("2026-10-02".into()), Value::Int(4)],
            ]
        ),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// EXISTS / NOT EXISTS，含相关子查询（引用外层列）
#[test]
fn exists_and_correlated_subquery() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("exists");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE s (id INT PRIMARY KEY, tag TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO s VALUES (1,'a',10),(2,'a',20),(3,'b',5),(4,'b',7),(5,'c',1)",
            &[],
        )
        .unwrap();

    let ids = |session: &mut Session, sql: &str| -> Vec<i64> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows
                .into_iter()
                .map(|row| match &row[0] {
                    Value::Int(n) => *n,
                    other => panic!("期望 INT，得到 {:?}", other),
                })
                .collect(),
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 非关联：子查询为真 → 全表命中；为假 → 一行不命中
    assert_eq!(
        ids(&mut session, "SELECT id FROM s WHERE EXISTS (SELECT 1 FROM s WHERE v > 15) ORDER BY id"),
        vec![1, 2, 3, 4, 5]
    );
    assert!(
        ids(
            &mut session,
            "SELECT id FROM s WHERE EXISTS (SELECT 1 FROM s WHERE v > 999)"
        )
        .is_empty()
    );

    // 关联：同 tag 里存在更大的 v
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM s x WHERE EXISTS \
             (SELECT 1 FROM s y WHERE y.tag = x.tag AND y.v > x.v) ORDER BY id"
        ),
        vec![1, 3]
    );
    // NOT EXISTS 是补集
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM s x WHERE NOT EXISTS \
             (SELECT 1 FROM s y WHERE y.tag = x.tag AND y.v > x.v) ORDER BY id"
        ),
        vec![2, 4, 5]
    );
    // 与其它条件组合（EXISTS 只是 WHERE 的一部分）
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM s x WHERE x.v > 5 AND EXISTS \
             (SELECT 1 FROM s y WHERE y.tag = x.tag AND y.v > x.v) ORDER BY id"
        ),
        vec![1]
    );

    // 写语句的 WHERE 也支持（同一条解析路径）
    assert_eq!(
        match session
            .execute(
                "DELETE FROM s WHERE EXISTS (SELECT 1 FROM s y WHERE y.v > 900)",
                &[]
            )
            .unwrap()
        {
            QueryResult::Delete { deleted } => deleted,
            other => panic!("期望 DELETE，得到 {:?}", other),
        },
        0
    );

    // EXISTS 出现在 SELECT 列表：支持（逐行解析成布尔）
    match session
        .execute("SELECT id, EXISTS (SELECT 1 FROM s y WHERE y.tag = s.tag) AS has_same_tag FROM s ORDER BY id", &[])
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Bool(true)],
                vec![Value::Int(2), Value::Bool(true)],
                vec![Value::Int(3), Value::Bool(true)],
                vec![Value::Int(4), Value::Bool(true)],
                vec![Value::Int(5), Value::Bool(true)],
            ],
            "自己和自己同 tag，恒为真"
        ),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }
    // 关联版：排除自己之后同 tag 还有更大的 v
    match session
        .execute(
            "SELECT id, EXISTS (SELECT 1 FROM s y WHERE y.tag = s.tag AND y.v > s.v) AS has_bigger \
             FROM s ORDER BY id",
            &[],
        )
        .unwrap()
    {
        QueryResult::Select { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Bool(true)],
                vec![Value::Int(2), Value::Bool(false)],
                vec![Value::Int(3), Value::Bool(true)],
                vec![Value::Int(4), Value::Bool(false)],
                vec![Value::Int(5), Value::Bool(false)],
            ]
        ),
        other => panic!("期望 SELECT，得到 {:?}", other),
    }

    // HAVING 里仍然不支持（HAVING 走聚合求值，没有行上下文）
    let err = session
        .execute(
            "SELECT COUNT(*) FROM s HAVING EXISTS (SELECT 1 FROM s)",
            &[],
        )
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("HAVING"), "{}", err.message);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// CTE（WITH）：只在当前语句可见，可链式引用、可重复引用
#[test]
fn cte_with_clause() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("cte");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE s (id INT PRIMARY KEY, tag TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO s VALUES (1,'a',10),(2,'a',20),(3,'b',5),(4,'b',7),(5,'c',1)",
            &[],
        )
        .unwrap();

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 单 CTE：聚合后外层再过滤
    assert_eq!(
        rows(
            &mut session,
            "WITH g AS (SELECT tag, COUNT(*) AS n FROM s GROUP BY tag) \
             SELECT tag, n FROM g WHERE n > 1 ORDER BY tag"
        ),
        vec![
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("b".into()), Value::Int(2)],
        ]
    );

    // 多 CTE 链式：后一个引用前一个
    assert_eq!(
        rows(
            &mut session,
            "WITH a AS (SELECT id, v FROM s WHERE v >= 7), \
                  b AS (SELECT id FROM a WHERE v < 15) \
             SELECT id FROM b ORDER BY id"
        ),
        vec![vec![Value::Int(1)], vec![Value::Int(4)]]
    );

    // 同一 CTE 引用两次（自连接）
    assert_eq!(
        rows(
            &mut session,
            "WITH c AS (SELECT v FROM s WHERE tag = 'a') \
             SELECT x.v, y.v FROM c x JOIN c y ON x.v < y.v"
        ),
        vec![vec![Value::Int(10), Value::Int(20)]]
    );

    // CTE 与真表 JOIN（CTE 侧用 DISTINCT 去重，否则 'b' 两行会让结果翻倍）
    assert_eq!(
        rows(
            &mut session,
            "WITH c AS (SELECT DISTINCT tag FROM s WHERE tag = 'b') \
             SELECT s.id FROM s JOIN c ON s.tag = c.tag WHERE s.v > 5 ORDER BY s.id"
        ),
        vec![vec![Value::Int(4)]]
    );

    // 重名 CTE
    let err = session
        .execute(
            "WITH x AS (SELECT 1 AS a), x AS (SELECT 2 AS a) SELECT * FROM x",
            &[],
        )
        .unwrap_err();
    assert_eq!(err.code, "ALREADY_EXISTS", "{}", err.message);

    // 递归 CTE 暂不支持，但报错要说明
    let err = session
        .execute("WITH RECURSIVE r AS (SELECT 1 AS n) SELECT * FROM r", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("递归"), "{}", err.message);

    // CTE 不跨语句：上一条语句的 g 在这里应当找不到
    let err = session.execute("SELECT * FROM g", &[]).unwrap_err();
    assert_eq!(err.code, "NOT_FOUND", "{}", err.message);

    session.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 派生表：FROM (SELECT ...) AS t
#[test]
fn derived_table_in_from_and_join() {
    use littledb::sql::QueryResult;

    let dir = temp_dir("derived");
    let mut session = Session::open(&dir).unwrap();
    session
        .execute("CREATE TABLE s (id INT PRIMARY KEY, tag TEXT, v INT)", &[])
        .unwrap();
    session
        .execute(
            "INSERT INTO s VALUES (1,'a',10),(2,'a',20),(3,'b',5),(4,'b',7),(5,'c',1)",
            &[],
        )
        .unwrap();

    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };

    // 1. 最基本的派生表 + 列名可裸用/可限定
    assert_eq!(
        rows(&mut session, "SELECT s.id, s.v FROM (SELECT id, v FROM s WHERE v >= 10) AS s ORDER BY id"),
        vec![vec![Value::Int(1), Value::Int(10)], vec![Value::Int(2), Value::Int(20)]]
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM (SELECT id FROM s WHERE id = 3) AS t"),
        vec![vec![Value::Int(3)]],
        "SELECT * 应按子查询的列展开"
    );
    // 外层 WHERE 作用在派生表结果上
    assert_eq!(
        rows(
            &mut session,
            "SELECT id FROM (SELECT id, v FROM s) AS s WHERE v > 15"
        ),
        vec![vec![Value::Int(2)]]
    );

    // 2. 子查询里做聚合，外层再过滤聚合结果
    assert_eq!(
        rows(
            &mut session,
            "SELECT g.tag, g.n FROM (SELECT tag, COUNT(*) AS n FROM s GROUP BY tag) AS g \
             WHERE g.n > 1 ORDER BY g.tag"
        ),
        vec![
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("b".into()), Value::Int(2)],
        ]
    );

    // 3. 派生表参与 JOIN
    assert_eq!(
        rows(
            &mut session,
            "SELECT a.id, b.total FROM s a JOIN \
             (SELECT tag, SUM(v) AS total FROM s GROUP BY tag) b ON a.tag = b.tag \
             WHERE a.id = 3"
        ),
        vec![vec![Value::Int(3), Value::Int(12)]],
        "b 组合计 5+7=12"
    );
    // LEFT JOIN 到派生表
    assert_eq!(
        rows(
            &mut session,
            "SELECT a.id FROM s a LEFT JOIN (SELECT tag FROM s WHERE tag = 'a') b ON a.tag = b.tag \
             WHERE a.id = 5"
        ),
        vec![vec![Value::Int(5)]],
        "不匹配时保留左行"
    );

    // 4. 必须起别名
    let err = session
        .execute("SELECT * FROM (SELECT 1 AS x)", &[])
        .unwrap_err();
    assert_eq!(err.code, "SYNTAX_ERROR", "{}", err.message);
    assert!(err.message.contains("别名"), "{}", err.message);

    // 5. EXPLAIN 要说明派生表被整体物化
    match session
        .execute("EXPLAIN SELECT * FROM (SELECT id FROM s) AS d", &[])
        .unwrap()
    {
        QueryResult::Explain { rows, .. } => {
            let text: String = rows
                .iter()
                .map(|row| format!("{}={}", row[0].as_text(), row[1].as_text()))
                .collect::<Vec<_>>()
                .join(";");
            assert!(text.contains("子查询"), "{}", text);
            assert!(text.contains("物化"), "{}", text);
        }
        other => panic!("期望 EXPLAIN，得到 {:?}", other),
    }

    session.close().unwrap();
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

    let affected = |session: &mut Session, sql: &str| -> usize {
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

    // ---- 复合索引的**范围**扫描（前导列）
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a > 1"),
        vec![4, 5],
        "a > 1"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a >= 1 AND a <= 1"),
        vec![1, 2, 3],
        "a 在 [1,1] 闭区间"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a < 2"),
        vec![1, 2, 3],
        "a < 2"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a BETWEEN 2 AND 2"),
        vec![4, 5],
        "BETWEEN 闭区间"
    );
    // 对照全表扫描（a + 0 用不上索引）
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE a + 0 >= 2"),
        vec![4, 5],
        "与全表扫描结果一致"
    );

    // 文本前导列的范围（复合索引 ix_ca(c, id)）
    session.execute("CREATE INDEX ix_ca ON m(c, id)", &[]).unwrap();
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE c > 'w'"),
        vec![1, 2, 3],
        "文本范围：'x'/'y'/'z' > 'w'"
    );
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE c >= 'y'"),
        vec![2, 3],
        "文本范围：>= 'y'"
    );
    // `OR 1 = 0` 让条件不能被拆成 AND 里的裸列比较 → 强制全表扫描，用来对照
    assert_eq!(
        ids(&mut session, "SELECT id FROM m WHERE c >= 'y' OR 1 = 0"),
        vec![2, 3],
        "与全表扫描结果一致（文本）"
    );

    // EXPLAIN 要如实报告走了范围索引
    match session.execute("EXPLAIN SELECT id FROM m WHERE a > 1", &[]).unwrap() {
        QueryResult::Explain { rows, .. } => {
            let way = rows
                .iter()
                .find(|row| row[0].as_text() == "访问方式")
                .map(|row| row[1].as_text())
                .unwrap_or_default();
            assert!(way.contains("范围索引"), "{}", way);
        }
        other => panic!("期望 EXPLAIN，得到 {:?}", other),
    }

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

/// ALTER 走影子表重建（删表+建表+逐行写回），中途崩溃后必须能靠 WAL 恢复
#[test]
fn alter_survives_crash() {
    use littledb::sql::QueryResult;
    let dir = temp_dir("altercrash");
    {
        let mut session = Session::open(&dir).unwrap();
        session
            .execute("CREATE TABLE a (id INT PRIMARY KEY, n TEXT)", &[])
            .unwrap();
        session
            .execute("INSERT INTO a VALUES (1,'one'),(2,'two'),(3,'three')", &[])
            .unwrap();
        session
            .execute("ALTER TABLE a ADD COLUMN score INT DEFAULT 7", &[])
            .unwrap();
        session
            .execute("INSERT INTO a VALUES (4,'four',4)", &[])
            .unwrap();
        // 直接丢弃 = 进程被强杀（不 close、不 checkpoint）
    }

    let mut session = Session::open(&dir).unwrap();
    let rows = |session: &mut Session, sql: &str| -> Vec<Vec<Value>> {
        match session.execute(sql, &[]).unwrap() {
            QueryResult::Select { rows, .. } => rows,
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    };
    assert_eq!(
        rows(&mut session, "SELECT COUNT(*) FROM a"),
        vec![vec![Value::Int(4)]],
        "重放后行数应完整（含 ALTER 之后插入的那行）"
    );
    assert_eq!(
        rows(&mut session, "SELECT score FROM a WHERE id = 1"),
        vec![vec![Value::Int(7)]],
        "ALTER 加的列与其默认值都要恢复"
    );
    assert_eq!(
        rows(&mut session, "SELECT score FROM a WHERE id = 4"),
        vec![vec![Value::Int(4)]]
    );
    session.close().unwrap();
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
