//! 递归下降语法分析。支持：SELECT / INSERT /
//! UPDATE / DELETE、CREATE/DROP TABLE、CREATE/DROP INDEX、事务控制与元命令。

use crate::codec::ColumnType;
use crate::error::{DbError, Result};

use super::ast::*;
use super::lexer::{tokenize, Token, TokenKind};

/// 表达式嵌套深度上限。递归下降解析每层约 9 个栈帧，默认线程栈 2MB 下
/// 200 层就会爆栈（实测），而真实 SQL 几乎不会超过 20 层——64 足够宽裕。
pub const MAX_EXPR_DEPTH: usize = 64;

/// 单条语句的记号数上限。`1+1+1+...` 这类链式表达式是迭代构建的，
/// 深度闸门拦不住，但求值和 AST 析构仍是递归——
/// 2 万记号最坏产生约 1 万层 AST，配合 8MB 线程栈是安全的。
pub const MAX_TOKENS: usize = 20_000;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    param_index: usize,
    expr_depth: usize,
    explain_depth: usize,
}

impl Parser {
    pub fn new(sql: &str) -> Result<Parser> {
        let tokens = tokenize(sql)?;
        if tokens.len() > MAX_TOKENS {
            return Err(DbError::syntax(format!(
                "语句过长：{} 个记号，超过 {} 上限",
                tokens.len(),
                MAX_TOKENS
            )));
        }
        Ok(Parser {
            tokens,
            pos: 0,
            param_index: 0,
            expr_depth: 0,
            explain_depth: 0,
        })
    }

    /// 解析多语句块：按分号拆分，返回所有语句。
    /// 分号在字符串字面量/注释内不算边界（lexer 已处理）。
    pub fn parse_batch(sql: &str) -> Result<Vec<Statement>> {
        let mut parser = Parser::new(sql)?;
        let mut statements = Vec::new();
        loop {
            parser.eat_punct(";");
            if parser.at_eof() {
                break;
            }
            statements.push(parser.parse_statement()?);
            // 吃掉语句末尾的分号
            while parser.eat_punct(";") {}
        }
        Ok(statements)
    }

    pub fn parse(sql: &str) -> Result<Statement> {
        let mut parser = Parser::new(sql)?;
        let statement = parser.parse_statement()?;
        while parser.eat_punct(";") {}
        if !parser.at_eof() {
            let extra = parser.peek().text.clone();
            return Err(DbError::syntax(format!("语句末尾存在多余内容: \"{}\"", extra)));
        }
        Ok(statement)
    }

    // ------------------------------------------------------------ 词法辅助

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &Token {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)]
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn at_eof(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn at_keyword(&self, word: &str) -> bool {
        self.peek().is_keyword(word)
    }

    fn peek_at_keyword(&self, offset: usize, word: &str) -> bool {
        self.peek_at(offset).is_keyword(word)
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        if self.at_keyword(word) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn at_punct(&self, ch: &str) -> bool {
        self.peek().is_punct(ch)
    }

    fn eat_punct(&mut self, ch: &str) -> bool {
        if self.at_punct(ch) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, word: &str) -> Result<()> {
        if self.eat_keyword(word) {
            Ok(())
        } else {
            let got = self.peek().text.clone();
            Err(DbError::syntax(format!(
                "期望关键字 {}，实际是 \"{}\"",
                word, got
            )))
        }
    }

    fn expect_punct(&mut self, ch: &str) -> Result<()> {
        if self.eat_punct(ch) {
            Ok(())
        } else {
            let got = self.peek().text.clone();
            Err(DbError::syntax(format!(
                "期望 \"{}\"，实际是 \"{}\"",
                ch, got
            )))
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        if self.peek().kind == TokenKind::Ident {
            Ok(self.advance().text)
        } else {
            let got = self.peek().text.clone();
            Err(DbError::syntax(format!("期望标识符，实际是 \"{}\"", got)))
        }
    }

    fn eat_if_not_exists(&mut self) -> bool {
        if self.at_keyword("IF") && self.peek_at_keyword(1, "NOT") && self.peek_at_keyword(2, "EXISTS")
        {
            self.advance();
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_if_exists(&mut self) -> bool {
        if self.at_keyword("IF") && self.peek_at_keyword(1, "EXISTS") {
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------ 语句

    pub fn parse_statement(&mut self) -> Result<Statement> {
        if self.at_keyword("SELECT") {
            return self.parse_select();
        }
        if self.at_keyword("INSERT") {
            return self.parse_insert();
        }
        if self.at_keyword("UPDATE") {
            return self.parse_update();
        }
        if self.at_keyword("DELETE") {
            return self.parse_delete();
        }
        if self.at_keyword("CREATE") {
            return self.parse_create();
        }
        if self.at_keyword("DROP") {
            return self.parse_drop();
        }
        if self.at_keyword("ALTER") {
            return self.parse_alter();
        }
        if self.at_keyword("SHOW") {
            self.advance();
            self.expect_keyword("TABLES")?;
            return Ok(Statement::ShowTables);
        }
        if self.at_keyword("DESCRIBE") {
            self.advance();
            let table = self.expect_ident()?;
            return Ok(Statement::Describe { table });
        }
        if self.at_keyword("EXPLAIN") {
            self.advance();
            self.explain_depth += 1;
            if self.explain_depth > 10 {
                return Err(DbError::syntax("EXPLAIN 嵌套层数过多"));
            }
            let inner = self.parse_statement();
            self.explain_depth -= 1;
            let inner = inner?;
            if !matches!(
                inner,
                Statement::Select(_)
                    | Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_)
            ) {
                return Err(DbError::syntax(
                    "EXPLAIN 只支持 SELECT / INSERT / UPDATE / DELETE",
                ));
            }
            return Ok(Statement::Explain {
                statement: Box::new(inner),
            });
        }
        if self.at_keyword("BEGIN") {
            self.advance();
            let _ = self.eat_keyword("TRANSACTION");
            return Ok(Statement::Begin);
        }
        if self.at_keyword("START") {
            self.advance();
            self.expect_keyword("TRANSACTION")?;
            return Ok(Statement::Begin);
        }
        if self.at_keyword("COMMIT") {
            self.advance();
            return Ok(Statement::Commit);
        }
        if self.at_keyword("ROLLBACK") {
            self.advance();
            return Ok(Statement::Rollback);
        }
        for word in ["JOIN", "LEFT", "INNER", "CROSS", "UNION"] {
            if self.at_keyword(word) {
                return Err(DbError::syntax(format!(
                    "语句不能以 {} 开头",
                    word
                )));
            }
        }
        if self.at_keyword("RIGHT") || self.at_keyword("FULL") || self.at_keyword("OUTER") {
            return Err(DbError::syntax(
                "暂不支持 RIGHT / FULL OUTER JOIN：RIGHT JOIN 可交换两侧写成 LEFT JOIN",
            ));
        }
        let got = self.peek().text.clone();
        Err(DbError::syntax(format!("无法识别的语句起始: \"{}\"", got)))
    }

    fn parse_select(&mut self) -> Result<Statement> {
        self.expect_keyword("SELECT")?;
        let distinct = self.eat_keyword("DISTINCT");
        let items = self.parse_select_items()?;

        let from = if self.eat_keyword("FROM") {
            Some(self.parse_from()?)
        } else {
            None
        };

        let where_clause = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let group_by = if self.eat_keyword("GROUP") {
            self.expect_keyword("BY")?;
            self.parse_expr_list()?
        } else {
            Vec::new()
        };

        let having = if self.eat_keyword("HAVING") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut order_by = if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            let mut list = Vec::new();
            loop {
                let expr = self.parse_expr()?;
                let desc = if self.eat_keyword("DESC") {
                    true
                } else {
                    let _ = self.eat_keyword("ASC");
                    false
                };
                list.push(OrderItem { expr, desc });
                if !self.eat_punct(",") {
                    break;
                }
            }
            list
        } else {
            Vec::new()
        };

        let mut limit = if self.eat_keyword("LIMIT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let mut offset = if self.eat_keyword("OFFSET") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        // UNION / UNION ALL：末尾的 ORDER BY / LIMIT / OFFSET 归属整条
        // 复合查询（标准语义），解析时从右侧上移；两侧都写才算错误
        let union = if self.eat_keyword("UNION") {
            let all = self.eat_keyword("ALL");
            let mut right = self.parse_select_body()?;
            if !right.order_by.is_empty() {
                if !order_by.is_empty() {
                    return Err(DbError::syntax(
                        "ORDER BY 在 UNION 两侧都出现了，只能写一次",
                    ));
                }
                order_by = std::mem::take(&mut right.order_by);
            }
            if right.limit.is_some() || right.offset.is_some() {
                if limit.is_some() || offset.is_some() {
                    return Err(DbError::syntax(
                        "LIMIT/OFFSET 在 UNION 两侧都出现了，只能写一次",
                    ));
                }
                limit = right.limit.take();
                offset = right.offset.take();
            }
            Some((
                if all {
                    UnionKind::All
                } else {
                    UnionKind::Distinct
                },
                Box::new(right),
            ))
        } else {
            None
        };

        Ok(Statement::Select(SelectStmt {
            distinct,
            items,
            from,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
            union,
        }))
    }

    /// parse_select 的内部形式：返回 SelectStmt 本身（供 UNION 右侧递归）
    fn parse_select_body(&mut self) -> Result<SelectStmt> {
        match self.parse_select()? {
            Statement::Select(select) => Ok(select),
            _ => unreachable!("parse_select 只返回 Select"),
        }
    }

    fn parse_select_items(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            if self.at_punct("*") {
                self.advance();
                items.push(SelectItem {
                    expr: Expr::Literal(Literal::Null),
                    alias: None,
                    star: true,
                    star_table: None,
                });
            } else if self.peek().kind == TokenKind::Ident
                && self.peek_at(1).is_punct(".")
                && self.peek_at(2).is_punct("*")
            {
                let table = self.advance().text;
                self.advance();
                self.advance();
                items.push(SelectItem {
                    expr: Expr::Literal(Literal::Null),
                    alias: None,
                    star: true,
                    star_table: Some(table),
                });
            } else {
                let expr = self.parse_expr()?;
                let alias = if self.eat_keyword("AS") {
                    Some(self.expect_ident()?)
                } else if self.peek().kind == TokenKind::Ident {
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                items.push(SelectItem {
                    expr,
                    alias,
                    star: false,
                    star_table: None,
                });
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        Ok(items)
    }

    /// FROM 子句：`表 [别名] (JOIN | `,`) 表 [别名] [ON ...]*` 的左深链
    fn parse_from(&mut self) -> Result<TableSource> {
        let mut left = self.parse_table_ref()?;
        loop {
            let kind = if self.at_keyword("INNER") {
                self.advance();
                self.expect_keyword("JOIN")?;
                JoinKind::Inner
            } else if self.eat_keyword("JOIN") {
                JoinKind::Inner
            } else if self.at_keyword("LEFT") {
                self.advance();
                let _ = self.eat_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinKind::Left
            } else if self.at_keyword("CROSS") {
                self.advance();
                self.expect_keyword("JOIN")?;
                JoinKind::Cross
            } else if self.at_punct(",") {
                // 逗号连接 = CROSS JOIN
                self.advance();
                JoinKind::Cross
            } else {
                break;
            };

            let right = self.parse_table_ref()?;
            let on = if kind == JoinKind::Cross {
                None
            } else {
                self.expect_keyword("ON")?;
                Some(self.parse_expr()?)
            };
            left = TableSource::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
            };
        }
        Ok(left)
    }

    fn parse_table_ref(&mut self) -> Result<TableSource> {
        let table = self.expect_ident()?;
        let alias = if self.eat_keyword("AS") {
            Some(self.expect_ident()?)
        } else if self.peek().kind == TokenKind::Ident {
            Some(self.expect_ident()?)
        } else {
            None
        };
        // 未支持的连接形式给出明确的替代建议，而不是笼统的语法错误
        for (word, hint) in [("RIGHT", "用 LEFT JOIN 交换两侧即可"), ("FULL", "暂不支持 FULL OUTER JOIN")] {
            if self.at_keyword(word) {
                return Err(DbError::syntax(format!("{} JOIN 不受支持：{}", word, hint)));
            }
        }
        if self.at_keyword("OUTER") {
            return Err(DbError::syntax("OUTER 需要跟在 LEFT JOIN 中使用"));
        }
        Ok(TableSource::Table { name: table, alias })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table = self.expect_ident()?;

        let columns = if self.at_punct("(") {
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct(")")?;
            Some(cols)
        } else {
            None
        };

        self.expect_keyword("VALUES")?;
        let mut values = Vec::new();
        loop {
            self.expect_punct("(")?;
            let mut row = Vec::new();
            if !self.at_punct(")") {
                loop {
                    row.push(self.parse_expr()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
            }
            self.expect_punct(")")?;
            values.push(row);
            if !self.eat_punct(",") {
                break;
            }
        }

        Ok(Statement::Insert(InsertStmt {
            table,
            columns,
            values,
        }))
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect_keyword("UPDATE")?;
        let table = self.expect_ident()?;
        self.expect_keyword("SET")?;
        let mut assignments = Vec::new();
        loop {
            let column = self.expect_ident()?;
            self.expect_punct("=")?;
            let value = self.parse_expr()?;
            assignments.push((column, value));
            if !self.eat_punct(",") {
                break;
            }
        }
        let where_clause = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Update(UpdateStmt {
            table,
            assignments,
            where_clause,
        }))
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("FROM")?;
        let table = self.expect_ident()?;
        let where_clause = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Delete(DeleteStmt {
            table,
            where_clause,
        }))
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword("CREATE")?;
        let unique = self.eat_keyword("UNIQUE");

        if self.eat_keyword("INDEX") {
            let if_not_exists = self.eat_if_not_exists();
            let name = self.expect_ident()?;
            self.expect_keyword("ON")?;
            let table = self.expect_ident()?;
            self.expect_punct("(")?;
            let mut columns = Vec::new();
            loop {
                columns.push(self.expect_ident()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct(")")?;
            return Ok(Statement::CreateIndex(CreateIndexStmt {
                name,
                table,
                columns,
                unique,
                if_not_exists,
            }));
        }

        if unique {
            return Err(DbError::syntax("UNIQUE 只能用于 CREATE UNIQUE INDEX"));
        }

        self.expect_keyword("TABLE")?;
        let if_not_exists = self.eat_if_not_exists();
        let name = self.expect_ident()?;
        self.expect_punct("(")?;

        let mut columns = Vec::new();
        let mut primary_key = Vec::new();
        loop {
            if self.at_keyword("PRIMARY") {
                self.advance();
                self.expect_keyword("KEY")?;
                self.expect_punct("(")?;
                loop {
                    primary_key.push(self.expect_ident()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.expect_punct(")")?;
            } else {
                columns.push(self.parse_column_def()?);
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;

        Ok(Statement::CreateTable(CreateTableStmt {
            name,
            if_not_exists,
            columns,
            primary_key,
        }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_ident()?;
        let mut type_name = self.expect_ident()?;
        // DECIMAL(p, s) / DECIMAL(p)：标度进类型声明，随 catalog 持久化
        if type_name.to_uppercase() == "DECIMAL" && self.at_punct("(") {
            self.advance();
            let mut parts = Vec::new();
            loop {
                let token = self.peek().clone();
                if token.kind != TokenKind::Number {
                    return Err(DbError::syntax(format!(
                        "DECIMAL 声明需要整数参数，实际是 \"{}\"",
                        token.text
                    )));
                }
                parts.push(self.advance().text);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct(")")?;
            type_name = format!("DECIMAL({})", parts.join(","));
        }

        let mut primary_key = false;
        let mut not_null = false;
        let mut unique = false;
        let mut default = None;
        let mut auto_increment = false;

        loop {
            if self.at_keyword("PRIMARY") {
                self.advance();
                self.expect_keyword("KEY")?;
                primary_key = true;
            } else if self.at_keyword("NOT") {
                self.advance();
                self.expect_keyword("NULL")?;
                not_null = true;
            } else if self.at_keyword("UNIQUE") {
                self.advance();
                unique = true;
            } else if self.at_keyword("DEFAULT") {
                self.advance();
                default = Some(self.parse_unary()?);
            } else if self.at_keyword("AUTO_INCREMENT") || self.at_keyword("AUTOINCREMENT") {
                self.advance();
                auto_increment = true;
            } else if self.at_keyword("NULL") {
                self.advance();
            } else {
                break;
            }
        }

        Ok(ColumnDef {
            name,
            type_name,
            primary_key,
            not_null,
            unique,
            default,
            auto_increment,
        })
    }

    /// `ALTER TABLE t ADD [COLUMN] ...| DROP [COLUMN] c | RENAME COLUMN a TO b | RENAME TO t2`
    fn parse_alter(&mut self) -> Result<Statement> {
        self.expect_keyword("ALTER")?;
        self.expect_keyword("TABLE")?;
        let table = self.expect_ident()?;

        if self.eat_keyword("ADD") {
            let _ = self.eat_keyword("COLUMN");
            let def = self.parse_column_def()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterAction::AddColumn(def),
            });
        }
        if self.eat_keyword("DROP") {
            let _ = self.eat_keyword("COLUMN");
            let name = self.expect_ident()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterAction::DropColumn { name },
            });
        }
        if self.eat_keyword("RENAME") {
            if self.eat_keyword("TO") {
                let to = self.expect_ident()?;
                return Ok(Statement::AlterTable {
                    table,
                    action: AlterAction::RenameTable { to },
                });
            }
            let _ = self.eat_keyword("COLUMN");
            let from = self.expect_ident()?;
            self.expect_keyword("TO")?;
            let to = self.expect_ident()?;
            return Ok(Statement::AlterTable {
                table,
                action: AlterAction::RenameColumn { from, to },
            });
        }

        Err(DbError::syntax(
            "ALTER TABLE 支持：ADD [COLUMN] 列定义 / DROP [COLUMN] 列名 / \
             RENAME COLUMN 旧列 TO 新列 / RENAME TO 新表名",
        ))
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_keyword("DROP")?;
        if self.eat_keyword("TABLE") {
            let if_exists = self.eat_if_exists();
            let name = self.expect_ident()?;
            return Ok(Statement::DropTable { name, if_exists });
        }
        if self.eat_keyword("INDEX") {
            let if_exists = self.eat_if_exists();
            let name = self.expect_ident()?;
            return Ok(Statement::DropIndex { name, if_exists });
        }
        Err(DbError::syntax("DROP 后面只能是 TABLE 或 INDEX"))
    }

    // ------------------------------------------------------------ 表达式

    /// 递归下降的深度闸门：括号、NOT、一元 +/- 都会叠加嵌套层级
    fn enter_expr(&mut self) -> Result<()> {
        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            return Err(DbError::syntax(format!(
                "表达式嵌套超过 {} 层上限",
                MAX_EXPR_DEPTH
            )));
        }
        Ok(())
    }

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.enter_expr()?;
        let outcome = self.parse_or();
        self.expr_depth -= 1;
        outcome
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut list = vec![self.parse_expr()?];
        while self.eat_punct(",") {
            list.push(self.parse_expr()?);
        }
        Ok(list)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.eat_keyword("OR") {
            let right = self.parse_and()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while self.eat_keyword("AND") {
            let right = self.parse_not()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_keyword("NOT") {
            self.enter_expr()?;
            let operand = self.parse_not();
            self.expr_depth -= 1;
            return Ok(Expr::Unary {
                op: UnOp::Not,
                operand: Box::new(operand?),
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut left = self.parse_additive()?;
        loop {
            if self.eat_keyword("IS") {
                let negated = self.eat_keyword("NOT");
                self.expect_keyword("NULL")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                continue;
            }

            let mut negated = false;
            if self.at_keyword("NOT")
                && (self.peek_at_keyword(1, "LIKE")
                    || self.peek_at_keyword(1, "IN")
                    || self.peek_at_keyword(1, "BETWEEN"))
            {
                self.advance();
                negated = true;
            }

            if self.eat_keyword("LIKE") {
                let pattern = self.parse_additive()?;
                left = Expr::Like {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
                    negated,
                };
                continue;
            }

            if self.eat_keyword("IN") {
                self.expect_punct("(")?;
                // IN (SELECT ...)：子查询形式
                if self.at_keyword("SELECT") {
                    let query = Box::new(self.parse_select_body()?);
                    self.expect_punct(")")?;
                    left = Expr::InSubquery {
                        expr: Box::new(left),
                        query,
                        negated,
                    };
                    continue;
                }
                let mut list = Vec::new();
                if !self.at_punct(")") {
                    loop {
                        list.push(self.parse_expr()?);
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                }
                self.expect_punct(")")?;
                left = Expr::InList {
                    expr: Box::new(left),
                    list,
                    negated,
                };
                continue;
            }

            if self.eat_keyword("BETWEEN") {
                let low = self.parse_additive()?;
                self.expect_keyword("AND")?;
                let high = self.parse_additive()?;
                left = Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                };
                continue;
            }

            if negated {
                return Err(DbError::syntax("NOT 后面只能跟 LIKE / IN / BETWEEN"));
            }

            let op = if self.eat_punct("=") {
                Some(BinOp::Eq)
            } else if self.eat_punct("!=") || self.eat_punct("<>") {
                Some(BinOp::Ne)
            } else if self.eat_punct("<=") {
                Some(BinOp::Le)
            } else if self.eat_punct(">=") {
                Some(BinOp::Ge)
            } else if self.eat_punct("<") {
                Some(BinOp::Lt)
            } else if self.eat_punct(">") {
                Some(BinOp::Gt)
            } else {
                None
            };

            match op {
                Some(op) => {
                    let right = self.parse_additive()?;
                    left = Expr::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                None => break,
            }
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = if self.eat_punct("+") {
                Some(BinOp::Add)
            } else if self.eat_punct("-") {
                Some(BinOp::Sub)
            } else {
                None
            };
            match op {
                Some(op) => {
                    let right = self.parse_multiplicative()?;
                    left = Expr::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                None => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.eat_punct("*") {
                Some(BinOp::Mul)
            } else if self.eat_punct("/") {
                Some(BinOp::Div)
            } else if self.eat_punct("%") {
                Some(BinOp::Mod)
            } else {
                None
            };
            match op {
                Some(op) => {
                    let right = self.parse_unary()?;
                    left = Expr::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    };
                }
                None => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let op = if self.at_punct("-") {
            self.advance();
            Some(UnOp::Neg)
        } else if self.at_punct("+") {
            self.advance();
            Some(UnOp::Plus)
        } else {
            None
        };
        if let Some(op) = op {
            self.enter_expr()?;
            let operand = self.parse_unary();
            self.expr_depth -= 1;
            return Ok(Expr::Unary {
                op,
                operand: Box::new(operand?),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        let token = self.peek().clone();

        match token.kind {
            TokenKind::Number => {
                self.advance();
                if token.text.contains('.') || token.text.contains('e') || token.text.contains('E') {
                    let value: f64 = token
                        .text
                        .parse()
                        .map_err(|_| DbError::syntax(format!("数字格式错误: {}", token.text)))?;
                    Ok(Expr::Literal(Literal::Real(value)))
                } else {
                    let value: i64 = token
                        .text
                        .parse()
                        .map_err(|_| DbError::syntax(format!("整数超出范围: {}", token.text)))?;
                    Ok(Expr::Literal(Literal::Int(value)))
                }
            }
            TokenKind::Str => {
                self.advance();
                Ok(Expr::Literal(Literal::Text(token.text.clone())))
            }
            TokenKind::Param => {
                self.advance();
                let index = self.param_index;
                self.param_index += 1;
                Ok(Expr::Param(index))
            }
            TokenKind::Punct => {
                if token.text == "(" {
                    self.advance();
                    let inner = self.parse_expr()?;
                    self.expect_punct(")")?;
                    return Ok(inner);
                }
                Err(DbError::syntax(format!("表达式中的意外符号 \"{}\"", token.text)))
            }
            TokenKind::Keyword => match token.text.as_str() {
                "CAST" => {
                    // CAST(expr AS 类型)：类型支持 DECIMAL(p,s)
                    self.advance();
                    self.expect_punct("(")?;
                    let inner = self.parse_expr()?;
                    self.expect_keyword("AS")?;
                    let mut type_name = self.expect_ident()?;
                    if type_name.to_uppercase() == "DECIMAL" && self.at_punct("(") {
                        self.advance();
                        let mut parts = Vec::new();
                        loop {
                            let token = self.peek().clone();
                            if token.kind != TokenKind::Number {
                                return Err(DbError::syntax(format!(
                                    "DECIMAL 声明需要整数参数，实际是 \"{}\"",
                                    token.text
                                )));
                            }
                            parts.push(self.advance().text);
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                        self.expect_punct(")")?;
                        type_name = format!("DECIMAL({})", parts.join(","));
                    }
                    let ty = ColumnType::parse(&type_name)?;
                    let scale = ColumnType::decimal_scale_from_type(&type_name)?;
                    self.expect_punct(")")?;
                    Ok(Expr::Cast {
                        expr: Box::new(inner),
                        ty,
                        scale,
                    })
                }
                "TRUE" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Bool(true)))
                }
                "FALSE" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Bool(false)))
                }
                "NULL" => {
                    self.advance();
                    Ok(Expr::Literal(Literal::Null))
                }
                _ => Err(DbError::syntax(format!(
                    "表达式中不能使用关键字 {}",
                    token.text
                ))),
            },
            TokenKind::Ident => {
                self.advance();
                let name = token.text.clone();

                // 函数调用
                if self.at_punct("(") {
                    self.advance();
                    let mut distinct = false;
                    let mut star = false;
                    let mut args = Vec::new();

                    if self.at_punct("*") {
                        self.advance();
                        star = true;
                    } else if !self.at_punct(")") {
                        if self.eat_keyword("DISTINCT") {
                            distinct = true;
                        }
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                    }
                    self.expect_punct(")")?;
                    return Ok(Expr::Function {
                        name: name.to_uppercase(),
                        args,
                        distinct,
                        star,
                    });
                }

                // 表限定列 t.col
                if self.at_punct(".") {
                    self.advance();
                    let column = self.expect_ident()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: column,
                    });
                }

                Ok(Expr::Column { table: None, name })
            }
            TokenKind::Eof => Err(DbError::syntax("表达式意外结束")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(sql: &str) -> Statement {
        Parser::parse(sql).unwrap()
    }

    #[test]
    fn select_basic() {
        let stmt = parse("SELECT id, name FROM users WHERE age > 18 ORDER BY id DESC LIMIT 10");
        match stmt {
            Statement::Select(select) => {
                assert_eq!(select.items.len(), 2);
                match select.from.as_ref().unwrap() {
                    TableSource::Table { name, .. } => assert_eq!(name, "users"),
                    other => panic!("期望单表，得到 {:?}", other),
                }
                assert!(select.where_clause.is_some());
                assert_eq!(select.order_by.len(), 1);
                assert!(select.order_by[0].desc);
                assert!(select.limit.is_some());
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn select_star_and_alias() {
        let stmt = parse("SELECT * FROM users u");
        match stmt {
            Statement::Select(select) => {
                assert!(select.items[0].star);
                match select.from.as_ref().unwrap() {
                    TableSource::Table { alias, .. } => assert_eq!(alias.as_deref(), Some("u")),
                    other => panic!("期望单表，得到 {:?}", other),
                }
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn operator_precedence() {
        // 1 + 2 * 3 -> Add(1, Mul(2, 3))
        let stmt = parse("SELECT 1 + 2 * 3");
        match stmt {
            Statement::Select(select) => match &select.items[0].expr {
                Expr::Binary { op, right, .. } => {
                    assert_eq!(*op, BinOp::Add);
                    match right.as_ref() {
                        Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Mul),
                        other => panic!("期望乘法，得到 {:?}", other),
                    }
                }
                other => panic!("期望加法，得到 {:?}", other),
            },
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn like_in_between_is_null() {
        assert!(matches!(
            parse("SELECT * FROM t WHERE name LIKE 'a%'"),
            Statement::Select(_)
        ));
        assert!(matches!(
            parse("SELECT * FROM t WHERE id IN (1, 2, 3)"),
            Statement::Select(_)
        ));
        assert!(matches!(
            parse("SELECT * FROM t WHERE id BETWEEN 1 AND 5"),
            Statement::Select(_)
        ));
        assert!(matches!(
            parse("SELECT * FROM t WHERE name IS NOT NULL"),
            Statement::Select(_)
        ));
        assert!(matches!(
            parse("SELECT * FROM t WHERE name NOT LIKE 'x%' AND id NOT IN (1)"),
            Statement::Select(_)
        ));
    }

    #[test]
    fn create_table_with_constraints() {
        let stmt = parse(
            "CREATE TABLE IF NOT EXISTS p (id INT PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
             age INT DEFAULT 0, PRIMARY KEY (id))",
        );
        match stmt {
            Statement::CreateTable(create) => {
                assert!(create.if_not_exists);
                assert_eq!(create.columns.len(), 3);
                assert!(create.columns[0].primary_key);
                assert!(create.columns[1].not_null);
                assert!(create.columns[1].unique);
                assert!(create.columns[2].default.is_some());
                assert_eq!(create.primary_key, vec!["id".to_string()]);
            }
            other => panic!("期望 CREATE TABLE，得到 {:?}", other),
        }
    }

    #[test]
    fn insert_multi_rows() {
        let stmt = parse("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')");
        match stmt {
            Statement::Insert(insert) => {
                assert_eq!(insert.table, "t");
                assert_eq!(insert.columns.as_ref().unwrap().len(), 2);
                assert_eq!(insert.values.len(), 2);
            }
            other => panic!("期望 INSERT，得到 {:?}", other),
        }
    }

    #[test]
    fn update_delete() {
        match parse("UPDATE t SET a = 1, b = a + 1 WHERE id = 2") {
            Statement::Update(update) => {
                assert_eq!(update.assignments.len(), 2);
                assert!(update.where_clause.is_some());
            }
            other => panic!("期望 UPDATE，得到 {:?}", other),
        }
        match parse("DELETE FROM t WHERE id = 1") {
            Statement::Delete(delete) => assert_eq!(delete.table, "t"),
            other => panic!("期望 DELETE，得到 {:?}", other),
        }
    }

    #[test]
    fn join_union_parse() {
        // INNER JOIN + ON
        match parse("SELECT u.id, o.item FROM users u JOIN orders o ON u.id = o.user_id") {
            Statement::Select(select) => match select.from.as_ref().unwrap() {
                TableSource::Join { kind, on, .. } => {
                    assert_eq!(*kind, JoinKind::Inner);
                    assert!(on.is_some());
                }
                other => panic!("期望 JOIN，得到 {:?}", other),
            },
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
        // LEFT OUTER JOIN / 逗号连接 / CROSS JOIN
        assert!(matches!(
            parse("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id"),
            Statement::Select(_)
        ));
        match parse("SELECT * FROM a, b") {
            Statement::Select(select) => match select.from.as_ref().unwrap() {
                TableSource::Join { kind, on, .. } => {
                    assert_eq!(*kind, JoinKind::Cross);
                    assert!(on.is_none());
                }
                other => panic!("期望 CROSS，得到 {:?}", other),
            },
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
        assert!(matches!(
            parse("SELECT * FROM a CROSS JOIN b"),
            Statement::Select(_)
        ));
        // UNION / UNION ALL
        match parse("SELECT id FROM a UNION ALL SELECT id FROM b") {
            Statement::Select(select) => {
                let (kind, right) = select.union.as_ref().unwrap();
                assert_eq!(*kind, UnionKind::All);
                assert!(matches!(right.from.as_ref().unwrap(), TableSource::Table { .. }));
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
        assert!(matches!(
            parse("SELECT id FROM a UNION SELECT id FROM b"),
            Statement::Select(_)
        ));
        // 末尾 ORDER BY 归属整条复合查询（上移到左侧）
        match parse("SELECT id FROM a UNION SELECT id FROM b ORDER BY id") {
            Statement::Select(select) => {
                assert_eq!(select.order_by.len(), 1);
                assert!(select.union.as_ref().unwrap().1.order_by.is_empty());
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
        // 两侧都写 ORDER BY 才报错
        let err =
            Parser::parse("SELECT id FROM a ORDER BY id UNION SELECT id FROM b ORDER BY id")
                .unwrap_err();
        assert!(err.message.contains("只能写一次"), "{}", err.message);
        // RIGHT / FULL 仍不支持，但给出明确指引
        let err = Parser::parse("SELECT * FROM a RIGHT JOIN b ON a.id = b.id").unwrap_err();
        assert!(err.message.contains("RIGHT"), "{}", err.message);
    }

    #[test]
    fn params_are_numbered() {
        // WHERE a = ? AND b = ?  ->  And(Eq(a, Param0), Eq(b, Param1))
        match parse("SELECT * FROM t WHERE a = ? AND b = ?") {
            Statement::Select(select) => match select.where_clause.unwrap() {
                Expr::Binary {
                    op: BinOp::And,
                    right,
                    ..
                } => match right.as_ref() {
                    Expr::Binary { right, .. } => match right.as_ref() {
                        Expr::Param(index) => assert_eq!(*index, 1),
                        other => panic!("期望参数，得到 {:?}", other),
                    },
                    other => panic!("期望等值比较，得到 {:?}", other),
                },
                other => panic!("期望 AND 表达式，得到 {:?}", other),
            },
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn explain_wraps_statement() {
        match parse("EXPLAIN SELECT * FROM t") {
            Statement::Explain { statement } => {
                assert!(matches!(*statement, Statement::Select(_)));
            }
            other => panic!("期望 EXPLAIN，得到 {:?}", other),
        }
    }

    #[test]
    fn trailing_content_is_error() {
        assert!(Parser::parse("SELECT * FROM t extra stuff").is_err());
    }

    #[test]
    fn deep_expression_nesting_is_rejected_not_crash() {
        // 曾经：1000 层嵌套括号让递归解析爆栈、整个进程 abort
        let too_deep = format!("SELECT {}1{}", "(".repeat(MAX_EXPR_DEPTH + 1), ")".repeat(MAX_EXPR_DEPTH + 1));
        assert!(Parser::parse(&too_deep).is_err());

        let ok = format!("SELECT {}1{}", "(".repeat(50), ")".repeat(50));
        assert!(Parser::parse(&ok).is_ok());

        // 一元符号和 NOT 的自递归路径同样受限
        let unary_chain = format!("SELECT {}1", "-".repeat(MAX_EXPR_DEPTH + 1));
        assert!(Parser::parse(&unary_chain).is_err());
        let not_chain = format!("SELECT {} 1", "NOT ".repeat(MAX_EXPR_DEPTH + 1));
        assert!(Parser::parse(&not_chain).is_err());
    }

    #[test]
    fn explain_nesting_is_bounded() {
        // 双层 EXPLAIN 本来就被「EXPLAIN 只支持 SELECT/INSERT/UPDATE/DELETE」拒绝；
        // explain_depth 守卫是纵深防御，防止该白名单将来放宽后递归失控
        assert!(Parser::parse("EXPLAIN SELECT 1").is_ok());
        assert!(Parser::parse("EXPLAIN EXPLAIN SELECT 1").is_err());
        let chain = "EXPLAIN ".repeat(50) + "SELECT 1";
        assert!(Parser::parse(&chain).is_err());
    }
}
