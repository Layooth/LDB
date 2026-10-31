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

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
    pub star: bool,
}

#[derive(Debug, Clone)]
pub struct FromItem {
    pub table: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone)]
pub struct SelectStmt {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Option<FromItem>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
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
