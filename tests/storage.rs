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

        let ids = engine.find_by_equality("players", "id", &Value::Int(7)).unwrap();
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

    let ids = engine.find_by_equality("players", "name", &Value::Text("new".to_string())).unwrap();
    assert_eq!(ids.as_ref().map(|v| v.len()), Some(1));
    assert!(engine
        .find_by_equality("players", "name", &Value::Text("old".to_string()))
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

/// 其它会话不能看到未提交事务的行（曾经是脏读：READ UNCOMMITTED）
#[test]
fn cross_session_reads_see_only_committed() {
    use littledb::sql::executor::ConnSession;
    use littledb::sql::QueryResult;
    use std::sync::{Arc, Mutex};

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

    let engine = Arc::new(Mutex::new(Engine::open(&dir).unwrap()));
    let mut writer = ConnSession::new(Arc::clone(&engine));
    let mut reader = littledb::sql::executor::ConnSession::new(Arc::clone(&engine));

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
    // 未提交期间，其它会话的写会被正确拒绝（全局单写锁语义保持不变）
    assert!(reader.execute("INSERT INTO players VALUES (3, 'blocked', 0)", &[]).is_err());

    writer.execute("COMMIT", &[]).unwrap();
    // 提交之后立即可见
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
    let mut engine = Mutex::into_inner(engine).unwrap();
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
