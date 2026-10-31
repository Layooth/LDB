// 应用接入示例：一个最小任务表
const { createClient } = require('./littledb.js');

const db = createClient(process.env.DEMO_URL, { token: process.env.DEMO_TOKEN });

(async () => {
  // 1) 建表（应用启动时跑一次，IF NOT EXISTS 保证幂等）
  await db.sql(`CREATE TABLE IF NOT EXISTS tasks (
      id INT PRIMARY KEY AUTO_INCREMENT,
      title TEXT NOT NULL,
      price DECIMAL(10,2),
      status TEXT DEFAULT 'todo',
      created DATETIME
  )`);

  // 2) 插入：拿回自增 id
  const a = await db.insert('tasks', [
    { title: '写周报', price: '19.99', created: '2026-11-01 09:00:00' },
    { title: '修 bug' },
  ]);
  console.log('插入 ->', a.inserted, '行, 新 id =', a.ids, ', lastId =', a.lastId);

  // 3) 查询：行字典 + 每列类型
  const rows = await db.rows('SELECT id, title, status, price FROM tasks ORDER BY id');
  console.log('查询 ->', JSON.stringify(rows));
  const meta = await db.query('SELECT id, price FROM tasks');
  console.log('类型 ->', meta.types.join(','));

  // 4) UPSERT：按主键有则更新无则插入
  const up = await db.upsert(
    'tasks',
    [{ id: a.lastId, title: '写周报（改）', status: 'done' }],
    ['id'],
    ['title', 'status']
  );
  console.log('upsert ->', up.updated, '行被更新');

  // 5) 事务：抛错自动回滚
  await db.transaction(async () => {
    await db.sql('INSERT INTO tasks (title) VALUES (?)', ['事务里插的']);
    await db.sql('UPDATE tasks SET status = ? WHERE id = ?', ['doing', a.ids[0]]);
  });
  console.log('事务后任务数 ->', await db.value('SELECT COUNT(*) FROM tasks'));

  // 6) 分页
  const page = await db.table('tasks', { limit: 2, offset: 1 });
  console.log('分页 -> 共', page.total, '行，本页', page.objects.length, '行');

  // 7) 错误处理：错误码 + HTTP 状态
  try {
    await db.rows('SELECT * FROM tasks WHERE id = ?', ['1']);
  } catch (e) {
    console.log('错误 ->', e.code, '/ HTTP', e.status, ':', e.message);
  }
})().catch((e) => {
  console.error('失败:', e.message);
  process.exit(1);
});
