// SDK 集成测试：需要一个正在运行的服务实例
//   LDB_BASE=http://127.0.0.1:7802 LDB_TOKEN=xxx node sdk/test.js
// 由 tools/verify.ps1 在冒烟阶段自动调用。
'use strict';

const assert = require('node:assert');
const { createClient, LittleDBError, quoteIdent } = require('./littledb.js');

const base = process.env.LDB_BASE || 'http://127.0.0.1:7802';
const token = process.env.LDB_TOKEN || undefined;
const table = 'sdk_' + Date.now().toString(36);
const t = quoteIdent(table);

function ok(label) {
  console.log('  [OK] ' + label);
}

(async () => {
  const db = createClient(base, { token });

  // 1. 健康检查
  const health = await db.health();
  assert.ok(health.stats, 'health 应带 stats');
  ok('health');

  // 2. 建表 + 批量插入（自增 id）
  await db.sql(
    `CREATE TABLE ${t} (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT, price DECIMAL(10,2), ok BOOL)`
  );
  const inserted = await db.insert(table, [
    { name: 'a', price: '1.50', ok: true },
    { name: 'b', price: '2.25', ok: false },
  ]);
  assert.equal(inserted.inserted, 2);
  assert.deepEqual(inserted.ids, [1, 2], '应回报自增 id');
  assert.equal(inserted.lastId, 2);
  ok('insert 批量 + 自增 id');

  // 3. 行对象化：DECIMAL 是字符串，BOOL 是布尔
  const rows = await db.rows(`SELECT id, name, price, ok FROM ${t} ORDER BY id`);
  assert.deepEqual(rows[0], { id: 1, name: 'a', price: '1.50', ok: true });
  assert.deepEqual(rows[1], { id: 2, name: 'b', price: '2.25', ok: false });
  ok('rows() 行对象 + DECIMAL 保真');

  // 4. 列类型
  const query = await db.query(`SELECT id, price FROM ${t} ORDER BY id`);
  assert.deepEqual(query.types, ['INT', 'DECIMAL'], 'types 应与列一一对应');
  ok('query() types');

  // 5. 标量与单行
  assert.equal(await db.value(`SELECT COUNT(*) AS n FROM ${t}`), 2);
  assert.equal((await db.row(`SELECT name FROM ${t} WHERE id = 2`)).name, 'b');
  ok('value() / row()');

  // 6. UPSERT（只更新 name，price 保持原值）
  const upserted = await db.upsert(table, [{ id: 1, name: 'a2' }], ['id'], ['name']);
  assert.equal(upserted.updated, 1);
  assert.equal(upserted.inserted, 0);
  const afterUpsert = await db.row(`SELECT name, price FROM ${t} WHERE id = 1`);
  assert.equal(afterUpsert.name, 'a2');
  assert.equal(afterUpsert.price, '1.50');
  ok('upsert');

  // 7. 事务回滚
  await assert.rejects(
    db.transaction(async () => {
      await db.sql(`INSERT INTO ${t} (name) VALUES ('rollback')`);
      throw new Error('boom');
    }),
    /boom/
  );
  assert.equal(await db.value(`SELECT COUNT(*) AS n FROM ${t}`), 2, '回滚后行数不变');
  ok('transaction 回滚');

  // 8. 事务提交（跨请求保持会话）
  const lastId = await db.transaction(async () => {
    const result = await db.sql(`INSERT INTO ${t} (name) VALUES ('tx2')`);
    return result.lastId;
  });
  assert.equal(typeof lastId, 'number');
  assert.equal(await db.value(`SELECT COUNT(*) AS n FROM ${t}`), 3);
  ok('transaction 提交');

  // 9. 错误码映射
  await assert.rejects(db.sql(`SELECT * FROM ${t} WHERE id = '1'`), (err) => {
    assert.ok(err instanceof LittleDBError, '应抛 LittleDBError');
    assert.equal(err.code, 'TYPE_ERROR', 'code: ' + err.code);
    assert.equal(err.status, 400, 'status: ' + err.status);
    return true;
  });
  await assert.rejects(db.sql('SELECT * FROM no_such_table_xyz'), (err) => {
    assert.equal(err.code, 'NOT_FOUND');
    assert.equal(err.status, 404);
    return true;
  });
  ok('错误码/状态码映射');

  // 10. schema / tables / 分页
  const schema = await db.schema();
  assert.ok(schema.tables.some((x) => x.name === table), 'schema 应包含新表');
  const page = await db.table(table, { limit: 1, offset: 1 });
  assert.equal(page.rows.length, 1);
  assert.equal(page.total, 3);
  assert.equal(page.objects.length, 1);
  assert.ok(page.schema.columns.length >= 4);
  ok('schema / table 分页');

  // 11. updateById
  const updated = await db.updateById(table, 2, { name: 'b2' });
  assert.equal(updated, 1);
  assert.equal((await db.row(`SELECT name FROM ${t} WHERE id = 2`)).name, 'b2');
  ok('updateById');

  // 12. 清理
  await db.dropTable(table);
  await assert.rejects(db.sql(`SELECT * FROM ${t}`), (err) => err.code === 'NOT_FOUND');
  ok('dropTable');

  console.log('SDK 测试通过：12 项');
})().catch((err) => {
  console.error('SDK 测试失败：' + (err && err.message ? err.message : err));
  if (err && err.stack) console.error(err.stack);
  process.exit(1);
});
