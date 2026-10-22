//! Recursive descent for rvbbit DDL.
//!
//! The productions live in `crates/rvbbit/grammar/rvbbit.y` (bison, zero
//! conflicts). This crate implements that language without linking the
//! engine, so `parser_driver` can classify a statement before gram.y runs.
//! Keywords are tokens of *this* grammar only. They are not added to
//! PostgreSQL's gram.y, so they stay unreserved in ordinary SQL.

use serde_json::{json, Value};

mod compat;
mod native;
pub use compat::{flow_arg_names, flow_statement, COMMON_CALLS};
pub use native::rewrite_native_semantic;

#[derive(Clone, Debug, PartialEq)]
pub struct Piece<'a> {
    pub sql: &'a str,
    pub offset: usize,
    pub rvbbit: bool,
}

/// `None` when the script has no rvbbit DDL. The caller must then parse the
/// original string, so non-rvbbit regression inputs stay byte-identical.
pub fn split_if_rvbbit(sql: &str) -> Option<Vec<Piece<'_>>> {
    let pieces = split_statements(sql);
    if !pieces.iter().any(|p| p.rvbbit) {
        return None;
    }
    Some(pieces)
}

pub fn verb(sql: &str) -> &'static str {
    let toks = lexer::tokenize(sql);
    match toks.first().and_then(|t| t.kw()) {
        Some(Kw::Drop) => "drop",
        Some(Kw::Alter) => "alter",
        Some(Kw::Grant) => "grant",
        Some(Kw::Revoke) => "revoke",
        Some(Kw::Comment) => "comment",
        _ => "create",
    }
}

pub fn parse_command(sql: &str) -> Result<Command, String> {
    let toks = lexer::tokenize(sql);
    let mut p = Parser { toks, i: 0, sql };
    let cmd = p.parse_stmt()?;
    p.skip_semi();
    if !p.at_end() {
        return Err(format!("extra tokens after rvbbit statement near {}", p.near()));
    }
    Ok(cmd)
}

fn split_statements(sql: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let b = sql.as_bytes();
    let mut i = 0;
    let mut start = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i < b.len() {
                i += 1;
            }
            continue;
        }
        if b[i] == b'$' {
            if let Some(end) = dollar_end(sql, i) {
                i = end;
                continue;
            }
        }
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        if b[i] == b';' {
            let piece = &sql[start..=i];
            if statement_has_tokens(piece) {
                out.push(Piece {
                    sql: piece,
                    offset: start,
                    rvbbit: is_rvbbit(piece),
                });
            }
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    if start < sql.len() && statement_has_tokens(&sql[start..]) {
        let piece = &sql[start..];
        out.push(Piece {
            sql: piece,
            offset: start,
            rvbbit: is_rvbbit(piece),
        });
    }
    out
}

fn dollar_end(sql: &str, start: usize) -> Option<usize> {
    let b = sql.as_bytes();
    if start >= b.len() || b[start] != b'$' {
        return None;
    }
    let mut j = start + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if j >= b.len() || b[j] != b'$' {
        return None;
    }
    let tag = &sql[start..=j];
    let rest = &sql[j + 1..];
    rest.find(tag).map(|k| j + 1 + k + tag.len())
}

fn statement_has_tokens(sql: &str) -> bool {
    !lexer::tokenize(sql).is_empty()
}

fn is_rvbbit(sql: &str) -> bool {
    let toks = lexer::tokenize(sql);
    let kw = |i: usize, w: Kw| toks.get(i).and_then(|t| t.kw()) == Some(w);
    let k = |i: usize| toks.get(i).and_then(|t| t.kw());
    if kw(0, Kw::Create) {
        // CREATE [OR REPLACE] [IF NOT EXISTS] <kind>
        let mut n = 1;
        if kw(1, Kw::Or) {
            n = 3;
        }
        if kw(n, Kw::If) {
            n += 3;
        }
        return matches!(
            k(n),
            Some(Kw::Backend | Kw::Model | Kw::Embedding | Kw::Semantic | Kw::Decision | Kw::Workflow)
        );
    }
    if kw(0, Kw::Alter) || kw(0, Kw::Drop) {
        let n = if kw(0, Kw::Drop) && kw(1, Kw::If) { 3 } else { 1 };
        return matches!(
            k(n),
            Some(Kw::Backend | Kw::Model | Kw::Semantic | Kw::Decision | Kw::Workflow)
        );
    }
    if kw(0, Kw::Comment) && kw(1, Kw::On) {
        return matches!(
            k(2),
            Some(Kw::Backend | Kw::Model | Kw::Semantic | Kw::Decision | Kw::Workflow)
        );
    }
    if kw(0, Kw::Grant) || kw(0, Kw::Revoke) {
        return toks.windows(2).any(|w| {
            w[0].kw() == Some(Kw::On)
                && matches!(
                    w[1].kw(),
                    Some(Kw::Backend | Kw::Semantic | Kw::Decision | Kw::Workflow)
                )
        });
    }
    false
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: String,
    pub typ: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WithOpts {
    pub batch_size: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub timeout_ms: Option<i64>,
    pub retries: Option<i64>,
}

impl Default for WithOpts {
    fn default() -> Self {
        Self {
            batch_size: None,
            max_concurrent: None,
            timeout_ms: None,
            retries: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnClauses {
    pub model: Option<String>,
    pub system: Option<String>,
    pub prompt: Option<String>,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub cache: Option<String>,
    pub tests: Vec<String>,
    pub version: Option<i64>,
    /// Fixed classify labels. Empty means extract (chat), not classify.
    pub labels: Vec<String>,
    /// Parallel to [`Self::labels`]. `None` means the default description.
    pub label_descriptions: Vec<Option<String>>,
    /// Noul cutoff in `[0, 1]`. Absent means the server default (0.5).
    pub threshold: Option<f64>,
    /// `DEFAULT CLAUSES (...)` tail. Call-site clauses override it.
    pub clauses: Option<String>,
    /// `RETURNS label`. The stored SQL type is `text` and `LABELS` is required.
    pub returns_label: bool,
    /// `AS DECISION TREE name`.
    pub tree: Option<String>,
    /// `AS PIPE` body, before the native rewrite.
    pub pipe: Option<String>,
}

impl Default for FnClauses {
    fn default() -> Self {
        Self {
            model: None,
            system: None,
            prompt: None,
            max_tokens: None,
            temperature: None,
            cache: None,
            tests: Vec::new(),
            version: None,
            labels: Vec::new(),
            label_descriptions: Vec::new(),
            threshold: None,
            clauses: None,
            returns_label: false,
            tree: None,
            pipe: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    CreateBackend {
        or_replace: bool,
        if_not_exists: bool,
        name: String,
        kind: String,
        url: String,
        auth_env: Option<String>,
        with: WithOpts,
        version: Option<i64>,
    },
    CreateModel {
        or_replace: bool,
        if_not_exists: bool,
        embedding: bool,
        name: String,
        backend: String,
        remote: String,
        dimensions: Option<i64>,
        cost_input: Option<f64>,
        cost_output: Option<f64>,
        version: Option<i64>,
    },
    CreateFunction {
        or_replace: bool,
        name: String,
        args: Vec<Arg>,
        returns: String,
        clauses: FnClauses,
    },
    CreateOperator {
        or_replace: bool,
        word: String,
        left: String,
        right: String,
        function: String,
        symbol: Option<String>,
        negator: Option<String>,
        version: Option<i64>,
        /// Operator cutoff. Absent means the function's `THRESHOLD`.
        threshold: Option<f64>,
    },
    CreateTree {
        or_replace: bool,
        name: String,
        args: Vec<Arg>,
        model: Option<String>,
        body: String,
        nodes: Value,
        version: Option<i64>,
    },
    CreateWorkflow {
        or_replace: bool,
        name: String,
        args: Option<String>,
        returns: Option<String>,
        version: Option<i64>,
        body: String,
    },
    AlterBackend {
        name: String,
        url: Option<String>,
        auth_env: Option<String>,
        with: WithOpts,
    },
    AlterModel {
        name: String,
        remote: Option<String>,
        backend: Option<String>,
    },
    AlterFunction {
        name: String,
        args: Vec<Arg>,
        model: Option<String>,
        prompt: Option<String>,
        system: Option<String>,
    },
    AlterOperator {
        word: String,
        function: String,
    },
    AlterTree {
        name: String,
        model: String,
    },
    AlterWorkflow {
        name: String,
        version: i64,
        body: String,
    },
    Drop {
        kind: ObjKind,
        if_exists: bool,
        name: String,
        args: Vec<Arg>,
        version: Option<i64>,
        cascade: bool,
    },
    Grant {
        is_grant: bool,
        privilege: String,
        kind: ObjKind,
        name: String,
        args: Vec<Arg>,
        role: String,
    },
    Comment {
        kind: ObjKind,
        name: String,
        args: Vec<Arg>,
        text: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    Backend,
    Model,
    Function,
    Operator,
    Tree,
    Workflow,
}

struct Parser<'a> {
    toks: Vec<lexer::Tok>,
    i: usize,
    sql: &'a str,
}

impl<'a> Parser<'a> {
    fn parse_stmt(&mut self) -> Result<Command, String> {
        if self.eat_kw(Kw::Create) {
            return self.parse_create();
        }
        if self.eat_kw(Kw::Alter) {
            return self.parse_alter();
        }
        if self.eat_kw(Kw::Drop) {
            return self.parse_drop();
        }
        if self.eat_kw(Kw::Grant) {
            return self.parse_grant(true);
        }
        if self.eat_kw(Kw::Revoke) {
            return self.parse_grant(false);
        }
        if self.eat_kw(Kw::Comment) {
            return self.parse_comment();
        }
        Err(format!("expected rvbbit DDL near {}", self.near()))
    }

    fn parse_create(&mut self) -> Result<Command, String> {
        let or_replace = self.eat_or_replace();
        let if_not_exists = self.eat_if_not_exists();
        if self.eat_kw(Kw::Backend) {
            let name = self.name()?;
            self.expect_kw(Kw::Type, "TYPE")?;
            let kind = self.name()?;
            self.expect_kw(Kw::Url, "URL")?;
            let url = self.string()?;
            let auth_env = self.opt_auth()?;
            let with = self.opt_with()?;
            let version = self.opt_version()?;
            return Ok(Command::CreateBackend {
                or_replace,
                if_not_exists,
                name,
                kind,
                url,
                auth_env,
                with,
                version,
            });
        }
        let embedding = self.eat_kw(Kw::Embedding);
        if self.eat_kw(Kw::Model) {
            let name = self.name()?;
            self.expect_kw(Kw::On, "ON")?;
            self.expect_kw(Kw::Backend, "BACKEND")?;
            let backend = self.name()?;
            self.expect_kw(Kw::Name, "NAME")?;
            let remote = self.string()?;
            let mut dimensions = None;
            let mut cost_input = None;
            let mut cost_output = None;
            if embedding {
                self.expect_kw(Kw::Dimensions, "DIMENSIONS")?;
                dimensions = Some(self.int()?);
            }
            if self.eat_kw(Kw::Cost) {
                self.expect_sym('(')?;
                loop {
                    if self.eat_kw(Kw::Input) {
                        self.expect_sym('=')?;
                        cost_input = Some(self.number()?);
                    } else if self.eat_kw(Kw::Output) {
                        self.expect_sym('=')?;
                        cost_output = Some(self.number()?);
                    } else if self.at_sym(')') || self.at_end() {
                        break;
                    } else {
                        self.bump();
                    }
                    self.eat_sym(',');
                    if self.at_sym(')') {
                        break;
                    }
                }
                self.expect_sym(')')?;
            }
            let version = self.opt_version()?;
            return Ok(Command::CreateModel {
                or_replace,
                if_not_exists,
                embedding,
                name,
                backend,
                remote,
                dimensions,
                cost_input,
                cost_output,
                version,
            });
        }
        if self.eat_kw(Kw::Semantic) {
            if self.eat_kw(Kw::Function) {
                let name = self.name()?;
                let args = self.arg_list()?;
                self.expect_kw(Kw::Returns, "RETURNS")?;
                let raw_returns = self.typ()?;
                let mut clauses = self.fn_clauses()?;
                let (returns, label) = normalize_returns(&raw_returns)?;
                clauses.returns_label = label;
                if label && clauses.labels.len() < 2 {
                    return Err("RETURNS label needs LABELS with at least two labels".into());
                }
                if clauses.pipe.is_some() && clauses.tree.is_some() {
                    return Err("AS PIPE and AS DECISION TREE cannot both be set".into());
                }
                if clauses.prompt.is_some() && (clauses.pipe.is_some() || clauses.tree.is_some()) {
                    return Err("AS template cannot be combined with AS PIPE or AS DECISION TREE".into());
                }
                return Ok(Command::CreateFunction {
                    or_replace,
                    name,
                    args,
                    returns,
                    clauses,
                });
            }
            if self.eat_kw(Kw::Operator) {
                let word = self.name()?;
                self.expect_sym('(')?;
                let mut left = None;
                let mut right = None;
                let mut function = None;
                let mut symbol = None;
                let mut negator = None;
                let mut threshold = None;
                while !self.at_sym(')') && !self.at_end() {
                    if self.eat_kw(Kw::Leftarg) {
                        self.expect_sym('=')?;
                        left = Some(self.typ_until(&[',', ')'])?);
                    } else if self.eat_kw(Kw::Rightarg) {
                        self.expect_sym('=')?;
                        right = Some(self.typ_until(&[',', ')'])?);
                    } else if self.eat_kw(Kw::Function) {
                        self.expect_sym('=')?;
                        function = Some(self.name()?);
                    } else if self.eat_kw(Kw::Symbol) {
                        self.expect_sym('=')?;
                        symbol = Some(self.string()?);
                    } else if self.eat_kw(Kw::Negator) {
                        self.expect_sym('=')?;
                        negator = Some(self.negator_name()?);
                    } else if self.eat_kw(Kw::Threshold) {
                        self.expect_sym('=')?;
                        threshold = Some(self.unit_threshold()?);
                    } else {
                        return Err(format!("bad operator item near {}", self.near()));
                    }
                    self.eat_sym(',');
                }
                self.expect_sym(')')?;
                let version = self.opt_version()?;
                return Ok(Command::CreateOperator {
                    or_replace,
                    word,
                    left: left.ok_or_else(|| "LEFTARG is required".to_string())?,
                    right: right.ok_or_else(|| "RIGHTARG is required".to_string())?,
                    function: function.ok_or_else(|| "FUNCTION is required".to_string())?,
                    symbol,
                    negator,
                    version,
                    threshold,
                });
            }
            return Err(format!("expected FUNCTION or OPERATOR near {}", self.near()));
        }
        if self.eat_kw(Kw::Decision) {
            self.expect_kw(Kw::Tree, "TREE")?;
            let name = self.name()?;
            let args = self.arg_list()?;
            let model = if self.eat_kw(Kw::Model) {
                Some(self.name()?)
            } else {
                None
            };
            self.expect_kw(Kw::As, "AS")?;
            let body_at = self.pos();
            let nodes = self.tree()?;
            let body = self.sql[body_at..self.pos()].trim().trim_end_matches(';').trim().to_string();
            let version = self.opt_version()?;
            return Ok(Command::CreateTree {
                or_replace,
                name,
                args,
                model,
                body,
                nodes,
                version,
            });
        }
        if self.eat_kw(Kw::Workflow) {
            let name = self.name()?;
            let args = if self.at_sym('(') {
                Some(self.raw_parens()?)
            } else {
                None
            };
            let returns = if self.eat_kw(Kw::Returns) {
                Some(self.raw_until(&[Kw::Version, Kw::As])?)
            } else {
                None
            };
            let version = self.opt_version()?;
            self.expect_kw(Kw::As, "AS")?;
            let body = self.rest_body()?;
            return Ok(Command::CreateWorkflow {
                or_replace,
                name,
                args,
                returns,
                version,
                body,
            });
        }
        Err(format!("expected BACKEND, MODEL, SEMANTIC, DECISION, or WORKFLOW near {}", self.near()))
    }

    fn parse_alter(&mut self) -> Result<Command, String> {
        if self.eat_kw(Kw::Backend) {
            let name = self.name()?;
            self.expect_kw(Kw::Set, "SET")?;
            if self.eat_kw(Kw::Url) {
                return Ok(Command::AlterBackend {
                    name,
                    url: Some(self.string()?),
                    auth_env: None,
                    with: WithOpts::default(),
                });
            }
            if self.eat_kw(Kw::Auth) {
                let auth_env = self.auth_after_auth()?;
                return Ok(Command::AlterBackend {
                    name,
                    url: None,
                    auth_env: Some(auth_env),
                    with: WithOpts::default(),
                });
            }
            if self.at_sym('(') {
                return Ok(Command::AlterBackend {
                    name,
                    url: None,
                    auth_env: None,
                    with: self.with_parens()?,
                });
            }
            return Err(format!("ALTER BACKEND expects SET URL, AUTH, or (...) near {}", self.near()));
        }
        if self.eat_kw(Kw::Model) {
            let name = self.name()?;
            if self.eat_kw(Kw::On) {
                self.expect_kw(Kw::Backend, "BACKEND")?;
                return Ok(Command::AlterModel {
                    name,
                    remote: None,
                    backend: Some(self.name()?),
                });
            }
            self.expect_kw(Kw::Set, "SET")?;
            self.expect_kw(Kw::Name, "NAME")?;
            return Ok(Command::AlterModel {
                name,
                remote: Some(self.string()?),
                backend: None,
            });
        }
        if self.eat_kw(Kw::Semantic) {
            if self.eat_kw(Kw::Function) {
                let name = self.name()?;
                let args = if self.at_sym('(') { self.arg_list()? } else { Vec::new() };
                self.expect_kw(Kw::Set, "SET")?;
                if self.eat_kw(Kw::Model) {
                    return Ok(Command::AlterFunction {
                        name,
                        args,
                        model: Some(self.name()?),
                        prompt: None,
                        system: None,
                    });
                }
                if self.eat_kw(Kw::Prompt) {
                    return Ok(Command::AlterFunction {
                        name,
                        args,
                        model: None,
                        prompt: Some(self.string()?),
                        system: None,
                    });
                }
                if self.eat_kw(Kw::System) {
                    return Ok(Command::AlterFunction {
                        name,
                        args,
                        model: None,
                        prompt: None,
                        system: Some(self.string()?),
                    });
                }
                return Err(format!("ALTER SEMANTIC FUNCTION expects SET MODEL, PROMPT, or SYSTEM near {}", self.near()));
            }
            if self.eat_kw(Kw::Operator) {
                let word = self.name()?;
                self.expect_kw(Kw::Set, "SET")?;
                self.expect_kw(Kw::Function, "FUNCTION")?;
                return Ok(Command::AlterOperator {
                    word,
                    function: self.name()?,
                });
            }
        }
        if self.eat_kw(Kw::Decision) {
            self.expect_kw(Kw::Tree, "TREE")?;
            let name = self.name()?;
            self.expect_kw(Kw::Set, "SET")?;
            self.expect_kw(Kw::Model, "MODEL")?;
            return Ok(Command::AlterTree {
                name,
                model: self.name()?,
            });
        }
        if self.eat_kw(Kw::Workflow) {
            let name = self.name()?;
            self.expect_kw(Kw::Version, "VERSION")?;
            let version = self.int()?;
            self.expect_kw(Kw::As, "AS")?;
            let body = self.rest_body()?;
            return Ok(Command::AlterWorkflow { name, version, body });
        }
        Err(format!("bad ALTER near {}", self.near()))
    }

    fn parse_drop(&mut self) -> Result<Command, String> {
        let if_exists = self.eat_if_exists();
        let kind = self.obj_kind()?;
        let name = self.name()?;
        let args = if kind == ObjKind::Function && self.at_sym('(') {
            self.arg_list()?
        } else {
            Vec::new()
        };
        let version = if kind == ObjKind::Workflow && self.eat_kw(Kw::Version) {
            Some(self.int()?)
        } else {
            None
        };
        let cascade = if self.eat_kw(Kw::Cascade) {
            true
        } else {
            self.eat_kw(Kw::Restrict);
            false
        };
        Ok(Command::Drop {
            kind,
            if_exists,
            name,
            args,
            version,
            cascade,
        })
    }

    fn parse_grant(&mut self, is_grant: bool) -> Result<Command, String> {
        let privilege = if self.eat_kw(Kw::Usage) {
            "USAGE".to_string()
        } else if self.eat_kw(Kw::Execute) {
            "EXECUTE".to_string()
        } else {
            return Err(format!("expected USAGE or EXECUTE near {}", self.near()));
        };
        self.expect_kw(Kw::On, "ON")?;
        let kind = self.obj_kind()?;
        let name = self.name()?;
        let args = if self.at_sym('(') { self.arg_list()? } else { Vec::new() };
        if is_grant {
            self.expect_kw(Kw::To, "TO")?;
        } else {
            self.expect_kw(Kw::From, "FROM")?;
        }
        let role = self.name()?;
        Ok(Command::Grant {
            is_grant,
            privilege,
            kind,
            name,
            args,
            role,
        })
    }

    fn parse_comment(&mut self) -> Result<Command, String> {
        self.expect_kw(Kw::On, "ON")?;
        let kind = self.obj_kind()?;
        let name = self.name()?;
        let args = if self.at_sym('(') { self.arg_list()? } else { Vec::new() };
        self.expect_kw(Kw::Is, "IS")?;
        let text = if self.eat_kw(Kw::Null) {
            None
        } else {
            Some(self.string()?)
        };
        Ok(Command::Comment { kind, name, args, text })
    }

    fn obj_kind(&mut self) -> Result<ObjKind, String> {
        if self.eat_kw(Kw::Backend) {
            return Ok(ObjKind::Backend);
        }
        if self.eat_kw(Kw::Model) {
            return Ok(ObjKind::Model);
        }
        if self.eat_kw(Kw::Semantic) {
            if self.eat_kw(Kw::Function) {
                return Ok(ObjKind::Function);
            }
            if self.eat_kw(Kw::Operator) {
                return Ok(ObjKind::Operator);
            }
            return Err(format!("expected FUNCTION or OPERATOR near {}", self.near()));
        }
        if self.eat_kw(Kw::Decision) {
            self.expect_kw(Kw::Tree, "TREE")?;
            return Ok(ObjKind::Tree);
        }
        if self.eat_kw(Kw::Workflow) {
            return Ok(ObjKind::Workflow);
        }
        Err(format!("expected an rvbbit object kind near {}", self.near()))
    }

    fn fn_clauses(&mut self) -> Result<FnClauses, String> {
        let mut c = FnClauses::default();
        loop {
            if self.eat_kw(Kw::Model) {
                c.model = Some(self.name()?);
            } else if self.eat_kw(Kw::System) {
                c.system = Some(self.string()?);
            } else if self.eat_kw(Kw::Prompt) {
                c.prompt = Some(self.string()?);
            } else if self.eat_kw(Kw::Max) {
                self.expect_kw(Kw::Tokens, "TOKENS")?;
                c.max_tokens = Some(self.int()?);
            } else if self.eat_kw(Kw::Temperature) {
                c.temperature = Some(self.number()?);
            } else if self.eat_kw(Kw::Threshold) {
                c.threshold = Some(self.unit_threshold()?);
            } else if self.eat_kw(Kw::Cache) {
                c.cache = Some(self.name()?);
            } else if self.eat_kw(Kw::Test) {
                let name = self.name()?;
                self.expect_kw(Kw::Expect, "EXPECT")?;
                let expect = self.name()?;
                self.expect_kw(Kw::For, "FOR")?;
                let for_list = self.raw_parens()?;
                c.tests.push(format!("{name} EXPECT {expect} FOR {for_list}"));
            } else if self.eat_kw(Kw::Version) {
                c.version = Some(self.int()?);
            } else if self.eat_kw(Kw::Labels) {
                self.expect_sym('(')?;
                let mut labels = Vec::new();
                let mut descriptions = Vec::new();
                if !self.at_sym(')') {
                    loop {
                        let label = self.string()?;
                        if label.is_empty() {
                            return Err("classify label is empty".into());
                        }
                        if labels.iter().any(|existing| existing == &label) {
                            return Err(format!("classify label \"{label}\" is duplicated"));
                        }
                        let description = if self.eat_kw(Kw::As) {
                            let text = self.string()?;
                            if text.is_empty() {
                                return Err("classify label description is empty".into());
                            }
                            Some(text)
                        } else {
                            None
                        };
                        labels.push(label);
                        descriptions.push(description);
                        if !self.eat_sym(',') {
                            break;
                        }
                    }
                }
                self.expect_sym(')')?;
                if labels.len() < 2 {
                    return Err("classify label set needs at least two labels".into());
                }
                c.labels = labels;
                c.label_descriptions = descriptions;
            } else if self.eat_kw(Kw::Default) {
                self.expect_kw(Kw::Clauses, "CLAUSES")?;
                self.expect_sym('(')?;
                let body = self.string()?;
                self.expect_sym(')')?;
                if body.trim().is_empty() {
                    return Err("DEFAULT CLAUSES is empty".into());
                }
                c.clauses = Some(body);
            } else if self.eat_kw(Kw::Using) {
                self.expect_kw(Kw::Model, "MODEL")?;
                c.model = Some(self.name()?);
            } else if self.eat_kw(Kw::As) {
                if self.eat_kw(Kw::Decision) {
                    self.expect_kw(Kw::Tree, "TREE")?;
                    c.tree = Some(self.name()?);
                } else if self.eat_kw(Kw::Pipe) {
                    let body = self.string()?;
                    if body.trim().is_empty() {
                        return Err("AS PIPE body is empty".into());
                    }
                    c.pipe = Some(body);
                } else {
                    c.prompt = Some(self.string()?);
                }
            } else if self.eat_kw(Kw::Takes) {
                let n = self.int()?;
                if n < 1 {
                    return Err("TAKES must be at least 1".into());
                }
                let extra = format!("WITH TAKES {n}");
                c.clauses = Some(match c.clauses.take() {
                    Some(prev) => format!("{prev} {extra}"),
                    None => extra,
                });
            } else if self.eat_kw(Kw::Steps) || self.eat_kw(Kw::Ward) {
                return Err(format!(
                    "{} is not implemented until milestone M5",
                    self.prev_text()
                ));
            } else {
                break;
            }
        }
        Ok(c)
    }

    fn tree(&mut self) -> Result<Value, String> {
        self.tree_limited(0, &mut 0)
    }

    fn tree_limited(&mut self, depth: i32, count: &mut i32) -> Result<Value, String> {
        if depth > 16 {
            return Err("decision tree deeper than 16".into());
        }
        *count += 1;
        if *count > 255 {
            return Err("decision tree has more than 255 nodes".into());
        }
        let id_n = *count;
        if self.eat_kw(Kw::Ask) {
            let question = self.string()?;
            let mut threshold = None;
            if self.eat_kw(Kw::Threshold) {
                let n = self.number()?;
                if !(0.0..=1.0).contains(&n) {
                    return Err(format!("THRESHOLD {n} is outside [0, 1]"));
                }
                threshold = Some(n);
            }
            let (takes, phrasings) = self.stack_opts()?;
            let (using, combine) = self.using_combine()?;
            self.expect_kw(Kw::Yes, "YES")?;
            self.eat_sym(':');
            let yes = self.tree_limited(depth + 1, count)?;
            self.expect_kw(Kw::No, "NO")?;
            self.eat_sym(':');
            let no = self.tree_limited(depth + 1, count)?;
            let mut node = json!({
                "kind": "ask",
                "id": format!("n{id_n}"),
                "question": question,
                "combine": combine,
                "yes": yes,
                "no": no
            });
            if let Some(t) = threshold {
                node["threshold"] = json!(t);
            }
            if let Some(n) = takes {
                node["takes"] = json!(n);
            }
            if !phrasings.is_empty() {
                node["phrasings"] = json!(phrasings);
            }
            if !using.is_empty() {
                node["using"] = json!(using);
            }
            return Ok(node);
        }
        if self.eat_kw(Kw::Choose) {
            let question = self.string()?;
            let (takes, phrasings) = self.stack_opts()?;
            let (using, combine) = self.using_combine()?;
            let mut when = Vec::new();
            if !self.eat_kw(Kw::When) {
                return Err(format!("CHOOSE requires WHEN near {}", self.near()));
            }
            loop {
                let label = self.string()?;
                let meaning = if self.eat_kw(Kw::Meaning) {
                    Some(self.string()?)
                } else {
                    None
                };
                self.expect_kw(Kw::Then, "THEN")?;
                let then = self.tree_limited(depth + 1, count)?;
                when.push(json!({"label": label, "meaning": meaning, "then": then}));
                if !self.eat_kw(Kw::When) {
                    break;
                }
            }
            if when.len() < 2 {
                return Err("CHOOSE needs at least two WHEN branches".into());
            }
            let mut node = json!({
                "kind": "choose",
                "id": format!("n{id_n}"),
                "question": question,
                "combine": combine,
                "when": when
            });
            if let Some(n) = takes {
                node["takes"] = json!(n);
            }
            if !phrasings.is_empty() {
                node["phrasings"] = json!(phrasings);
            }
            if !using.is_empty() {
                node["using"] = json!(using);
            }
            return Ok(node);
        }
        if self.eat_kw(Kw::True) {
            return Ok(json!({"kind": "leaf", "id": format!("n{id_n}"), "type": "bool", "value": true}));
        }
        if self.eat_kw(Kw::False) {
            return Ok(json!({"kind": "leaf", "id": format!("n{id_n}"), "type": "bool", "value": false}));
        }
        if self.at_string() {
            return Ok(json!({"kind": "leaf", "id": format!("n{id_n}"), "label": self.string()?}));
        }
        Err(format!("expected ASK, CHOOSE, TRUE, FALSE, or a leaf label near {}", self.near()))
    }

    /// `USING (slug, ...)` is the stack hook: those slugs are sent as
    /// `decision.models`. `COMBINE mean|vote` selects how they join.
    /// Absent USING is one call to the tree's model.
    fn using_combine(&mut self) -> Result<(Vec<String>, String), String> {
        let mut using = Vec::new();
        if self.eat_kw(Kw::Using) {
            self.expect_sym('(')?;
            loop {
                using.push(self.name()?);
                if !self.eat_sym(',') {
                    break;
                }
            }
            self.expect_sym(')')?;
            if using.len() < 2 {
                return Err("USING needs at least two models".into());
            }
        }
        let combine = if self.eat_kw(Kw::Combine) {
            let word = self.name()?;
            if word != "mean" && word != "vote" {
                return Err(format!("COMBINE {word} is not mean or vote"));
            }
            word
        } else {
            "mean".into()
        };
        Ok((using, combine))
    }

    /// `TAKES n` and `PHRASINGS ('...', ...)`. Absent means one wording, one take.
    fn stack_opts(&mut self) -> Result<(Option<i64>, Vec<String>), String> {
        let takes = if self.eat_kw(Kw::Takes) {
            Some(self.int()?)
        } else {
            None
        };
        let mut phrasings = Vec::new();
        if self.eat_kw(Kw::Phrasings) {
            self.expect_sym('(')?;
            if !self.at_sym(')') {
                loop {
                    phrasings.push(self.string()?);
                    if !self.eat_sym(',') {
                        break;
                    }
                }
            }
            self.expect_sym(')')?;
            if phrasings.len() < 2 {
                return Err("PHRASINGS needs at least two wordings".into());
            }
        }
        Ok((takes, phrasings))
    }

    fn arg_list(&mut self) -> Result<Vec<Arg>, String> {
        self.expect_sym('(')?;
        let mut args = Vec::new();
        if !self.at_sym(')') {
            loop {
                let name = self.name()?;
                let typ = self.typ_until(&[',', ')'])?;
                if typ.is_empty() {
                    return Err(format!("missing type for argument {name}"));
                }
                args.push(Arg { name, typ });
                if !self.eat_sym(',') {
                    break;
                }
            }
        }
        self.expect_sym(')')?;
        Ok(args)
    }

    fn typ(&mut self) -> Result<String, String> {
        // Stop before a clause keyword.
        let stop = [
            Kw::Model, Kw::System, Kw::Prompt, Kw::Max, Kw::Temperature, Kw::Cache, Kw::Test,
            Kw::Version, Kw::Steps, Kw::Takes, Kw::Ward, Kw::As, Kw::Labels, Kw::Default,
            Kw::Using,
        ];
        let mut depth = 0i32;
        let mut parts = Vec::new();
        if self.at_end() || self.at_sym(';') {
            return Err("expected a type".into());
        }
        loop {
            if depth == 0 {
                if let Some(Kind::Kw(k)) = self.peek_kind() {
                    if stop.contains(&k) {
                        break;
                    }
                }
                if self.at_sym(';') {
                    break;
                }
            }
            if self.at_end() {
                break;
            }
            if self.at_sym('(') {
                depth += 1;
                parts.push("(".into());
                self.bump();
                continue;
            }
            if self.at_sym(')') {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                parts.push(")".into());
                self.bump();
                continue;
            }
            parts.push(self.bump_text()?);
        }
        let s = join_type(&parts);
        if s.is_empty() {
            return Err("expected a type".into());
        }
        Ok(s)
    }

    fn typ_until(&mut self, stops: &[char]) -> Result<String, String> {
        let mut depth = 0i32;
        let mut parts = Vec::new();
        loop {
            if depth == 0 && stops.iter().any(|c| self.at_sym(*c)) {
                break;
            }
            if self.at_end() || (depth == 0 && self.at_sym(';')) {
                break;
            }
            if self.at_sym('(') {
                depth += 1;
                parts.push("(".into());
                self.bump();
                continue;
            }
            if self.at_sym(')') {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                parts.push(")".into());
                self.bump();
                continue;
            }
            parts.push(self.bump_text()?);
        }
        Ok(join_type(&parts))
    }

    fn opt_auth(&mut self) -> Result<Option<String>, String> {
        if !self.eat_kw(Kw::Auth) {
            return Ok(None);
        }
        Ok(Some(self.auth_after_auth()?))
    }

    fn auth_after_auth(&mut self) -> Result<String, String> {
        if self.eat_kw(Kw::Env) || self.eat_kw(Kw::Secret) {
            if self.at_string() {
                return self.string();
            }
            return self.name();
        }
        Err(format!("expected AUTH ENV or AUTH SECRET near {}", self.near()))
    }

    fn opt_with(&mut self) -> Result<WithOpts, String> {
        if !self.eat_kw(Kw::With) {
            return Ok(WithOpts::default());
        }
        self.with_parens()
    }

    fn with_parens(&mut self) -> Result<WithOpts, String> {
        self.expect_sym('(')?;
        let mut w = WithOpts::default();
        while !self.at_sym(')') && !self.at_end() {
            let key = self.name()?;
            self.expect_sym('=')?;
            match key.as_str() {
                "batch_size" => w.batch_size = Some(self.int()?),
                "max_concurrent" => w.max_concurrent = Some(self.int()?),
                "retries" => w.retries = Some(self.int()?),
                "timeout" => w.timeout_ms = Some(self.timeout()?),
                other => return Err(format!("unknown backend option {other}")),
            }
            self.eat_sym(',');
        }
        self.expect_sym(')')?;
        Ok(w)
    }

    fn timeout(&mut self) -> Result<i64, String> {
        if self.at_string() {
            let s = self.string()?;
            return parse_timeout(&s);
        }
        self.int()
    }

    fn opt_version(&mut self) -> Result<Option<i64>, String> {
        if self.eat_kw(Kw::Version) {
            Ok(Some(self.int()?))
        } else {
            Ok(None)
        }
    }

    fn eat_or_replace(&mut self) -> bool {
        if self.eat_kw(Kw::Or) {
            return self.eat_kw(Kw::Replace);
        }
        false
    }

    fn eat_if_not_exists(&mut self) -> bool {
        if self.eat_kw(Kw::If) && self.peek_kw(Kw::Not) {
            self.bump();
            return self.eat_kw(Kw::Exists);
        }
        false
    }

    fn eat_if_exists(&mut self) -> bool {
        if self.eat_kw(Kw::If) {
            return self.eat_kw(Kw::Exists);
        }
        false
    }

    fn negator_name(&mut self) -> Result<String, String> {
        let mut parts = vec![self.name()?];
        if self.peek_kind().is_some_and(|k| matches!(k, Kind::Ident(_) | Kind::Kw(_)))
            && !self.at_sym(',')
            && !self.at_sym(')')
        {
            // `NOT MEANS` — a second word, but stop at clause keywords that
            // would start the next item. The next item always follows a comma.
            if !self.at_kw_item_end() {
                parts.push(self.name()?);
            }
        }
        Ok(parts.join(" "))
    }

    fn at_kw_item_end(&self) -> bool {
        self.at_sym(',') || self.at_sym(')') || self.at_end()
    }

    fn rest_body(&mut self) -> Result<String, String> {
        if self.at_string() {
            return self.string();
        }
        let start = self.pos();
        let body = self.sql[start..].trim().trim_end_matches(';').trim().to_string();
        self.i = self.toks.len();
        if body.is_empty() {
            return Err("workflow body is empty".into());
        }
        Ok(body)
    }

    fn raw_parens(&mut self) -> Result<String, String> {
        let start = self.pos();
        self.expect_sym('(')?;
        let mut depth = 1i32;
        while depth > 0 {
            if self.at_end() {
                return Err("unbalanced '('".into());
            }
            if self.at_sym('(') {
                depth += 1;
            } else if self.at_sym(')') {
                depth -= 1;
            }
            self.bump();
        }
        let end = self.pos();
        Ok(self.sql[start..end].trim().to_string())
    }

    fn raw_until(&mut self, stop: &[Kw]) -> Result<String, String> {
        let start = self.pos();
        while !self.at_end() {
            if let Some(Kind::Kw(k)) = self.peek_kind() {
                if stop.contains(&k) {
                    break;
                }
            }
            self.bump();
        }
        Ok(self.sql[start..self.pos()].trim().to_string())
    }

    fn name(&mut self) -> Result<String, String> {
        match self.peek_kind() {
            Some(Kind::Ident(s)) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            Some(Kind::Kw(k)) => {
                // Quoted identifiers are Idents. A bare keyword used as a
                // name is accepted so object names can be words like "model"
                // only when the caller asked for a name, not a clause.
                let s = k.as_str().to_string();
                self.bump();
                Ok(s)
            }
            _ => Err(format!("expected a name near {}", self.near())),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        match self.peek_kind() {
            Some(Kind::Str(s)) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            _ => Err(format!("expected a string near {}", self.near())),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        let n = self.number()?;
        if n.fract() != 0.0 {
            return Err(format!("expected an integer near {n}"));
        }
        Ok(n as i64)
    }

    fn number(&mut self) -> Result<f64, String> {
        match self.peek_kind() {
            Some(Kind::Num(s)) => {
                let v: f64 = s.parse().map_err(|_| format!("bad number {s}"))?;
                self.bump();
                Ok(v)
            }
            _ => Err(format!("expected a number near {}", self.near())),
        }
    }

    fn unit_threshold(&mut self) -> Result<f64, String> {
        let n = self.number()?;
        if !(0.0..=1.0).contains(&n) {
            return Err(format!("THRESHOLD {n} is outside [0, 1]"));
        }
        Ok(n)
    }

    fn expect_kw(&mut self, kw: Kw, label: &str) -> Result<(), String> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(format!("expected {label} near {}", self.near()))
        }
    }

    fn expect_sym(&mut self, c: char) -> Result<(), String> {
        if self.eat_sym(c) {
            Ok(())
        } else {
            Err(format!("expected '{c}' near {}", self.near()))
        }
    }

    fn eat_kw(&mut self, kw: Kw) -> bool {
        if self.peek_kind() == Some(Kind::Kw(kw)) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek_kw(&self, kw: Kw) -> bool {
        self.peek_kind() == Some(Kind::Kw(kw))
    }

    fn eat_sym(&mut self, c: char) -> bool {
        if self.at_sym(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn at_sym(&self, c: char) -> bool {
        self.peek_kind() == Some(Kind::Sym(c))
    }

    fn at_string(&self) -> bool {
        matches!(self.peek_kind(), Some(Kind::Str(_)))
    }

    fn at_end(&self) -> bool {
        self.peek_kind().is_none() || self.peek_kind() == Some(Kind::Sym(';'))
    }

    fn skip_semi(&mut self) {
        self.eat_sym(';');
    }

    fn peek_kind(&self) -> Option<Kind> {
        self.toks.get(self.i).map(|t| t.kind.clone())
    }

    fn bump(&mut self) {
        if self.i < self.toks.len() {
            self.i += 1;
        }
    }

    fn bump_text(&mut self) -> Result<String, String> {
        let t = self.toks.get(self.i).ok_or_else(|| "unexpected end".to_string())?;
        let s = match &t.kind {
            Kind::Ident(s) | Kind::Str(s) | Kind::Num(s) => s.clone(),
            Kind::Kw(k) => k.as_str().to_string(),
            Kind::Sym(c) => c.to_string(),
        };
        self.i += 1;
        Ok(s)
    }

    fn pos(&self) -> usize {
        self.toks.get(self.i).map(|t| t.start).unwrap_or(self.sql.len())
    }

    fn near(&self) -> String {
        let start = self.pos().min(self.sql.len());
        let end = (start + 24).min(self.sql.len());
        self.sql[start..end].trim().to_string()
    }

    fn prev_text(&self) -> String {
        if self.i == 0 {
            return String::new();
        }
        match &self.toks[self.i - 1].kind {
            Kind::Kw(k) => k.as_str().to_string(),
            Kind::Ident(s) | Kind::Str(s) | Kind::Num(s) => s.clone(),
            Kind::Sym(c) => c.to_string(),
        }
    }
}

fn join_type(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && p != ")" && !out.ends_with('(') && p != "(" {
            out.push(' ');
        }
        out.push_str(p);
    }
    out
}

fn parse_timeout(s: &str) -> Result<i64, String> {
    let t = s.trim();
    if let Some(rest) = t.strip_suffix("ms") {
        return rest.trim().parse::<i64>().map_err(|_| format!("bad timeout {s}"));
    }
    if let Some(rest) = t.strip_suffix('s') {
        let n: f64 = rest.trim().parse().map_err(|_| format!("bad timeout {s}"))?;
        return Ok((n * 1000.0) as i64);
    }
    if let Some(rest) = t.strip_suffix('m') {
        let n: f64 = rest.trim().parse().map_err(|_| format!("bad timeout {s}"))?;
        return Ok((n * 60_000.0) as i64);
    }
    t.parse::<i64>().map_err(|_| format!("bad timeout {s}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kw {
    Create, Alter, Drop, Grant, Revoke, Comment, Or, Replace, If, Not, Exists,
    Backend, Model, Embedding, Semantic, Function, Operator, Decision, Tree, Workflow,
    Type, Url, Auth, Env, Secret, With, On, Name, Cost, Input, Output, Dimensions,
    Returns, System, Prompt, Max, Tokens, Temperature, Cache, Test, Expect, For,
    Leftarg, Rightarg, Symbol, Negator, Ask, Yes, No, Choose, When, Then, Meaning,
    Threshold, Using, Combine, As, Version, Set, Cascade, Restrict, Usage, Execute, To, From, Is,
    Null, Steps, Takes, Ward, Labels, True, False, Default, Clauses, Phrasings, Pipe,
}

impl Kw {
    fn as_str(self) -> &'static str {
        match self {
            Kw::Create => "create",
            Kw::Alter => "alter",
            Kw::Drop => "drop",
            Kw::Grant => "grant",
            Kw::Revoke => "revoke",
            Kw::Comment => "comment",
            Kw::Or => "or",
            Kw::Replace => "replace",
            Kw::If => "if",
            Kw::Not => "not",
            Kw::Exists => "exists",
            Kw::Backend => "backend",
            Kw::Model => "model",
            Kw::Embedding => "embedding",
            Kw::Semantic => "semantic",
            Kw::Function => "function",
            Kw::Operator => "operator",
            Kw::Decision => "decision",
            Kw::Tree => "tree",
            Kw::Workflow => "workflow",
            Kw::Type => "type",
            Kw::Url => "url",
            Kw::Auth => "auth",
            Kw::Env => "env",
            Kw::Secret => "secret",
            Kw::With => "with",
            Kw::On => "on",
            Kw::Name => "name",
            Kw::Cost => "cost",
            Kw::Input => "input",
            Kw::Output => "output",
            Kw::Dimensions => "dimensions",
            Kw::Returns => "returns",
            Kw::System => "system",
            Kw::Prompt => "prompt",
            Kw::Max => "max",
            Kw::Tokens => "tokens",
            Kw::Temperature => "temperature",
            Kw::Cache => "cache",
            Kw::Test => "test",
            Kw::Expect => "expect",
            Kw::For => "for",
            Kw::Leftarg => "leftarg",
            Kw::Rightarg => "rightarg",
            Kw::Symbol => "symbol",
            Kw::Negator => "negator",
            Kw::Ask => "ask",
            Kw::Yes => "yes",
            Kw::No => "no",
            Kw::Choose => "choose",
            Kw::When => "when",
            Kw::Then => "then",
            Kw::Meaning => "meaning",
            Kw::Threshold => "threshold",
            Kw::Using => "using",
            Kw::Combine => "combine",
            Kw::As => "as",
            Kw::Version => "version",
            Kw::Set => "set",
            Kw::Cascade => "cascade",
            Kw::Restrict => "restrict",
            Kw::Usage => "usage",
            Kw::Execute => "execute",
            Kw::To => "to",
            Kw::From => "from",
            Kw::Is => "is",
            Kw::Null => "null",
            Kw::Steps => "steps",
            Kw::Takes => "takes",
            Kw::Ward => "ward",
            Kw::Labels => "labels",
            Kw::True => "true",
            Kw::False => "false",
            Kw::Default => "default",
            Kw::Clauses => "clauses",
            Kw::Phrasings => "phrasings",
            Kw::Pipe => "pipe",
        }
    }

    fn from_ident(s: &str) -> Option<Self> {
        Some(match s {
            "create" => Kw::Create,
            "alter" => Kw::Alter,
            "drop" => Kw::Drop,
            "grant" => Kw::Grant,
            "revoke" => Kw::Revoke,
            "comment" => Kw::Comment,
            "or" => Kw::Or,
            "replace" => Kw::Replace,
            "if" => Kw::If,
            "not" => Kw::Not,
            "exists" => Kw::Exists,
            "backend" => Kw::Backend,
            "model" => Kw::Model,
            "embedding" => Kw::Embedding,
            "semantic" => Kw::Semantic,
            "function" => Kw::Function,
            "operator" => Kw::Operator,
            "decision" => Kw::Decision,
            "tree" => Kw::Tree,
            "workflow" => Kw::Workflow,
            "type" => Kw::Type,
            "url" => Kw::Url,
            "auth" => Kw::Auth,
            "env" => Kw::Env,
            "secret" => Kw::Secret,
            "with" => Kw::With,
            "on" => Kw::On,
            "name" => Kw::Name,
            "cost" => Kw::Cost,
            "input" => Kw::Input,
            "output" => Kw::Output,
            "dimensions" => Kw::Dimensions,
            "returns" => Kw::Returns,
            "system" => Kw::System,
            "prompt" => Kw::Prompt,
            "max" => Kw::Max,
            "tokens" => Kw::Tokens,
            "temperature" => Kw::Temperature,
            "cache" => Kw::Cache,
            "test" => Kw::Test,
            "expect" => Kw::Expect,
            "for" => Kw::For,
            "leftarg" => Kw::Leftarg,
            "rightarg" => Kw::Rightarg,
            "symbol" => Kw::Symbol,
            "negator" => Kw::Negator,
            "ask" => Kw::Ask,
            "yes" => Kw::Yes,
            "no" => Kw::No,
            "choose" => Kw::Choose,
            "when" => Kw::When,
            "then" => Kw::Then,
            "meaning" => Kw::Meaning,
            "threshold" => Kw::Threshold,
            "using" => Kw::Using,
            "combine" => Kw::Combine,
            "true" => Kw::True,
            "false" => Kw::False,
            "as" => Kw::As,
            "version" => Kw::Version,
            "set" => Kw::Set,
            "cascade" => Kw::Cascade,
            "restrict" => Kw::Restrict,
            "usage" => Kw::Usage,
            "execute" => Kw::Execute,
            "to" => Kw::To,
            "from" => Kw::From,
            "is" => Kw::Is,
            "null" => Kw::Null,
            "steps" => Kw::Steps,
            "takes" => Kw::Takes,
            "ward" => Kw::Ward,
            "default" => Kw::Default,
            "clauses" => Kw::Clauses,
            "phrasings" => Kw::Phrasings,
            "pipe" => Kw::Pipe,
            "labels" => Kw::Labels,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Kw(Kw),
    Ident(String),
    Str(String),
    Num(String),
    Sym(char),
}

mod lexer {
    use super::{Kind, Kw};

    #[derive(Clone, Debug)]
    pub struct Tok {
        pub kind: Kind,
        pub start: usize,
    }

    impl Tok {
        pub fn kw(&self) -> Option<Kw> {
            match &self.kind {
                Kind::Kw(k) => Some(*k),
                _ => None,
            }
        }
    }

    pub fn tokenize(sql: &str) -> Vec<Tok> {
        let b = sql.as_bytes();
        let mut i = 0;
        let mut out = Vec::new();
        while i < b.len() {
            let c = b[i] as char;
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if c == '-' && i + 1 < b.len() && b[i + 1] == b'-' {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if c == '/' && i + 1 < b.len() && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            if c == '\'' {
                let start = i;
                i += 1;
                let mut s = String::new();
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    s.push(b[i] as char);
                    i += 1;
                }
                out.push(Tok { kind: Kind::Str(s), start });
                continue;
            }
            if c == '$' {
                if let Some((end, body)) = dollar(sql, i) {
                    out.push(Tok { kind: Kind::Str(body), start: i });
                    i = end;
                    continue;
                }
            }
            if c == '"' {
                let start = i;
                i += 1;
                let mut s = String::new();
                while i < b.len() && b[i] != b'"' {
                    s.push(b[i] as char);
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
                out.push(Tok { kind: Kind::Ident(s), start });
                continue;
            }
            if c.is_ascii_digit() || (c == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                out.push(Tok {
                    kind: Kind::Num(sql[start..i].to_string()),
                    start,
                });
                continue;
            }
            // Model slugs (`typesafe/jev-1.14`, `~typesafe/jev-latest`) are one
            // ident. `/`, `-`, `.`, and `~` continue a word; a bare `.` stays
            // skipped punctuation so it is not a schema separator token.
            if c.is_ascii_alphabetic() || c == '_' || c == '~' {
                let start = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'_'
                        || ((b[i] == b'/' || b[i] == b'-' || b[i] == b'.' || b[i] == b'~')
                            && i + 1 < b.len()
                            && (b[i + 1].is_ascii_alphanumeric() || b[i + 1] == b'_' || b[i + 1] == b'~')))
                {
                    i += 1;
                }
                let raw = &sql[start..i];
                let folded = raw.to_ascii_lowercase();
                let kind = if raw.chars().any(|ch| ch.is_ascii_uppercase()) || true {
                    match Kw::from_ident(&folded) {
                        Some(k) => Kind::Kw(k),
                        None => Kind::Ident(folded),
                    }
                } else {
                    Kind::Ident(folded)
                };
                out.push(Tok { kind, start });
                continue;
            }
            if "()=,.;:".contains(c) {
                out.push(Tok { kind: Kind::Sym(c), start: i });
                i += 1;
                continue;
            }
            // Skip unknown punctuation (for example `|` in a workflow body
            // that was tokenized only for classification).
            i += 1;
        }
        out
    }

    fn dollar(sql: &str, start: usize) -> Option<(usize, String)> {
        let b = sql.as_bytes();
        if start >= b.len() || b[start] != b'$' {
            return None;
        }
        let mut j = start + 1;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        if j >= b.len() || b[j] != b'$' {
            return None;
        }
        let tag = &sql[start..=j];
        let body_start = j + 1;
        let rest = &sql[body_start..];
        let k = rest.find(tag)?;
        Some((body_start + k + tag.len(), rest[..k].to_string()))
    }
}

/// `bool`, `score`, and `label` are spellings of the stored SQL types.
/// `label` still needs `LABELS`.
fn normalize_returns(raw: &str) -> Result<(String, bool), String> {
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "bool" | "boolean" => Ok(("boolean".into(), false)),
        "score" => Ok(("double precision".into(), false)),
        "label" => Ok(("text".into(), true)),
        _ => Ok((trimmed.to_string(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_sql_is_not_split() {
        assert!(split_if_rvbbit("SELECT 1 AS backend; SELECT means FROM t").is_none());
        let s = "SELECT 'CREATE BACKEND x' ;";
        assert!(split_if_rvbbit(s).is_none());
        let guarded = split_if_rvbbit(
            "CREATE IF NOT EXISTS BACKEND hutch TYPE hutch URL 'http://h'; \
             CREATE OR REPLACE IF NOT EXISTS MODEL m ON BACKEND hutch NAME 'remote';",
        )
        .expect("guarded create");
        assert!(guarded.iter().all(|p| p.rvbbit));
    }

    #[test]
    fn mixed_script_splits_on_the_semicolon() {
        let sql = "CREATE BACKEND hutch TYPE hutch URL 'http://h'; SELECT 1;";
        let pieces = split_if_rvbbit(sql).expect("split");
        assert_eq!(pieces.len(), 2);
        assert!(pieces[0].rvbbit);
        assert!(!pieces[1].rvbbit);
        assert_eq!(pieces[1].sql.trim(), "SELECT 1;");
    }

    #[test]
    fn parses_the_proposal_shapes() {
        let b = parse_command(
            "CREATE BACKEND hutch TYPE hutch URL 'https://hutch.rvbbit.ai' \
             AUTH SECRET hutch_key WITH (batch_size = 32, max_concurrent = 4, timeout = '30s', retries = 3)",
        )
        .unwrap();
        match b {
            Command::CreateBackend { auth_env, with, .. } => {
                assert_eq!(auth_env.as_deref(), Some("hutch_key"));
                assert_eq!(with.timeout_ms, Some(30_000));
                assert_eq!(with.batch_size, Some(32));
            }
            other => panic!("{other:?}"),
        }

        let f = parse_command(
            "CREATE SEMANTIC FUNCTION means(text text, criterion text) RETURNS boolean \
             MODEL fast SYSTEM $$sys$$ PROMPT $$TEXT {{text}}$$ MAX TOKENS 16 CACHE memoize \
             TEST obvious_positive EXPECT true FOR ('a', 'b')",
        )
        .unwrap();
        match f {
            Command::CreateFunction { name, args, clauses, .. } => {
                assert_eq!(name, "means");
                assert_eq!(args.len(), 2);
                assert_eq!(clauses.model.as_deref(), Some("fast"));
                assert_eq!(clauses.system.as_deref(), Some("sys"));
                assert_eq!(clauses.max_tokens, Some(16));
                assert_eq!(clauses.tests.len(), 1);
                assert!(clauses.labels.is_empty());
            }
            other => panic!("{other:?}"),
        }

        let kind = parse_command(
            "CREATE SEMANTIC FUNCTION kind(notes text) RETURNS text MODEL jev \
             LABELS ('bigfoot', 'known animal', 'unclear')",
        )
        .unwrap();
        match kind {
            Command::CreateFunction { returns, clauses, .. } => {
                assert_eq!(returns, "text");
                assert_eq!(
                    clauses.labels,
                    vec!["bigfoot".to_string(), "known animal".to_string(), "unclear".to_string()]
                );
                assert!(clauses.label_descriptions.iter().all(|d| d.is_none()));
            }
            other => panic!("{other:?}"),
        }
        let described = parse_command(
            "CREATE SEMANTIC FUNCTION kind(notes text) RETURNS text LABELS (\
             'bigfoot' AS 'an unidentified large primate or sasquatch', \
             'known animal', \
             'unclear' AS 'not enough to tell')",
        )
        .unwrap();
        match described {
            Command::CreateFunction { clauses, .. } => {
                assert_eq!(clauses.labels[0], "bigfoot");
                assert_eq!(
                    clauses.label_descriptions[0].as_deref(),
                    Some("an unidentified large primate or sasquatch")
                );
                assert_eq!(clauses.label_descriptions[1], None);
                assert_eq!(clauses.label_descriptions[2].as_deref(), Some("not enough to tell"));
            }
            other => panic!("{other:?}"),
        }
        let one = parse_command(
            "CREATE SEMANTIC FUNCTION kind(notes text) RETURNS text LABELS ('only')",
        );
        assert!(one.unwrap_err().contains("at least two"));

        let op = parse_command(
            "CREATE SEMANTIC OPERATOR MEANS (LEFTARG = text, RIGHTARG = text, FUNCTION = means, SYMBOL = '~~?', NEGATOR = NOT MEANS)",
        )
        .unwrap();
        match op {
            Command::CreateOperator { word, negator, symbol, .. } => {
                assert_eq!(word, "means");
                assert_eq!(symbol.as_deref(), Some("~~?"));
                assert_eq!(negator.as_deref(), Some("not means"));
            }
            other => panic!("{other:?}"),
        }

        let tree = parse_command(
            "CREATE DECISION TREE churn_policy (recent_messages text, account_summary text) \
             MODEL jev AS \
             ASK 'risk?' THRESHOLD 0.5 \
               YES: ASK 'service?' YES: 'service intervention' NO: 'relationship review' \
               NO: 'routine follow-up'",
        )
        .unwrap();
        match tree {
            Command::CreateTree { nodes, .. } => {
                assert_eq!(nodes["kind"], "ask");
                assert_eq!(nodes["yes"]["kind"], "ask");
                assert_eq!(nodes["no"]["label"], "routine follow-up");
            }
            other => panic!("{other:?}"),
        }

        let choose = parse_command(
            "CREATE DECISION TREE help_route (message text) MODEL jev AS \
             CHOOSE 'help?' \
               WHEN 'billing' MEANING 'invoices' THEN 'finance' \
               WHEN 'unclear' MEANING 'none' THEN 'needs review'",
        )
        .unwrap();
        match choose {
            Command::CreateTree { nodes, .. } => {
                assert_eq!(nodes["kind"], "choose");
                assert_eq!(nodes["when"][0]["then"]["label"], "finance");
            }
            other => panic!("{other:?}"),
        }

        let follow = parse_command(
            "CREATE DECISION TREE bigfoot_triage (notes text) MODEL jev AS \
             ASK 'credible?' USING (typesafe/high, typesafe/jev-1.14) COMBINE vote \
               YES: TRUE NO: FALSE",
        )
        .unwrap();
        match follow {
            Command::CreateTree { nodes, .. } => {
                assert_eq!(nodes["id"], "n1");
                assert_eq!(nodes["combine"], "vote");
                assert_eq!(nodes["using"][0], "typesafe/high");
                assert_eq!(nodes["using"][1], "typesafe/jev-1.14");
                assert_eq!(nodes["yes"]["value"], true);
                assert_eq!(nodes["yes"]["id"], "n2");
                assert_eq!(nodes["no"]["value"], false);
            }
            other => panic!("{other:?}"),
        }

        let stacked = parse_command(
            "CREATE DECISION TREE ufo_stack (notes text) MODEL jev AS \
             ASK 'sky?' YES: CHOOSE 'What is the most likely explanation?' \
               TAKES 3 PHRASINGS ('What is the most likely explanation?', 'Which ordinary cause fits?') \
               WHEN 'aircraft' THEN 'aircraft' WHEN 'other' THEN 'other' NO: FALSE",
        )
        .unwrap();
        match stacked {
            Command::CreateTree { nodes, .. } => {
                assert_eq!(nodes["yes"]["takes"], 3);
                assert_eq!(nodes["yes"]["phrasings"][1], "Which ordinary cause fits?");
            }
            other => panic!("{other:?}"),
        }
        let defaults = parse_command(
            "CREATE SEMANTIC FUNCTION means(input text, criterion text) RETURNS boolean \
             DEFAULT CLAUSES ($$WITH TAKES 3 PICK BY VOTE$$) MODEL jev",
        )
        .unwrap();
        match defaults {
            Command::CreateFunction { clauses, .. } => {
                assert_eq!(clauses.clauses.as_deref(), Some("WITH TAKES 3 PICK BY VOTE"));
            }
            other => panic!("{other:?}"),
        }

        let sugar = parse_command(
            "CREATE SEMANTIC FUNCTION is_biped(notes text) RETURNS bool \
             AS 'large hairy biped' USING MODEL jev DEFAULT CLAUSES ('WITH TAKES 1')",
        )
        .unwrap();
        match sugar {
            Command::CreateFunction { returns, clauses, args, .. } => {
                assert_eq!(returns, "boolean");
                assert_eq!(args[0].name, "notes");
                assert_eq!(clauses.prompt.as_deref(), Some("large hairy biped"));
                assert_eq!(clauses.model.as_deref(), Some("jev"));
                assert_eq!(clauses.clauses.as_deref(), Some("WITH TAKES 1"));
                assert!(clauses.tree.is_none() && clauses.pipe.is_none());
            }
            other => panic!("{other:?}"),
        }
        let score = parse_command(
            "CREATE SEMANTIC FUNCTION how_biped(notes text) RETURNS score AS '{{notes}} is a biped'",
        )
        .unwrap();
        match score {
            Command::CreateFunction { returns, clauses, .. } => {
                assert_eq!(returns, "double precision");
                assert_eq!(clauses.prompt.as_deref(), Some("{{notes}} is a biped"));
            }
            other => panic!("{other:?}"),
        }
        let label = parse_command(
            "CREATE SEMANTIC FUNCTION kind(notes text) RETURNS label \
             LABELS ('bigfoot' AS 'a large primate', 'other') USING MODEL jev",
        )
        .unwrap();
        match label {
            Command::CreateFunction { returns, clauses, .. } => {
                assert_eq!(returns, "text");
                assert!(clauses.returns_label);
                assert_eq!(clauses.labels.len(), 2);
                assert_eq!(clauses.model.as_deref(), Some("jev"));
            }
            other => panic!("{other:?}"),
        }
        let tree = parse_command(
            "CREATE SEMANTIC FUNCTION sighting_of(notes text) RETURNS text AS DECISION TREE sighting",
        )
        .unwrap();
        match tree {
            Command::CreateFunction { clauses, .. } => {
                assert_eq!(clauses.tree.as_deref(), Some("sighting"));
            }
            other => panic!("{other:?}"),
        }
        let pipe = parse_command(
            "CREATE SEMANTIC FUNCTION creature_of(notes text) RETURNS text AS PIPE $$ {{notes}} |> EXTRACT creature $$",
        )
        .unwrap();
        match pipe {
            Command::CreateFunction { clauses, .. } => {
                assert_eq!(clauses.pipe.as_deref(), Some(" {{notes}} |> EXTRACT creature "));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_command(
            "CREATE SEMANTIC FUNCTION kind(notes text) RETURNS label LABELS ('only')"
        )
        .unwrap_err()
        .contains("at least two"));

        parse_command(
            "CREATE WORKFLOW triage(tickets text) RETURNS TABLE (id bigint, route text) VERSION 2 AS $$\nFROM tickets\n$$",
        )
        .unwrap();

        parse_command("GRANT EXECUTE ON SEMANTIC FUNCTION means(text text, criterion text) TO reader").unwrap();
        parse_command("COMMENT ON BACKEND hutch IS 'local'").unwrap();
        parse_command("DROP BACKEND hutch CASCADE").unwrap();
    }
}
