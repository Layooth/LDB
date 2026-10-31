/**
 * LittleDB SQL 编辑器的补全逻辑（纯函数，不碰 DOM）。
 * 浏览器里挂到 window.LdbHints，Node 里可以直接 require 做单元测试。
 */
(function (root, factory) {
  const api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  else root.LdbHints = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  const KEYWORDS = [
    'SELECT', 'DISTINCT', 'ALL', 'FROM', 'WHERE', 'GROUP', 'BY', 'HAVING', 'ORDER',
    'ASC', 'DESC', 'LIMIT', 'OFFSET', 'INSERT', 'INTO', 'VALUES', 'UPDATE', 'SET',
    'DELETE', 'CREATE', 'TABLE', 'DROP', 'IF', 'NOT', 'EXISTS', 'PRIMARY', 'KEY',
    'UNIQUE', 'NULL', 'DEFAULT', 'AND', 'OR', 'TRUE', 'FALSE', 'LIKE', 'IN',
    'BETWEEN', 'IS', 'AS', 'INDEX', 'ON', 'BEGIN', 'COMMIT', 'ROLLBACK',
    'TRANSACTION', 'SHOW', 'TABLES', 'DESCRIBE', 'EXPLAIN', 'JOIN', 'UNION',
  ];

  const FUNCTIONS = ['COUNT', 'SUM', 'AVG', 'MIN', 'MAX', 'COALESCE', 'LENGTH', 'UPPER', 'LOWER', 'ABS', 'ROUND'];

  const KW_SET = new Set(KEYWORDS);
  const CTX_TABLE = new Set(['FROM', 'INTO', 'UPDATE', 'TABLE', 'JOIN']);
  const CTX_COLUMN = new Set(['WHERE', 'SET', 'HAVING', 'SELECT', 'BY', 'AND', 'OR', 'ON', 'LIMIT', 'ORDER', 'GROUP']);
  const RANK = { column: 0, table: 1, function: 2, keyword: 3 };

  /**
   * 从 SQL 文本里解析出「表名或别名 -> 表结构」的映射。
   * 只识别 FROM / UPDATE / INTO 后面紧跟的表名，够单表查询使用。
   */
  function parseFroms(text, tables) {
    const map = new Map();
    const list = tables || [];
    const re = /\b(?:from|update|into)\s+("?[\w$]+"?)(?:\s+(?:as\s+)?([A-Za-z_][\w$]*))?/gi;
    let m;
    while ((m = re.exec(text))) {
      const name = m[1].replace(/"/g, '').toLowerCase();
      const table = list.find((x) => String(x.name).toLowerCase() === name);
      if (!table) continue;
      map.set(String(table.name).toLowerCase(), table);
      const alias = m[2];
      if (alias && !KW_SET.has(alias.toUpperCase())) map.set(alias.toLowerCase(), table);
    }
    return map;
  }

  function inferTable(text, tables) {
    let last = null;
    for (const t of parseFroms(text, tables).values()) last = t;
    return last;
  }

  /** 分析光标前的上下文：正在输入的词、起点、是否在「别名.」之后、最近的子句关键字 */
  function analyzeContext(text, pos) {
    const before = String(text).slice(0, pos);
    const m = /[\w$]*$/.exec(before);
    const word = m ? m[0] : '';
    const start = pos - word.length;
    const head = before.slice(0, start).replace(/\s+$/, '');

    let qualifier = null;
    const q = /([A-Za-z_][\w$]*)\s*\.\s*$/.exec(head);
    if (q) qualifier = q[1];

    let ctxKeyword = null;
    const re = /[A-Za-z_][\w$]*/g;
    let t;
    while ((t = re.exec(head))) {
      const up = t[0].toUpperCase();
      if (CTX_TABLE.has(up) || CTX_COLUMN.has(up)) ctxKeyword = up;
    }
    return { word, start, qualifier, ctxKeyword };
  }

  function buildItems(ctx, tables) {
    const out = [];
    const seen = new Set();
    const list = tables || [];
    const push = (label, kind, detail, insert, caretOffset) => {
      const key = kind + ':' + label.toLowerCase();
      if (seen.has(key)) return;
      seen.add(key);
      out.push({ label, kind, detail: detail || '', insert: insert || label, caretOffset: caretOffset || 0 });
    };

    // 「别名.」之后只提示这张表的列
    if (ctx.qualifier) {
      const map = parseFroms(ctx.text || '', list);
      const target = map.get(ctx.qualifier.toLowerCase());
      const pool = target ? [target] : list;
      for (const t of pool) {
        for (const c of t.columns || []) push(c.name, 'column', `${t.name}.${c.name} · ${c.type}`);
      }
      return out;
    }

    const current = inferTable(ctx.text || '', list);
    if (current) {
      for (const c of current.columns || []) push(c.name, 'column', `${current.name}.${c.name} · ${c.type}`);
    }
    for (const t of list) push(t.name, 'table', `${(t.columns || []).length} 列 · ${t.rows || 0} 行`);
    for (const f of FUNCTIONS) push(f, 'function', '函数', f + '()', -1);
    for (const k of KEYWORDS) push(k, 'keyword');
    return out;
  }

  /**
   * 计算补全建议。
   * @returns {{start:number, word:string, items:Array}|null} 没有建议时返回 null
   */
  function suggest(text, pos, tables, force) {
    const ctx = analyzeContext(text, pos);
    ctx.text = String(text);
    // 输入「别名.」之后即使还没敲字母也要弹列名
    if (!force && ctx.word.length === 0 && !ctx.qualifier) return null;

    const word = ctx.word.toLowerCase();
    const items = buildItems(ctx, tables).filter((it) => it.label.toLowerCase().startsWith(word));
    items.sort((a, b) => RANK[a.kind] - RANK[b.kind] || a.label.localeCompare(b.label));
    if (items.length === 0) return null;
    // 已经把这个词完整打出来时不再打扰
    if (!force && items.length === 1 && items[0].label.toLowerCase() === word) return null;

    return { start: ctx.start, word: ctx.word, items: items.slice(0, 12) };
  }

  return { KEYWORDS, FUNCTIONS, parseFroms, inferTable, analyzeContext, buildItems, suggest };
});
