//! 递归下降语法分析。支持的语法范围与 Node 版一致：单表 SELECT / INSERT /
//! UPDATE / DELETE、CREATE/DROP TABLE、CREATE/DROP INDEX、事务控制与元命令。

use crate::error::{DbError, Result};

use super::ast::*;
use super::lexer::{tokenize, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    param_index: usize,
}

impl Parser {
    pub fn new(sql: &str) -> Result<Parser> {
        let tokens = tokenize(sql)?;
        Ok(Parser {
            tokens,
            pos: 0,
            param_index: 0,
        })
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
            let inner = self.parse_statement()?;
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
        for word in ["JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "FULL", "UNION"] {
            if self.at_keyword(word) {
                return Err(DbError::syntax(format!(
                    "{} 不受支持：只支持单表查询，请在应用层合并结果",
                    word
                )));
            }
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
        self.reject_multi_table()?;

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

        let order_by = if self.eat_keyword("ORDER") {
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

        let limit = if self.eat_keyword("LIMIT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let offset = if self.eat_keyword("OFFSET") {
            Some(self.parse_expr()?)
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
        }))
    }

    fn reject_multi_table(&mut self) -> Result<()> {
        if self.at_punct(",") {
            return Err(DbError::syntax(
                "不支持逗号连接的多表查询：只支持单表查询",
            ));
        }
        for word in ["JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "FULL", "UNION"] {
            if self.at_keyword(word) {
                return Err(DbError::syntax(format!(
                    "{} 查询不受支持：只支持单表查询，请在应用层合并结果",
                    word
                )));
            }
        }
        Ok(())
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
                });
            } else if self.peek().kind == TokenKind::Ident
                && self.peek_at(1).is_punct(".")
                && self.peek_at(2).is_punct("*")
            {
                self.advance();
                self.advance();
                self.advance();
                items.push(SelectItem {
                    expr: Expr::Literal(Literal::Null),
                    alias: None,
                    star: true,
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
                });
            }
            if !self.eat_punct(",") {
                break;
            }
        }
        Ok(items)
    }

    fn parse_from(&mut self) -> Result<FromItem> {
        let table = self.expect_ident()?;
        let alias = if self.eat_keyword("AS") {
            Some(self.expect_ident()?)
        } else if self.peek().kind == TokenKind::Ident {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(FromItem { table, alias })
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
        let type_name = self.expect_ident()?;

        let mut primary_key = false;
        let mut not_null = false;
        let mut unique = false;
        let mut default = None;

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
        })
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

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
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
            let operand = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnOp::Not,
                operand: Box::new(operand),
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
        if self.eat_punct("-") {
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnOp::Neg,
                operand: Box::new(operand),
            });
        }
        if self.eat_punct("+") {
            let operand = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnOp::Plus,
                operand: Box::new(operand),
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
                assert_eq!(select.from.as_ref().unwrap().table, "users");
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
                assert_eq!(select.from.as_ref().unwrap().alias.as_deref(), Some("u"));
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
    fn join_is_rejected_clearly() {
        let err = Parser::parse("SELECT * FROM a JOIN b ON a.id = b.id").unwrap_err();
        assert!(err.message.contains("只支持单表查询"), "{}", err.message);

        let err = Parser::parse("SELECT * FROM a, b").unwrap_err();
        assert!(err.message.contains("多表查询"), "{}", err.message);
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
}
