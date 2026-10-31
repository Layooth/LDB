//! SQL 语法树定义。

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column {
        table: Option<String>,
        name: String,
    },
    Param(usize),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        star: bool,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr IN (SELECT ...)`：执行期被解析成 InList（非关联子查询）
    InSubquery {
        expr: Box<Expr>,
        query: Box<SelectStmt>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
    pub star: bool,
    /// `t.*` 里的表限定；`*` 和 `t.*` 都置 star = true
    pub star_table: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

/// FROM 子句：单表或 JOIN 树（左深）
#[derive(Debug, Clone, PartialEq)]
pub enum TableSource {
    Table { name: String, alias: Option<String> },
    Join {
        left: Box<TableSource>,
        right: Box<TableSource>,
        kind: JoinKind,
        on: Option<Expr>,
    },
}

impl TableSource {
    /// 供 EXPLAIN / 错误信息展示
    pub fn describe(&self) -> String {
        match self {
            TableSource::Table { name, .. } => name.clone(),
            TableSource::Join { left, right, kind, .. } => {
                let word = match kind {
                    JoinKind::Inner => "JOIN",
                    JoinKind::Left => "LEFT JOIN",
                    JoinKind::Cross => "CROSS JOIN",
                };
                format!("{} {} {}", left.describe(), word, right.describe())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnionKind {
    /// UNION：结果去重
    Distinct,
    /// UNION ALL：保留全部
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<TableSource>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// UNION / UNION ALL：右侧 SELECT。右侧不允许再写 ORDER BY
    pub union: Option<(UnionKind, Box<SelectStmt>)>,
}

#[derive(Debug, Clone)]
pub struct InsertStmt {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub values: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone)]
pub struct UpdateStmt {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct DeleteStmt {
    pub table: String,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub type_name: String,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct CreateTableStmt {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CreateIndexStmt {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    CreateTable(CreateTableStmt),
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex(CreateIndexStmt),
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Begin,
    Commit,
    Rollback,
    ShowTables,
    Describe {
        table: String,
    },
    Explain {
        statement: Box<Statement>,
    },
}

impl Statement {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Statement::Select(_) => "select",
            Statement::Insert(_) => "insert",
            Statement::Update(_) => "update",
            Statement::Delete(_) => "delete",
            Statement::CreateTable(_) => "create_table",
            Statement::DropTable { .. } => "drop_table",
            Statement::CreateIndex(_) => "create_index",
            Statement::DropIndex { .. } => "drop_index",
            Statement::Begin => "begin",
            Statement::Commit => "commit",
            Statement::Rollback => "rollback",
            Statement::ShowTables => "show_tables",
            Statement::Describe { .. } => "describe",
            Statement::Explain { .. } => "explain",
        }
    }

    /// 只读语句不需要开启写事务
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Statement::Select(_)
                | Statement::ShowTables
                | Statement::Describe { .. }
                | Statement::Explain { .. }
        )
    }
}
