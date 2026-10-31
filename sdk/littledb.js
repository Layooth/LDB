/**
 * LittleDB JS 客户端（浏览器 / Node 18+ 通用，零依赖）。
 *
 * 为什么需要它：服务端只给了 HTTP API，直接用 fetch 要自己处理
 * `X-Session-Id` 跨请求保持、错误码 → 状态码映射、`429/503` 的 `Retry-After`、
 * `DECIMAL` 是字符串而不是数字、结果只有列名没有列类型。
 * 这些都在这里封好了。
 *
 * ```js
 * // Node
 * const { createClient } = require('./sdk/littledb.js');
 * // 浏览器
 * // <script src="littledb.js"></script>  →  window.LittleDB.createClient
 *
 * const db = createClient('http://127.0.0.1:7802', { token: 'xxx' });
 *
 * await db.sql('CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT)');
 * const ins = await db.insert('t', [{ name: 'a' }, { name: 'b' }]);
 * ins.ids;                  // [1, 2]（自增分配值）
 * const rows = await db.rows('SELECT id, name FROM t ORDER BY id');
 * rows[0];                  // { id: 1, name: 'a' }
 *
 * await db.transaction(async () => {
 *   await db.sql("INSERT INTO t (name) VALUES ('c')");
 * });                       // 抛错自动 ROLLBACK
 *
 * await db.upsert('t', [{ id: 1, name: 'a2' }], ['id'], ['name']);
 * ```
 *
 * 约定：
 * - `DECIMAL` 一律以**字符串**返回（金额不经过浮点），`types` 会告诉你是 `"DECIMAL"`；
 * - 没有自增列时 `insert()` 的 `ids` 是空数组、`lastId` 是 `null`；
 * - 服务端的错误码原样挂在 `LittleDBError#code` 上（`TYPE_ERROR`/`NOT_FOUND`/`WRITE_CONFLICT`…），
 *   HTTP 状态在 `#status`，`429/503` 的等待毫秒数在 `#retryAfterMs`。
 */

(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory();
  } else {
    root.LittleDB = factory();
  }
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  /** 服务端返回的错误（`ok:false`）或网络层错误 */
  class LittleDBError extends Error {
    constructor(message, options) {
      super(message);
      this.name = 'LittleDBError';
      const opts = options || {};
      /** 服务端错误码，如 TYPE_ERROR / NOT_FOUND / WRITE_CONFLICT / RATE_LIMITED */
      this.code = opts.code || 'CLIENT_ERROR';
      /** HTTP 状态码（网络错误为 0） */
      this.status = opts.status || 0;
      /** 429/503 时建议的重试等待毫秒数 */
      this.retryAfterMs = opts.retryAfterMs || 0;
    }

    /** 是否值得重试（写冲突 / 限速 / 会话忙） */
    get retryable() {
      return (
        this.code === 'WRITE_CONFLICT' ||
        this.code === 'RATE_LIMITED' ||
        this.code === 'SESSION_BUSY'
      );
    }
  }

  /** 标识符加双引号（内部 " 翻倍），与服务端词法器一致 */
  function quoteIdent(name) {
    return '"' + String(name).replace(/"/g, '""') + '"';
  }

  /** 值 → 参数数组元素（Date 转服务端认的文本格式） */
  function toParam(value) {
    if (value === undefined || value === null) return null;
    if (value instanceof Date) {
      const pad = (n) => String(n).padStart(2, '0');
      return (
        value.getFullYear() +
        '-' + pad(value.getMonth() + 1) +
        '-' + pad(value.getDate()) +
        ' ' + pad(value.getHours()) +
        ':' + pad(value.getMinutes()) +
        ':' + pad(value.getSeconds())
      );
    }
    return value;
  }

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  class LittleDB {
    /**
     * @param {string} baseUrl 例如 http://127.0.0.1:7802
     * @param {{token?: string, sessionId?: string, fetch?: Function}} [options]
     */
    constructor(baseUrl, options) {
      const opts = options || {};
      this.baseUrl = String(baseUrl || 'http://127.0.0.1:7802').replace(/\/+$/, '');
      this.token = opts.token || null;
      /** 会话 id：首次请求后由服务端回填，后续请求自动携带（事务才能跨请求保持） */
      this.sessionId = opts.sessionId || null;
      this._fetch = opts.fetch || (typeof fetch === 'function' ? fetch : null);
      if (!this._fetch) {
        throw new LittleDBError('当前环境没有 fetch：Node 需要 18+，或通过 options.fetch 注入');
      }
    }

    // ------------------------------------------------------------ 底层请求

    async _request(method, path, body, extraHeaders) {
      const headers = Object.assign({}, extraHeaders || {});
      if (body !== undefined) headers['Content-Type'] = 'application/json';
      if (this.token) headers['Authorization'] = 'Bearer ' + this.token;
      if (this.sessionId) headers['X-Session-Id'] = this.sessionId;

      let response;
      try {
        response = await this._fetch(this.baseUrl + path, {
          method,
          headers,
          body: body === undefined ? undefined : JSON.stringify(body),
        });
      } catch (err) {
        throw new LittleDBError('无法连接 LittleDB：' + err.message, { code: 'NETWORK' });
      }

      const sessionId = response.headers && response.headers.get
        ? response.headers.get('X-Session-Id')
        : null;
      if (sessionId) this.sessionId = sessionId;

      let payload = null;
      try {
        payload = await response.json();
      } catch (_) {
        throw new LittleDBError('服务端返回了非 JSON 响应（HTTP ' + response.status + '）', {
          code: 'PROTOCOL_ERROR',
          status: response.status,
        });
      }

      if (!payload || payload.ok !== true) {
        const error = (payload && payload.error) || {};
        const retryAfter = response.headers && response.headers.get
          ? Number(response.headers.get('Retry-After')) || 0
          : 0;
        throw new LittleDBError(error.message || '请求失败', {
          code: error.code || 'INTERNAL_ERROR',
          status: response.status,
          retryAfterMs: retryAfter * 1000,
        });
      }
      return payload.result;
    }

    // ------------------------------------------------------------ 查询

    /**
     * 执行任意 SQL，返回服务端原始结果（select 带 columns/types/rows/rowCount）。
     *
     * 单语句直接返回该语句的结果；多语句（`a;b;c`）返回 `{type:'batch', results:[...]}`。
     * @param {string} sql
     * @param {any[]} [params] `?` 占位参数
     */
    async sql(sql, params) {
      const result = await this._request('POST', '/api/sql', { sql, params: params || [] });
      // 兼容旧版服务端：单语句会被包成只有一项的 batch
      if (result && result.type === 'batch' && result.results && result.results.length === 1) {
        return result.results[0];
      }
      return result;
    }

    /** 多语句执行，永远返回结果数组 */
    async sqlAll(sql, params) {
      const result = await this._request('POST', '/api/sql', { sql, params: params || [] });
      if (result && result.type === 'batch') return result.results || [];
      return result ? [result] : [];
    }

    /** 只取结果集：返回 { columns, types, rows, objects } */
    async query(sql, params) {
      const result = await this.sql(sql, params);
      if (result.type !== 'select') {
        throw new LittleDBError(
          result.type === 'batch'
            ? '多语句请用 sqlAll()；query() 只接受单条 SELECT'
            : '该语句没有结果集（type=' + result.type + '）',
          { code: 'NOT_A_SELECT' }
        );
      }
      return {
        columns: result.columns,
        types: result.types,
        rows: result.rows,
        /** 行 → 对象（列名做 key），方便直接喂给模板/JSON */
        objects: toObjects(result.columns, result.rows),
      };
    }

    /** 只要「行对象数组」，最常用 */
    async rows(sql, params) {
      return (await this.query(sql, params)).objects;
    }

    /** 只要一行（没有结果返回 null） */
    async row(sql, params) {
      const list = await this.rows(sql, params);
      return list.length ? list[0] : null;
    }

    /** 只要一个标量（第一行第一列） */
    async value(sql, params) {
      const list = await this.query(sql, params);
      return list.rows.length ? list.rows[0][0] : null;
    }

    // ------------------------------------------------------------ 便捷写入

    /**
     * 批量插入（对象数组），自动拼列并走 `?` 参数。
     * @returns {Promise<{inserted:number, ids:number[], lastId:number|null}>}
     */
    async insert(table, rows, options) {
      const list = normalizeRows(rows);
      if (!list.length) return { inserted: 0, ids: [], lastId: null };
      const columns = unionColumns(list);
      const placeholders = list.map(() => '(' + columns.map(() => '?').join(',') + ')').join(',');
      const params = [];
      for (const row of list) {
        for (const column of columns) params.push(toParam(row[column]));
      }
      const sql =
        'INSERT INTO ' + quoteIdent(table) +
        ' (' + columns.map(quoteIdent).join(',') + ') VALUES ' + placeholders;
      if (options && options.onConflict) {
        return this._insertWithConflict(sql, params, options.onConflict);
      }
      const result = await this.sql(sql, params);
      return {
        inserted: result.inserted,
        ids: result.ids || [],
        lastId: result.lastId === undefined ? null : result.lastId,
      };
    }

    /**
     * UPSERT：冲突时更新。
     * @param {string} table
     * @param {object|object[]} rows
     * @param {string[]} conflictColumns 冲突判定列（要有主键/唯一索引）
     * @param {string[]} [updateColumns] 冲突时要更新的列；省略=除冲突列外全部；传 []=DO NOTHING
     */
    async upsert(table, rows, conflictColumns, updateColumns) {
      const list = normalizeRows(rows);
      if (!list.length) return { inserted: 0, updated: 0, skipped: 0, ids: [], lastId: null };
      const columns = unionColumns(list);
      const conflict = conflictColumns && conflictColumns.length ? conflictColumns : [];
      let targets = updateColumns;
      if (!targets) {
        targets = columns.filter((c) => !conflict.some((k) => k.toLowerCase() === c.toLowerCase()));
      }
      const action = targets.length
        ? 'DO UPDATE SET ' + targets.map((c) => quoteIdent(c) + ' = EXCLUDED.' + quoteIdent(c)).join(', ')
        : 'DO NOTHING';
      const placeholders = list.map(() => '(' + columns.map(() => '?').join(',') + ')').join(',');
      const params = [];
      for (const row of list) {
        for (const column of columns) params.push(toParam(row[column]));
      }
      let sql =
        'INSERT INTO ' + quoteIdent(table) +
        ' (' + columns.map(quoteIdent).join(',') + ') VALUES ' + placeholders +
        ' ON CONFLICT';
      if (conflict.length) sql += ' (' + conflict.map(quoteIdent).join(',') + ')';
      sql += ' ' + action;
      const result = await this.sql(sql, params);
      return {
        inserted: result.inserted,
        updated: result.updated || 0,
        skipped: result.skipped || 0,
        ids: result.ids || [],
        lastId: result.lastId === undefined ? null : result.lastId,
      };
    }

    async _insertWithConflict(sql, params, onConflict) {
      const conflict = onConflict.columns || [];
      let action = 'DO NOTHING';
      if (onConflict.update && onConflict.update.length) {
        action =
          'DO UPDATE SET ' +
          onConflict.update.map((c) => quoteIdent(c) + ' = EXCLUDED.' + quoteIdent(c)).join(', ');
      }
      let text = sql + ' ON CONFLICT';
      if (conflict.length) text += ' (' + conflict.map(quoteIdent).join(',') + ')';
      text += ' ' + action;
      const result = await this.sql(text, params);
      return {
        inserted: result.inserted,
        updated: result.updated || 0,
        skipped: result.skipped || 0,
        ids: result.ids || [],
        lastId: result.lastId === undefined ? null : result.lastId,
      };
    }

    /** 主键/唯一列定位后更新一行（等价于 UPDATE ... WHERE key = ?） */
    async updateById(table, id, patch) {
      const columns = Object.keys(patch);
      if (!columns.length) return 0;
      const sql =
        'UPDATE ' + quoteIdent(table) + ' SET ' +
        columns.map((c) => quoteIdent(c) + ' = ?').join(', ') +
        ' WHERE id = ?';
      const params = columns.map((c) => toParam(patch[c]));
      params.push(toParam(id));
      const result = await this.sql(sql, params);
      return result.updated;
    }

    // ------------------------------------------------------------ 事务

    /**
     * 事务：`BEGIN` → 执行回调 → `COMMIT`；回调抛错则 `ROLLBACK` 后重新抛出。
     *
     * 注意回调里**必须用同一个 client 实例**（`db` 或参数 `tx`），
     * 因为事务状态是绑定在会话上的（服务端用 `X-Session-Id` 识别）。
     *
     * @param {(tx: LittleDB) => Promise<any>} fn
     * @param {{retries?: number}} [options] 写冲突(409)/限速(429) 时自动重试次数
     */
    async transaction(fn, options) {
      const retries = (options && options.retries) || 0;
      let attempt = 0;
      for (;;) {
        try {
          return await this._runTransaction(fn);
        } catch (err) {
          const canRetry =
            attempt < retries && err instanceof LittleDBError && err.retryable;
          if (!canRetry) throw err;
          attempt += 1;
          const wait = err.retryAfterMs || Math.min(50 * attempt, 500);
          await sleep(wait);
        }
      }
    }

    async _runTransaction(fn) {
      await this.sql('BEGIN');
      try {
        const outcome = await fn(this);
        await this.sql('COMMIT');
        return outcome;
      } catch (err) {
        try {
          await this.sql('ROLLBACK');
        } catch (_) {
          // 回滚失败（例如连接已断）不掩盖原始错误
        }
        throw err;
      }
    }

    // ------------------------------------------------------------ 元数据

    health() {
      return this._request('GET', '/api/health');
    }

    tables() {
      return this._request('GET', '/api/tables');
    }

    schema() {
      return this._request('GET', '/api/schema');
    }

    /** 分页取行：返回 { columns, types, rows, objects, total, limit, offset, schema } */
    async table(name, options) {
      const opts = options || {};
      const query = [];
      if (opts.limit !== undefined) query.push('limit=' + encodeURIComponent(opts.limit));
      if (opts.offset !== undefined) query.push('offset=' + encodeURIComponent(opts.offset));
      const path = '/api/tables/' + encodeURIComponent(name) + (query.length ? '?' + query.join('&') : '');
      const result = await this._request('GET', path);
      return {
        columns: result.columns,
        types: result.types,
        rows: result.rows,
        objects: toObjects(result.columns, result.rows),
        total: result.total,
        limit: result.limit,
        offset: result.offset,
        schema: result.schema,
      };
    }

    dropTable(name) {
      return this._request('DELETE', '/api/tables/' + encodeURIComponent(name));
    }
  }

  function normalizeRows(rows) {
    if (Array.isArray(rows)) return rows.filter((row) => row && typeof row === 'object');
    if (rows && typeof rows === 'object') return [rows];
    return [];
  }

  /** 多行对象取列并集（保持首次出现顺序） */
  function unionColumns(rows) {
    const seen = new Set();
    const columns = [];
    for (const row of rows) {
      for (const key of Object.keys(row)) {
        const lower = key.toLowerCase();
        if (seen.has(lower)) continue;
        seen.add(lower);
        columns.push(key);
      }
    }
    return columns;
  }

  function toObjects(columns, rows) {
    const list = Array.isArray(rows) ? rows : [];
    return list.map((row) => {
      const object = {};
      columns.forEach((name, index) => {
        object[name] = row[index];
      });
      return object;
    });
  }

  function createClient(baseUrl, options) {
    return new LittleDB(baseUrl, options);
  }

  return { LittleDB, LittleDBError, createClient, quoteIdent, toParam };
});
