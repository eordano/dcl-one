//! The layout this binary's `schema.sql` declares, checked against a stamped
//! database, and the additive migration that brings an older one up to it.
//!
//! `schema apply` runs `schema.sql` once, on an empty database; a node
//! stamped by an earlier binary lacks every relation, type, function and
//! trigger added since (the closure pass alone added 38 tables and the
//! hash-named content write functions), and its existing tables lack the
//! columns later releases declared on them (the closure base's
//! `credits.purchases` has no `quantity` or `source`). `catalyst schema
//! migrate` (the administrative role, the schema oneshot on every activation)
//! first compares `meta.schema` with this binary's identity and version (a
//! different one is the explicit data-safe release procedure, never this
//! command), then, in one transaction: applies the active steps of
//! `schemas/migrations/` the database has not recorded in `meta.schema_steps`
//! (each one additive by construction, see [`classify_step`]; a recorded
//! step whose text changed is refused; release-retired SQL never runs or gains
//! a new receipt), creates each object of `schema.sql`
//! the database does not have, statement by statement in file order, with
//! the statements that only make sense for a new object (`ALTER SEQUENCE ...
//! OWNED BY`, seed `INSERT`s) applied only when that object was created in
//! the same run, with the layout checked between the two: a column an
//! existing table still lacks after the steps is drift no step covers,
//! reported with the table and column names, and the whole transaction is
//! rolled back. The exact pending receipt-retirement step can first create its
//! missing canonical receipt prerequisites; these share the same transaction
//! and object accounting, so drift rolls them back too. It never
//! alters, drops or resets anything a step does not spell out, and a step
//! may not drop, rename, retype or delete. `db::bootstrap` refuses to start
//! serve on a database missing any relation, type, function or column of the
//! layout or retaining the exact release-retired voice objects (indexes only
//! warn when missing), naming them and `catalyst schema migrate`;
//! `catalyst schema status` prints the same comparison for an operator.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;

/// The top-level statements of `sql` (comments stripped at the edges, no
/// trailing `;`): quotes, quoted identifiers, dollar-quoted bodies and
/// comments never split a statement.
pub fn statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'\'' | b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == c {
                        if bytes.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'$' => {
                let tag_end = bytes[i + 1..]
                    .iter()
                    .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                    .map(|p| i + 1 + p);
                match tag_end {
                    Some(e) if bytes[e] == b'$' => {
                        let tag = &sql[i..=e];
                        match sql[e + 1..].find(tag) {
                            Some(close) => i = e + 1 + close + tag.len(),
                            None => i = bytes.len(),
                        }
                    }
                    _ => i += 1,
                }
            }
            b';' => {
                push_statement(&mut out, &sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_statement(&mut out, &sql[start..]);
    out
}

fn push_statement(out: &mut Vec<String>, raw: &str) {
    let text = strip_leading_comments(raw).trim();
    if !text.is_empty() {
        out.push(text.to_string());
    }
}

fn strip_leading_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest
                .split_once('\n')
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest
                .split_once("*/")
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
        } else {
            return s;
        }
    }
}

/// One object `schema.sql` creates, by the name its catalog lookup needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Object {
    Schema(String),
    /// A table, view or sequence (`schema.name`).
    Relation(String),
    /// An index, qualified with its table's schema.
    Index(String),
    Type(String),
    Function {
        schema: String,
        name: String,
    },
    Trigger {
        name: String,
        table: String,
    },
}

impl std::fmt::Display for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Object::Schema(s) => write!(f, "schema {s}"),
            Object::Relation(r) => write!(f, "relation {r}"),
            Object::Index(r) => write!(f, "index {r}"),
            Object::Type(t) => write!(f, "type {t}"),
            Object::Function { schema, name } => write!(f, "function {schema}.{name}"),
            Object::Trigger { name, table } => write!(f, "trigger {name} on {table}"),
        }
    }
}

/// What a statement is to the migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Creates `object`; a `CREATE TABLE` also names its columns.
    Create {
        object: Object,
        columns: Vec<String>,
    },
    /// Belongs to the object (`ALTER SEQUENCE ... OWNED BY`, a seed `INSERT`,
    /// `COMMENT ON`): applied only in the run that created it.
    FollowUp(Object),
    /// Idempotent by construction (the database-default `DO` block).
    Always,
}

#[derive(Debug, Clone)]
pub struct Planned {
    pub step: Step,
    pub sql: String,
}

fn words(stmt: &str) -> Vec<&str> {
    stmt.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|w| !w.is_empty())
        .collect()
}

fn upper(w: &str) -> String {
    w.to_ascii_uppercase()
}

/// `stmt` split at its top-level commas (outside parentheses and quotes).
fn top_level_comma_parts(stmt: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quote) = (0i32, 0usize, None::<u8>);
    for (i, c) in stmt.bytes().enumerate() {
        match c {
            _ if quote == Some(c) => quote = None,
            _ if quote.is_some() => {}
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&stmt[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&stmt[start..]);
    parts
}

/// `name` qualified with `schema_of`'s schema when it has none.
fn qualify(name: &str, schema_of: &str) -> String {
    if name.contains('.') {
        name.to_string()
    } else {
        format!(
            "{}.{name}",
            schema_of
                .split_once('.')
                .map(|(s, _)| s)
                .unwrap_or("public")
        )
    }
}

fn split_qualified(name: &str) -> (String, String) {
    match name.split_once('.') {
        Some((s, n)) => (s.to_string(), n.to_string()),
        None => ("public".to_string(), name.to_string()),
    }
}

/// The column names a `CREATE TABLE` body declares at its top level (table
/// constraints aside).
pub fn table_columns(stmt: &str) -> Vec<String> {
    let Some(open) = stmt.find('(') else {
        return Vec::new();
    };
    let body = &stmt[open + 1..];
    let mut items = Vec::new();
    let (mut depth, mut start, mut quote) = (0i32, 0usize, None::<u8>);
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' if depth == 0 => {
                items.push(&body[start..i]);
                break;
            }
            b')' => depth -= 1,
            b',' if depth == 0 => {
                items.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    const CONSTRAINTS: [&str; 7] = [
        "CONSTRAINT",
        "PRIMARY",
        "UNIQUE",
        "CHECK",
        "FOREIGN",
        "EXCLUDE",
        "LIKE",
    ];
    items
        .into_iter()
        .filter_map(|item| {
            let item = strip_leading_comments(item.trim());
            let first = item.split_whitespace().next()?;
            (!CONSTRAINTS.contains(&upper(first).as_str()))
                .then(|| first.trim_matches('"').to_string())
        })
        .collect()
}

/// `stmt`'s role in the migration, or `None` for a statement the migration
/// does not know (refused: the layout must be fully classified).
pub fn classify(stmt: &str) -> Option<Step> {
    let w = words(stmt);
    let kw: Vec<String> = w.iter().take(6).map(|x| upper(x)).collect();
    let at = |i: usize| w.get(i).map(|s| s.trim_end_matches(';').to_string());
    let create = |object: Object| {
        Some(Step::Create {
            object,
            columns: Vec::new(),
        })
    };
    match kw.as_slice() {
        [a, b, ..] if a == "CREATE" && b == "SCHEMA" => create(Object::Schema(at(2)?)),
        [a, b, ..] if a == "CREATE" && b == "TABLE" => Some(Step::Create {
            object: Object::Relation(at(2)?),
            columns: table_columns(stmt),
        }),
        [a, b, ..] if a == "CREATE" && (b == "SEQUENCE" || b == "VIEW") => {
            create(Object::Relation(at(2)?))
        }
        [a, b, c, ..] if a == "CREATE" && b == "MATERIALIZED" && c == "VIEW" => {
            create(Object::Relation(at(3)?))
        }
        [a, b, ..] if a == "CREATE" && b == "TYPE" => create(Object::Type(at(2)?)),
        [a, b, c, ..] if a == "CREATE" && (b == "INDEX" || (b == "UNIQUE" && c == "INDEX")) => {
            let name_at = if b == "UNIQUE" { 3 } else { 2 };
            let on = w.iter().position(|x| upper(x) == "ON")?;
            let mut table = at(on + 1)?;
            if upper(&table) == "ONLY" {
                table = at(on + 2)?;
            }
            create(Object::Index(qualify(&at(name_at)?, &table)))
        }
        [a, b, ..] if a == "CREATE" && b == "FUNCTION" => {
            let head = stmt.split('(').next()?;
            let name = head.split_whitespace().last()?;
            let (schema, name) = split_qualified(name);
            create(Object::Function { schema, name })
        }
        [a, b, c, d, ..] if a == "CREATE" && b == "OR" && c == "REPLACE" && d == "FUNCTION" => {
            let head = stmt.split('(').next()?;
            let name = head.split_whitespace().last()?;
            let (schema, name) = split_qualified(name);
            create(Object::Function { schema, name })
        }
        [a, b, ..] if a == "CREATE" && b == "TRIGGER" => {
            let on = w.iter().position(|x| upper(x) == "ON")?;
            create(Object::Trigger {
                name: at(2)?,
                table: at(on + 1)?,
            })
        }
        [a, b, ..] if a == "ALTER" && b == "SEQUENCE" => {
            Some(Step::FollowUp(Object::Relation(at(2)?)))
        }
        [a, b, ..] if a == "INSERT" && b == "INTO" => {
            Some(Step::FollowUp(Object::Relation(at(2)?)))
        }
        [a, b, c, ..] if a == "COMMENT" && b == "ON" && (c == "TABLE" || c == "VIEW") => {
            Some(Step::FollowUp(Object::Relation(at(3)?)))
        }
        [a, b, c, ..] if a == "COMMENT" && b == "ON" && c == "COLUMN" => {
            let col = at(3)?;
            let table = col.rsplit_once('.').map(|(t, _)| t.to_string())?;
            Some(Step::FollowUp(Object::Relation(table)))
        }
        [a, ..] if a == "DO" => Some(Step::Always),
        _ => None,
    }
}

const STEP_MARKER: &str = "-- ════ step ";

/// One file of `schemas/migrations/`, as build.rs rendered it into
/// `db::MIGRATIONS_SQL`: its name (the file stem, `NNNN_<what>`), its text
/// and the digest `meta.schema_steps` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStep {
    pub name: String,
    pub sql: String,
    pub sha256: String,
}

/// The steps of the rendering, in the order they apply (build.rs sorts the
/// files by name, so the `NNNN_` prefix is the order).
pub fn parse_steps(rendered: &str) -> Vec<MigrationStep> {
    let mut steps: Vec<MigrationStep> = Vec::new();
    for line in rendered.lines() {
        if let Some(rest) = line.strip_prefix(STEP_MARKER) {
            let name = rest.trim().trim_end_matches('═').trim().to_string();
            steps.push(MigrationStep {
                name,
                sql: String::new(),
                sha256: String::new(),
            });
        } else if let Some(step) = steps.last_mut() {
            step.sql.push_str(line);
            step.sql.push('\n');
        }
    }
    for step in &mut steps {
        step.sql = format!("{}\n", step.sql.trim());
        step.sha256 = hex::encode(Sha256::digest(step.sql.as_bytes()));
    }
    steps
}

/// What a build migrates a database to, one line each: the identity and
/// version it stamps, the digest of its schema.sql and each step's name and
/// digest. Two builds with the same layout migrate a database alike; one
/// whose layout differs may need `schema migrate` before it serves
/// (`catalyst schema layout`; local.sh compares the dev build's with the
/// commit build's before the dev build may touch a shared database).
pub fn layout(identity: &str, version: i32, schema_sql: &str, migrations_sql: &str) -> String {
    let mut out = format!(
        "identity {identity}\nversion {version}\nschema {}\n",
        hex::encode(Sha256::digest(schema_sql.as_bytes()))
    );
    for step in parse_steps(migrations_sql) {
        out.push_str(&format!("step {} {}\n", step.name, step.sha256));
    }
    out
}

/// How one statement of a step runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatement {
    /// Idempotent as written (`ADD COLUMN IF NOT EXISTS`, `DROP CONSTRAINT IF
    /// EXISTS`, a default, a backfill `UPDATE`, ...): run as is.
    Plain,
    /// `ALTER TABLE t ADD CONSTRAINT name ...`: skipped when a constraint of
    /// that name is already on the table (a database whose schema.sql
    /// already declared it, recorded or not).
    AddConstraint { table: String, name: String },
    /// `CREATE FUNCTION s.name_<hash>(...)`: a hash-named function the
    /// step's columns call, skipped when the schema already has it (the
    /// name pins the definition, so it is the same function).
    CreateFunction { schema: String, name: String },
}

/// The forms a step may take. Additive only: a step adds a column (with `IF
/// NOT EXISTS`), a constraint, a default, an enum value, drops a constraint,
/// a default or a NOT NULL (`IF EXISTS` where the syntax has it), creates a
/// hash-named function (`name_<12 hex digits>`, skipped when present),
/// replaces a function body, comments, or backfills with `UPDATE`. Dropping, renaming or
/// retyping a table, column, schema or type, `DELETE`, `TRUNCATE`, `SET NOT
/// NULL` (fails on rows the step did not backfill) and `DO` blocks are
/// refused with the reason: those are the explicit data-safe release
/// procedure, reviewed as their own change, never a step the oneshot runs
/// on every activation. An `ALTER TABLE` with several actions is taken when
/// every one is `ADD COLUMN IF NOT EXISTS` (one rewrite of the table for all
/// of them) and refused when any is another action.
pub fn classify_step(stmt: &str) -> Result<StepStatement, String> {
    let w = words(stmt);
    let kw: Vec<String> = w.iter().map(|x| upper(x)).collect();
    let k = |i: usize| kw.get(i).map(String::as_str).unwrap_or("");
    let at = |i: usize| {
        w.get(i)
            .map(|s| s.trim_end_matches(';').to_string())
            .unwrap_or_default()
    };
    let refuse = |why: &str| {
        Err(format!(
            "{why}: {}",
            stmt.chars().take(100).collect::<String>()
        ))
    };
    match (k(0), k(1)) {
        ("ALTER", "TABLE") => {
            let mut i = 2;
            if k(i) == "IF" && k(i + 1) == "EXISTS" {
                i += 2;
            }
            if k(i) == "ONLY" {
                i += 1;
            }
            let table = at(i);
            let actions = top_level_comma_parts(stmt);
            if actions.len() > 1 {
                let add_column = |ws: &[&str]| {
                    ws.len() >= 5
                        && ws
                            .iter()
                            .zip(["ADD", "COLUMN", "IF", "NOT", "EXISTS"])
                            .all(|(word, expected)| upper(word) == expected)
                };
                return if add_column(w.get(i + 1..).unwrap_or(&[]))
                    && actions[1..].iter().all(|a| add_column(&words(a)))
                {
                    Ok(StepStatement::Plain)
                } else {
                    refuse(
                        "an ALTER TABLE with several actions is every one ADD COLUMN IF NOT EXISTS (the table is rewritten once for all of them); any other action takes its own statement",
                    )
                };
            }
            let action = (k(i + 1), k(i + 2));
            match action {
                ("ADD", "COLUMN")
                    if k(i + 3) == "IF" && k(i + 4) == "NOT" && k(i + 5) == "EXISTS" =>
                {
                    Ok(StepStatement::Plain)
                }
                ("ADD", "COLUMN") => refuse(
                    "a step adds a column with ADD COLUMN IF NOT EXISTS, so a database that already has it (schema.sql declared it) takes the step too",
                ),
                ("ADD", "CONSTRAINT") => Ok(StepStatement::AddConstraint {
                    table,
                    name: at(i + 3),
                }),
                ("ADD", _) => refuse("ADD without COLUMN or CONSTRAINT is not a step"),
                ("DROP", "CONSTRAINT") if k(i + 3) == "IF" && k(i + 4) == "EXISTS" => {
                    Ok(StepStatement::Plain)
                }
                ("DROP", "CONSTRAINT") => {
                    refuse("a step drops a constraint with DROP CONSTRAINT IF EXISTS")
                }
                ("DROP", _) => refuse(
                    "a step never drops a column or a table; that is the data-safe release procedure",
                ),
                ("ALTER", "COLUMN") => match (k(i + 4), k(i + 5)) {
                    ("SET", "DEFAULT") | ("DROP", "DEFAULT") | ("DROP", "NOT") => {
                        Ok(StepStatement::Plain)
                    }
                    ("SET", "NOT") => refuse(
                        "SET NOT NULL fails on rows the step did not backfill; it needs its own reviewed release",
                    ),
                    ("TYPE", _) | ("SET", "DATA") => refuse(
                        "a step never retypes a column; that is the data-safe release procedure",
                    ),
                    _ => refuse("ALTER COLUMN form the migration does not know"),
                },
                ("RENAME", _) => refuse(
                    "a step never renames: the binary being replaced still runs against the old names",
                ),
                _ => refuse("ALTER TABLE form the migration does not know"),
            }
        }
        ("ALTER", "TYPE")
            if !k(2).is_empty()
                && k(3) == "ADD"
                && k(4) == "VALUE"
                && k(5) == "IF"
                && k(6) == "NOT"
                && k(7) == "EXISTS" =>
        {
            Ok(StepStatement::Plain)
        }
        ("ALTER", "TYPE") => refuse(
            "a step extends an enum with ALTER TYPE ... ADD VALUE IF NOT EXISTS and changes a type in no other way",
        ),
        ("CREATE", "OR") if k(2) == "REPLACE" && k(3) == "FUNCTION" => Ok(StepStatement::Plain),
        ("CREATE", "FUNCTION") => match at(2).split_once('.') {
            Some((schema, name)) if hash_named_prefix(name).is_some() => {
                Ok(StepStatement::CreateFunction {
                    schema: schema.to_string(),
                    name: name.to_string(),
                })
            }
            _ => refuse(
                "a step creates a function only under the hash of its definition (schema.name_<12 hex digits>)",
            ),
        },
        ("CREATE", _) => refuse(
            "a step does not create objects: every object of schema.sql the database lacks is created by the object pass that follows the steps",
        ),
        ("UPDATE", _) => Ok(StepStatement::Plain),
        ("COMMENT", "ON") => Ok(StepStatement::Plain),
        ("DROP", "INDEX") if k(2) == "IF" && k(3) == "EXISTS" => Ok(StepStatement::Plain),
        ("DROP", _) => refuse(
            "a step never drops a table, column, schema, type or function; that is the data-safe release procedure",
        ),
        ("DELETE", _) | ("TRUNCATE", _) => {
            refuse("a step never deletes rows; that is the data-safe release procedure")
        }
        ("DO", _) => refuse("a DO block cannot be reviewed as additive; spell the statements out"),
        _ => refuse("statement form the migration does not know"),
    }
}

/// A step's name and its statements, each with its text.
pub type LintedStep = (String, Vec<(String, StepStatement)>);

/// Every statement of every step, classified; the first refused one names
/// its step.
pub fn lint_steps(steps: &[MigrationStep]) -> Result<Vec<LintedStep>, MigrateError> {
    let mut names: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    let mut previous = "";
    for step in steps {
        if !names.insert(&step.name) || step.name.as_str() <= previous {
            return Err(MigrateError::StepRefused {
                step: step.name.clone(),
                why: "steps are named NNNN_<what>, unique and in increasing order".into(),
            });
        }
        previous = &step.name;
        let mut classified = Vec::new();
        for stmt in statements(&step.sql) {
            let kind = classify_step(&stmt).map_err(|why| MigrateError::StepRefused {
                step: step.name.clone(),
                why,
            })?;
            classified.push((stmt, kind));
        }
        if classified.is_empty() {
            return Err(MigrateError::StepRefused {
                step: step.name.clone(),
                why: "the step holds no statement".into(),
            });
        }
        out.push((step.name.clone(), classified));
    }
    Ok(out)
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("schema.sql has a statement the migration cannot classify: {0}")]
    Unclassified(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Every statement of `sql`, classified.
pub fn plan(sql: &str) -> Result<Vec<Planned>, LayoutError> {
    statements(sql)
        .into_iter()
        .map(|s| match classify(&s) {
            Some(step) => Ok(Planned { step, sql: s }),
            None => Err(LayoutError::Unclassified(s.chars().take(120).collect())),
        })
        .collect()
}

async fn exists(pool: &mut sqlx::PgConnection, object: &Object) -> sqlx::Result<bool> {
    match object {
        Object::Schema(s) => sqlx::query_scalar("SELECT to_regnamespace($1) IS NOT NULL").bind(s).fetch_one(pool).await,
        Object::Relation(r) | Object::Index(r) => sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL").bind(r).fetch_one(pool).await,
        Object::Type(t) => sqlx::query_scalar("SELECT to_regtype($1) IS NOT NULL").bind(t).fetch_one(pool).await,
        Object::Function { schema, name } => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 AND p.proname = $2)")
                .bind(schema)
                .bind(name)
                .fetch_one(pool)
                .await
        }
        Object::Trigger { name, table } => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger WHERE tgname = $1 AND tgrelid = to_regclass($2))")
                .bind(name)
                .bind(table)
                .fetch_one(pool)
                .await
        }
    }
}

async fn constraint_exists(
    conn: &mut sqlx::PgConnection,
    table: &str,
    name: &str,
) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint WHERE conname = $1 AND conrelid = to_regclass($2))")
        .bind(name)
        .bind(table)
        .fetch_one(conn)
        .await
}

async fn missing_columns(
    conn: &mut sqlx::PgConnection,
    table: &str,
    columns: &[String],
) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT c FROM unnest($2::text[]) AS c WHERE NOT EXISTS (
             SELECT 1 FROM pg_catalog.pg_attribute a
              WHERE a.attrelid = to_regclass($1) AND a.attname = c AND a.attnum > 0 AND NOT a.attisdropped)",
    )
    .bind(table)
    .bind(columns)
    .fetch_all(conn)
    .await
}

/// What a database lacks of the layout: objects absent, and columns absent
/// from tables that exist.
#[derive(Debug, Default)]
pub struct Gaps {
    pub objects: Vec<Object>,
    pub columns: Vec<String>,
    /// Exact objects retired by a reviewed schema release, still present.
    pub retired: Vec<String>,
}

impl Gaps {
    /// The gaps serve cannot run with (every missing object but an index).
    pub fn fatal(&self) -> Vec<String> {
        self.objects
            .iter()
            .filter(|o| !matches!(o, Object::Index(_)))
            .map(|o| o.to_string())
            .chain(self.columns.iter().map(|c| format!("column {c}")))
            .chain(self.retired.iter().map(|c| format!("retired {c}")))
            .collect()
    }

    pub fn indexes(&self) -> Vec<String> {
        self.objects
            .iter()
            .filter(|o| matches!(o, Object::Index(_)))
            .map(|o| o.to_string())
            .collect()
    }
}

/// The layout's gaps in the database `pool` reaches (read-only).
pub async fn gaps(pool: &PgPool, sql: &str) -> Result<Gaps, LayoutError> {
    let plan = plan(sql)?;
    let mut conn = pool.acquire().await?;
    Ok(gaps_on(&mut conn, &plan).await?)
}

async fn gaps_on(conn: &mut sqlx::PgConnection, plan: &[Planned]) -> sqlx::Result<Gaps> {
    let mut out = Gaps {
        retired: retired_voice_objects(conn).await?,
        ..Default::default()
    };
    for p in plan {
        if let Step::Create { object, columns } = &p.step {
            if !exists(&mut *conn, object).await? {
                out.objects.push(object.clone());
            } else if let Object::Relation(table) = object
                && !columns.is_empty()
            {
                for c in missing_columns(&mut *conn, table, columns).await? {
                    out.columns.push(format!("{table}.{c}"));
                }
            }
        }
    }
    Ok(out)
}

const LEDGER: &str = "meta.schema_steps";

// Release 3 retires exactly this immutable historical step. Missing receipt
// means it never ran, not permission to recreate columns the release removed.
const RETIRED_VOICE_STEP: &str = "0020_social_voice_notifications";
const RETIRED_VOICE_SHA256: &str =
    "41c5172fe07ee74acad62a9e1fa3ae74e5eaeb51d2a13262146f20034faaef17";
const VOICE_RETIRED_SINCE: i32 = 3;

fn step_retired(step: &MigrationStep, version: i32) -> Result<bool, MigrateError> {
    if step.name != RETIRED_VOICE_STEP {
        return Ok(false);
    }
    if step.sha256 != RETIRED_VOICE_SHA256 {
        return Err(MigrateError::StepDrift {
            step: step.name.clone(),
            recorded: RETIRED_VOICE_SHA256.into(),
            embedded: step.sha256.clone(),
        });
    }
    Ok(version >= VOICE_RETIRED_SINCE)
}

/// Fresh bootstrap records only steps whose effects the current schema carries.
/// Retired SQL and any genuine historical receipt remain immutable.
pub fn fresh_steps(steps: Vec<MigrationStep>) -> Result<Vec<MigrationStep>, MigrateError> {
    let mut active = Vec::new();
    for step in steps {
        if !step_retired(&step, crate::SCHEMA_VERSION)? {
            active.push(step);
        }
    }
    Ok(active)
}

async fn retired_voice_objects(conn: &mut sqlx::PgConnection) -> sqlx::Result<Vec<String>> {
    if !matches!(read_stamp(conn).await?, Some((identity, version))
        if identity == crate::SCHEMA_IDENTITY && version >= VOICE_RETIRED_SINCE)
    {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT 'column social.communities.' || attname FROM pg_attribute \
         WHERE attrelid=to_regclass('social.communities') AND NOT attisdropped \
         AND attname IN ('voice_notification_scope','voice_notification_generation','voice_notification_expires_at') \
         UNION ALL SELECT 'constraint social.communities.communities_voice_notification_scope_check' \
         WHERE EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('social.communities') \
         AND conname='communities_voice_notification_scope_check') \
         UNION ALL SELECT 'index social.communities_voice_notifications_idx' \
         WHERE to_regclass('social.communities_voice_notifications_idx') IS NOT NULL ORDER BY 1",
    ).fetch_all(conn).await
}

/// The steps `meta.schema_steps` records, by name, with the digest recorded;
/// empty when the ledger table does not exist yet (a database stamped before
/// it: every step is pending there).
async fn recorded_steps(conn: &mut sqlx::PgConnection) -> sqlx::Result<HashMap<String, String>> {
    let ledger: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(LEDGER)
        .fetch_one(&mut *conn)
        .await?;
    if !ledger {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT step, sha256 FROM meta.schema_steps")
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Records `steps` as taken: `created_fresh` for a database `schema apply`
/// just created (their effects came with schema.sql), false for a step
/// `migrate` ran.
pub async fn record_steps<'e, E>(
    executor: E,
    steps: &[MigrationStep],
    created_fresh: bool,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    let digests: Vec<&str> = steps.iter().map(|s| s.sha256.as_str()).collect();
    sqlx::query("INSERT INTO meta.schema_steps (step, sha256, created_fresh) SELECT n, d, $3 FROM unnest($1::text[], $2::text[]) AS t(n, d) ON CONFLICT (step) DO NOTHING")
        .bind(&names)
        .bind(&digests)
        .bind(created_fresh)
        .execute(executor)
        .await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error(transparent)]
    Encoding(#[from] crate::EncodingError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("meta.schema is missing: an empty database takes `catalyst schema apply`, not migrate")]
    NotStamped,
    #[error(
        "the database is stamped {identity:?} version {version}; this binary's schema is {expected_identity:?} \
         version {expected_version}. `schema migrate` only brings a database of the same identity and version up \
         to this binary's layout; a database one version behind takes `schema migrate --release` with every \
         writer stopped, and any other version change is the explicit data-safe release procedure (DATABASE.md). \
         Nothing was applied."
    )]
    Incompatible {
        identity: String,
        version: i32,
        expected_identity: &'static str,
        expected_version: i32,
    },
    #[error(
        "the release to version {version} needs every other session of this database closed (stop every \
         writer, poller and administrative path first); {count} connected, applications: {applications}. \
         Nothing was applied.",
        version = crate::SCHEMA_VERSION
    )]
    SessionsConnected { count: usize, applications: String },
    #[error(
        "the database has these tables without columns this binary's schema.sql declares, and no step of \
         schemas/migrations/ adds them: {0}. `schema migrate` only creates missing objects and applies the recorded \
         additive steps, never an ALTER a step does not spell out; add a step (ADD COLUMN IF NOT EXISTS) for a column \
         a release added, or take the data-safe release procedure for a change that is not additive. Nothing was applied."
    )]
    Drift(String),
    #[error(
        "the database retains objects retired by schema release 3: {0}. Inspect the release state; nothing was applied."
    )]
    RetiredDrift(String),
    #[error("schemas/migrations/{step}.sql is not an additive step: {why}. Nothing was applied.")]
    StepRefused { step: String, why: String },
    #[error(
        "meta.schema_steps records {step} with digest {recorded}, this binary's schemas/migrations/{step}.sql has {embedded}: \
         a step that already ran must never change (add a new one). Nothing was applied."
    )]
    StepDrift {
        step: String,
        recorded: String,
        embedded: String,
    },
    #[error("the layout has no {LEDGER} (schemas/00_content.sql) to record steps in")]
    NoLedger,
    #[error(
        "0013_island_receipt_retirement requires exactly the canonical CREATEs for social, social.island_admission_fences, social.island_cleanup_dispatches and social.island_takeover_receipts, in dependency order. Nothing was applied."
    )]
    ReceiptPrerequisites,
    #[error(
        "0016_report_upload_capabilities requires the canonical CREATE for explore.place_reports. Nothing was applied."
    )]
    ReportPrerequisite,
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// What one `migrate` run did.
#[derive(Debug, Default)]
pub struct Migrated {
    /// The steps applied, in order.
    pub steps: Vec<String>,
    /// The objects created, including any explicit step prerequisites, once each.
    pub created: Vec<Object>,
    /// The views re-created in place (`CREATE OR REPLACE VIEW`).
    pub views: Vec<Object>,
    /// The version the run released the database to, when `--release` found it one version behind.
    pub released: Option<i32>,
}

impl Migrated {
    pub fn is_empty(&self) -> bool {
        self.released.is_none() && self.steps.is_empty() && self.created.is_empty()
    }
}

/// A stamp one version behind this binary's: what `RELEASE_SQL` accepts.
fn release_pending(stamp: &Option<(String, i32)>) -> bool {
    matches!(stamp, Some((identity, version))
        if identity == crate::SCHEMA_IDENTITY && *version == crate::SCHEMA_VERSION - 1)
}

/// The `pg_stat_activity` predicate for a client session, the sessions that
/// block the release. PostgreSQL hides `backend_type` (NULL) for a session of
/// a role the caller is not a member of, unless the caller has
/// `pg_read_all_stats`; such a row counts as a client session. A hidden row
/// without a role (`usesysid` NULL) is a server process, such as an
/// autovacuum worker, never a client session.
pub const CLIENT_SESSION: &str = "(backend_type = 'client backend' \
     OR (backend_type IS NULL AND usesysid IS NOT NULL))";

/// The application name of every other client session of this database
/// (`-` when unset or hidden).
async fn other_sessions(conn: &mut sqlx::PgConnection) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT coalesce(nullif(application_name, ''), '-') \
         FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid() \
           AND {CLIENT_SESSION} \
         ORDER BY 1"
    )))
    .fetch_all(conn)
    .await
}

async fn read_stamp(conn: &mut sqlx::PgConnection) -> sqlx::Result<Option<(String, i32)>> {
    let stamped: bool = sqlx::query_scalar("SELECT to_regclass('meta.schema') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !stamped {
        return Ok(None);
    }
    sqlx::query_as("SELECT identity, version FROM meta.schema WHERE id = 1")
        .fetch_optional(conn)
        .await
}

fn check_stamp(stamp: Option<(String, i32)>) -> Result<(), MigrateError> {
    match stamp {
        None => Err(MigrateError::NotStamped),
        Some((identity, version))
            if identity == crate::SCHEMA_IDENTITY && version == crate::SCHEMA_VERSION =>
        {
            Ok(())
        }
        Some((identity, version)) => Err(MigrateError::Incompatible {
            identity,
            version,
            expected_identity: crate::SCHEMA_IDENTITY,
            expected_version: crate::SCHEMA_VERSION,
        }),
    }
}

/// Shared by the planner regression and executor: recorded identical steps never replay.
fn step_needs_apply(step: &MigrationStep, recorded: Option<&str>) -> Result<bool, MigrateError> {
    match recorded {
        Some(digest) if digest == step.sha256 => Ok(false),
        Some(digest) => Err(MigrateError::StepDrift {
            step: step.name.clone(),
            recorded: digest.into(),
            embedded: step.sha256.clone(),
        }),
        None => Ok(true),
    }
}

// This one step can meet a database older than the receipt tables themselves.
// Keep its bounded dependency chain in the canonical plan, never alternate DDL.
const RECEIPT_RETIREMENT: &str = "0013_island_receipt_retirement";
const RECEIPTS: &str = "social.island_takeover_receipts";

fn receipt_prerequisites(plan: &[Planned]) -> Result<Vec<&Planned>, MigrateError> {
    let expected = [
        Object::Schema("social".into()),
        Object::Relation("social.island_admission_fences".into()),
        Object::Relation("social.island_cleanup_dispatches".into()),
        Object::Relation(RECEIPTS.into()),
    ];
    let selected: Vec<_> = plan
        .iter()
        .filter(|p| matches!(&p.step, Step::Create { object, .. } if expected.contains(object)))
        .collect();
    let actual: Vec<_> = selected
        .iter()
        .filter_map(|p| {
            if let Step::Create { object, .. } = &p.step {
                Some(object)
            } else {
                None
            }
        })
        .collect();
    if actual != expected.iter().collect::<Vec<_>>() {
        return Err(MigrateError::ReceiptPrerequisites);
    }
    Ok(selected)
}

const REPORT_CAPABILITIES: &str = "0016_report_upload_capabilities";
const REPORTS: &str = "explore.place_reports";

fn report_prerequisite(plan: &[Planned]) -> Result<&Planned, MigrateError> {
    let mut selected = plan.iter().filter(|p| matches!(&p.step, Step::Create { object: Object::Relation(name), .. } if name == REPORTS));
    let report = selected.next().ok_or(MigrateError::ReportPrerequisite)?;
    if selected.next().is_some() {
        return Err(MigrateError::ReportPrerequisite);
    }
    Ok(report)
}

async fn create_missing(
    conn: &mut sqlx::PgConnection,
    object: &Object,
    sql: &str,
    made: &mut HashSet<Object>,
    done: &mut Migrated,
) -> Result<(), MigrateError> {
    if !exists(conn, object).await? {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(conn)
            .await?;
        made.insert(object.clone());
        done.created.push(object.clone());
    } else if let Some(replace) = view_replacement(sql) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(replace))
            .execute(conn)
            .await?;
        done.views.push(object.clone());
    }
    Ok(())
}

/// A `CREATE VIEW` as the `CREATE OR REPLACE VIEW` that brings an existing
/// view to the layout. A view holds no rows, and PostgreSQL refuses a
/// replacement that drops, renames or retypes a column, so the run is
/// additive by construction: a view that needs one of those is a reviewed
/// release.
fn view_replacement(sql: &str) -> Option<String> {
    let w = words(sql);
    (w.len() > 2 && upper(w[0]) == "CREATE" && upper(w[1]) == "VIEW")
        .then(|| format!("CREATE OR REPLACE{}", sql.trim_start()[6..].to_owned()))
}

/// Brings a stamped database of this binary's identity and version up to its
/// layout, in one transaction: the steps of `migrations` it has not recorded
/// (in order, each recorded as it goes), the layout check that rolls
/// everything back when a column is still missing (including any explicit
/// canonical receipt prerequisites created for pending 0013), then every object of
/// `sql` it lacks (and the follow-ups of the ones it creates); a view it has
/// is re-created in place, so a column a step added reaches the view. Returns
/// what it did. `sql` and `migrations` are this binary's own texts (`db::SCHEMA_SQL`,
/// `db::MIGRATIONS_SQL`), never input.
pub async fn migrate(pool: &PgPool, sql: &str, migrations: &str) -> Result<Migrated, MigrateError> {
    migrate_in_one(pool, sql, migrations, None).await
}

/// `migrate`, preceded by `release` (this binary's `db::RELEASE_SQL`) when the
/// database is stamped one version behind, all in the same transaction: a
/// refusal or failure anywhere leaves the database as it was. The release
/// runs only when no other client session is connected to the database, so
/// every writer, poller and administrative path is stopped first. A database
/// already at this binary's version takes the plain `migrate`.
pub async fn migrate_with_release(
    pool: &PgPool,
    sql: &str,
    migrations: &str,
    release: &str,
) -> Result<Migrated, MigrateError> {
    migrate_in_one(pool, sql, migrations, Some(release)).await
}

async fn migrate_in_one(
    pool: &PgPool,
    sql: &str,
    migrations: &str,
    release: Option<&str>,
) -> Result<Migrated, MigrateError> {
    crate::require_utf8(pool).await?;
    let steps = parse_steps(migrations);
    let linted = lint_steps(&steps)?;
    let plan = plan(sql)?;
    let mut tx = pool.begin().await?;
    let mut stamp = read_stamp(&mut tx).await?;
    let mut done = Migrated::default();
    if let Some(release) = release
        && release_pending(&stamp)
    {
        let mut sessions = other_sessions(&mut tx).await?;
        if !sessions.is_empty() {
            let count = sessions.len();
            sessions.dedup();
            return Err(MigrateError::SessionsConnected {
                count,
                applications: sessions.join(", "),
            });
        }
        // The release backfills and builds indexes; the pool's per-statement
        // limit is for serving, not for this.
        sqlx::query("SET LOCAL statement_timeout = 0")
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(release.to_owned()))
            .execute(&mut *tx)
            .await?;
        stamp = read_stamp(&mut tx).await?;
        done.released = Some(crate::SCHEMA_VERSION);
    }
    check_stamp(stamp.clone())?;
    let version = stamp.expect("checked stamp").1;
    let retired = retired_voice_objects(&mut tx).await?;
    if !retired.is_empty() {
        return Err(MigrateError::RetiredDrift(retired.join(", ")));
    }
    let mut made: HashSet<Object> = HashSet::new();

    // 1. the steps, recorded in the ledger (created first on a database
    //    stamped before it, from the layout's own CREATE)
    if !exists(&mut tx, &Object::Relation(LEDGER.into())).await? {
        let create = plan
            .iter()
            .find(|p| matches!(&p.step, Step::Create { object: Object::Relation(r), .. } if r == LEDGER))
            .ok_or(MigrateError::NoLedger)?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(create.sql.clone()))
            .execute(&mut *tx)
            .await?;
    }
    let recorded = recorded_steps(&mut tx).await?;
    for (step, statements) in steps.iter().zip(linted.iter().map(|(_, s)| s)) {
        // Verify both the embedded immutable bytes and any genuine receipt,
        // even when this version deliberately carries no effect of the step.
        let retired = step_retired(step, version)?;
        let pending = step_needs_apply(step, recorded.get(&step.name).map(String::as_str))?;
        if retired || !pending {
            continue;
        }
        if step.name == RECEIPT_RETIREMENT
            && !exists(&mut tx, &Object::Relation(RECEIPTS.into())).await?
        {
            for p in receipt_prerequisites(&plan)? {
                if let Step::Create { object, .. } = &p.step {
                    create_missing(&mut tx, object, &p.sql, &mut made, &mut done).await?;
                }
            }
        }
        // Closure-base databases predate this table. Create its canonical empty
        // shape before 0016's scrub; existing rows still take the normal migration.
        if step.name == REPORT_CAPABILITIES
            && !exists(&mut tx, &Object::Relation(REPORTS.into())).await?
        {
            let p = report_prerequisite(&plan)?;
            if let Step::Create { object, .. } = &p.step {
                create_missing(&mut tx, object, &p.sql, &mut made, &mut done).await?;
            }
        }
        for (stmt, kind) in statements {
            if let StepStatement::AddConstraint { table, name } = kind
                && constraint_exists(&mut tx, table, name).await?
            {
                continue;
            }
            if let StepStatement::CreateFunction { schema, name } = kind
                && exists(
                    &mut tx,
                    &Object::Function {
                        schema: schema.clone(),
                        name: name.clone(),
                    },
                )
                .await?
            {
                continue;
            }
            sqlx::raw_sql(sqlx::AssertSqlSafe(stmt.clone()))
                .execute(&mut *tx)
                .await?;
        }
        record_steps(&mut *tx, std::slice::from_ref(step), false).await?;
        done.steps.push(step.name.clone());
    }

    // 2. the layout, after the steps and before any object: a column an
    //    existing table still lacks is drift no step covers, and the
    //    transaction is dropped unapplied (an index over such a column would
    //    otherwise fail the run with a bare SQL error).
    let left = gaps_on(&mut tx, &plan).await?;
    if !left.columns.is_empty() {
        return Err(MigrateError::Drift(left.columns.join(", ")));
    }

    // 3. the remaining objects, sharing accounting with any prerequisites.
    for p in &plan {
        match &p.step {
            Step::Create { object, .. } => {
                create_missing(&mut tx, object, &p.sql, &mut made, &mut done).await?;
            }
            Step::FollowUp(object) => {
                if made.contains(object) {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(p.sql.clone()))
                        .execute(&mut *tx)
                        .await?;
                }
            }
            Step::Always => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(p.sql.clone()))
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }

    tx.commit().await?;
    Ok(done)
}

/// The comparison `catalyst schema status` prints: the stamp against this
/// binary's, the steps pending or drifted, the layout's gaps.
#[derive(Debug, Default)]
pub struct SchemaStatus {
    pub stamp: Option<(String, i32)>,
    pub pending_steps: Vec<String>,
    pub drifted_steps: Vec<String>,
    pub refused_step: Option<String>,
    pub gaps: Gaps,
    /// Hash-named functions an earlier layout created and this one no longer
    /// declares (`schema.name(args)`); informational, the exit code ignores them.
    pub superseded_functions: Vec<String>,
}

impl SchemaStatus {
    pub fn compatible(&self) -> bool {
        check_stamp(self.stamp.clone()).is_ok()
    }

    /// 0: this binary can serve the database as it is. 3: `schema migrate`
    /// has work (steps to apply, objects to create) and nothing stands in
    /// its way. 4: no stamp, another identity or version, a drifted or
    /// refused step, or a missing column no pending step adds. 5: the
    /// database is one version behind and `schema migrate --release` takes
    /// it to this binary's version once every writer is stopped.
    pub fn exit_code(&self) -> i32 {
        if release_pending(&self.stamp)
            && self.drifted_steps.is_empty()
            && self.refused_step.is_none()
        {
            return 5;
        }
        if !self.compatible()
            || !self.drifted_steps.is_empty()
            || self.refused_step.is_some()
            || !self.gaps.retired.is_empty()
        {
            return 4;
        }
        if !self.gaps.columns.is_empty() && self.pending_steps.is_empty() {
            return 4;
        }
        if !self.pending_steps.is_empty()
            || !self.gaps.objects.is_empty()
            || !self.gaps.columns.is_empty()
        {
            return 3;
        }
        0
    }

    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        match &self.stamp {
            Some((identity, version)) => out.push(format!(
                "stamp: database {identity} version {version}, binary {} version {}: {}",
                crate::SCHEMA_IDENTITY,
                crate::SCHEMA_VERSION,
                if self.compatible() {
                    "compatible"
                } else if release_pending(&self.stamp) {
                    "one version behind (`schema migrate --release` with every writer stopped)"
                } else {
                    "INCOMPATIBLE (the data-safe release procedure, not migrate)"
                }
            )),
            None => {
                out.push("stamp: none (an empty database takes `catalyst schema apply`)".into())
            }
        }
        out.push(format!(
            "steps pending: {}",
            if self.pending_steps.is_empty() {
                "none".to_string()
            } else {
                self.pending_steps.join(", ")
            }
        ));
        if !self.drifted_steps.is_empty() {
            out.push(format!(
                "steps whose recorded text differs from this binary's: {}",
                self.drifted_steps.join(", ")
            ));
        }
        if let Some(why) = &self.refused_step {
            out.push(format!("step refused: {why}"));
        }
        if !self.gaps.retired.is_empty() {
            out.push(format!(
                "retired objects present: {}",
                self.gaps.retired.join(", ")
            ));
        }
        out.push(format!("objects missing: {}", self.gaps.objects.len()));
        out.push(format!(
            "columns missing: {}",
            if self.gaps.columns.is_empty() {
                "none".to_string()
            } else {
                self.gaps.columns.join(", ")
            }
        ));
        if !self.superseded_functions.is_empty() {
            out.push(format!(
                "functions superseded after migrate (drop once no process runs an older binary): {}",
                self.superseded_functions.join(", ")
            ));
        }
        out.push(format!(
            "verdict: {}",
            match self.exit_code() {
                0 => "up to date",
                3 => "catalyst schema migrate brings it up to date",
                5 => "catalyst schema migrate --release brings it up to date once every writer is stopped",
                _ => "not this binary's layout and migrate cannot make it so",
            }
        ));
        out
    }
}

/// Read-only: what `migrate` would find.
pub async fn status(
    pool: &PgPool,
    sql: &str,
    migrations: &str,
) -> Result<SchemaStatus, MigrateError> {
    let mut conn = pool.acquire().await?;
    let mut out = SchemaStatus {
        stamp: read_stamp(&mut conn).await?,
        ..Default::default()
    };
    let steps = parse_steps(migrations);
    if let Err(e) = lint_steps(&steps) {
        out.refused_step = Some(e.to_string());
    }
    if let Some(stamp) = &out.stamp {
        let recorded = recorded_steps(&mut conn).await?;
        for step in &steps {
            let version = stamp.1;
            let retired = match step_retired(step, version) {
                Ok(retired) => retired,
                Err(error) => {
                    out.refused_step = Some(error.to_string());
                    continue;
                }
            };
            match step_needs_apply(step, recorded.get(&step.name).map(String::as_str)) {
                Ok(true) if !retired => out.pending_steps.push(step.name.clone()),
                Ok(_) => {}
                Err(_) => out.drifted_steps.push(step.name.clone()),
            }
        }
        let plan = plan(sql)?;
        out.gaps = gaps_on(&mut conn, &plan).await?;
        out.superseded_functions = superseded_functions(&mut conn, &plan).await?;
    }
    Ok(out)
}

/// `name` split into its prefix when it ends in `_` and 12 hex digits, the
/// body-hash suffix of the write path's functions.
fn hash_named_prefix(name: &str) -> Option<&str> {
    let (prefix, hash) = name.rsplit_once('_')?;
    (!prefix.is_empty()
        && hash.len() == 12
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(prefix)
}

/// Functions in the database named like a hash-named function of the layout
/// (same schema and prefix, another hash) that the layout does not declare
/// and no column default, generated column or index still calls
/// (`pg_depend`: a step replaced such a function's body in place, and
/// dropping it would take the column or the index with it).
async fn superseded_functions(
    conn: &mut sqlx::PgConnection,
    plan: &[Planned],
) -> sqlx::Result<Vec<String>> {
    let declared: Vec<(String, String)> = plan
        .iter()
        .filter_map(|p| match &p.step {
            Step::Create {
                object: Object::Function { schema, name },
                ..
            } => Some((schema.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let mut families: Vec<(String, String)> = declared
        .iter()
        .filter_map(|(schema, name)| {
            hash_named_prefix(name).map(|prefix| (schema.clone(), prefix.to_string()))
        })
        .collect();
    families.sort();
    families.dedup();
    let mut out = Vec::new();
    for (schema, prefix) in families {
        let found: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.proname::text, format('%I.%I(%s)', n.nspname, p.proname, pg_get_function_identity_arguments(p.oid))
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = $1 AND starts_with(p.proname, $2 || '_')
               AND NOT EXISTS (
                 SELECT 1 FROM pg_catalog.pg_depend d
                 WHERE d.refclassid = 'pg_catalog.pg_proc'::regclass AND d.refobjid = p.oid
                   AND d.classid IN ('pg_catalog.pg_attrdef'::regclass, 'pg_catalog.pg_class'::regclass))
             ORDER BY 2",
        )
        .bind(&schema)
        .bind(&prefix)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(found.into_iter().filter_map(|(name, signature)| {
            (hash_named_prefix(&name) == Some(prefix.as_str())
                && !declared.iter().any(|(s, n)| *s == schema && *n == name))
            .then_some(signature)
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_prerequisites_are_exact_canonical_creates_with_bounded_dependencies() {
        let plan = plan(crate::SCHEMA_SQL).unwrap();
        let selected = receipt_prerequisites(&plan).unwrap();
        assert_eq!(selected.len(), 4);
        let expected_refs: [&[&str]; 4] = [
            &[],
            &[],
            &["social.island_admission_fences"],
            &[
                "social.island_admission_fences",
                "social.island_cleanup_dispatches",
            ],
        ];
        for (p, expected) in selected.iter().zip(expected_refs) {
            let Step::Create { object, .. } = &p.step else {
                panic!("not CREATE")
            };
            let tokens = words(&p.sql);
            let references: Vec<_> = tokens
                .windows(2)
                .filter(|w| upper(w[0]) == "REFERENCES")
                .map(|w| w[1])
                .collect();
            assert_eq!(
                references, expected,
                "review any new prerequisite dependency: {object}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|p| matches!(&p.step, Step::FollowUp(o) if o == object)),
                "review any new prerequisite follow-up: {object}"
            );
        }
        for missing in selected {
            let incomplete: Vec<_> = plan
                .iter()
                .filter(|p| !std::ptr::eq(*p, missing))
                .cloned()
                .collect();
            assert!(matches!(
                receipt_prerequisites(&incomplete),
                Err(MigrateError::ReceiptPrerequisites)
            ));
        }
        let mut duplicated = plan.clone();
        duplicated.push(receipt_prerequisites(&plan).unwrap()[0].clone());
        assert!(matches!(
            receipt_prerequisites(&duplicated),
            Err(MigrateError::ReceiptPrerequisites)
        ));
    }

    #[test]
    fn report_prerequisite_has_no_external_dependencies_or_followups() {
        let plan = plan(crate::SCHEMA_SQL).unwrap();
        let report = report_prerequisite(&plan).unwrap();
        assert!(
            !words(&report.sql)
                .iter()
                .any(|word| upper(word) == "REFERENCES")
        );
        assert!(
            !plan.iter().any(
                |p| matches!(&p.step, Step::FollowUp(Object::Relation(name)) if name == REPORTS)
            )
        );
        let missing: Vec<_> = plan
            .iter()
            .filter(|p| !std::ptr::eq(*p, report))
            .cloned()
            .collect();
        assert!(matches!(
            report_prerequisite(&missing),
            Err(MigrateError::ReportPrerequisite)
        ));
        let mut duplicate = plan.clone();
        duplicate.push(report.clone());
        assert!(matches!(
            report_prerequisite(&duplicate),
            Err(MigrateError::ReportPrerequisite)
        ));
    }

    #[test]
    fn statements_split_on_top_level_semicolons_only() {
        let sql = "-- a; comment\nCREATE TABLE a.b (x text DEFAULT 'a;b', \"y;\" int);\nCREATE FUNCTION a.f() RETURNS int AS $body$ SELECT 1; SELECT 2; $body$ LANGUAGE sql;\n/* c; */ DO $$ BEGIN PERFORM 1; END $$;\n";
        let s = statements(sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[0].starts_with("CREATE TABLE a.b"));
        assert!(s[1].contains("SELECT 2;"));
        assert!(s[2].starts_with("DO $$"));
    }

    #[test]
    fn a_table_declares_its_columns_not_its_constraints() {
        let cols = table_columns(
            "CREATE TABLE a.t (\n  id integer NOT NULL DEFAULT nextval('a.s'::regclass), -- the id (a, b\n  -- note, with (parens\n  name text CHECK (name <> ''),\n  n numeric(78, 0),\n  CONSTRAINT t_pkey PRIMARY KEY (id),\n  UNIQUE (name)\n)",
        );
        assert_eq!(cols, vec!["id", "name", "n"]);
    }

    #[test]
    fn every_statement_of_this_binarys_schema_is_classified() {
        let plan = plan(crate::SCHEMA_SQL).expect("schema.sql fully classified");
        let creates = plan
            .iter()
            .filter(|p| matches!(p.step, Step::Create { .. }))
            .count();
        assert!(creates > 300, "{creates}");
        let tables: Vec<&Planned> = plan.iter().filter(|p| matches!(&p.step, Step::Create { object: Object::Relation(r), .. } if r == "content.deployments")).collect();
        assert_eq!(tables.len(), 1);
        let Step::Create { columns, .. } = &tables[0].step else {
            unreachable!()
        };
        assert!(
            columns.contains(&"deleter_deployment".to_string())
                && columns.contains(&"auth_chain".to_string()),
            "{columns:?}"
        );
        assert!(plan.iter().any(|p| matches!(&p.step, Step::Create { object: Object::Function { name, .. }, .. } if name.starts_with("deploy_entity_"))));
        assert!(plan.iter().any(|p| matches!(&p.step, Step::Create { object: Object::Index(i), .. } if i.starts_with("content."))));
        assert!(plan.iter().any(|p| matches!(&p.step, Step::Create { object: Object::Relation(r), .. } if r == LEDGER)), "the steps ledger is part of the layout");
    }

    #[test]
    fn removal_feed_objects_and_seeds_are_in_the_existing_database_plan() {
        let planned = plan(crate::SCHEMA_SQL).unwrap();
        for name in [
            "explore.place_removal_pending",
            "explore.place_removal_state",
        ] {
            assert!(planned.iter().any(
                |p| matches!(&p.step, Step::Create { object: Object::Relation(n), .. } if n == name)
            ));
            assert!(
                planned
                    .iter()
                    .any(|p| matches!(&p.step, Step::FollowUp(Object::Relation(n)) if n == name))
            );
        }
        assert!(planned.iter().any(|p| matches!(&p.step, Step::Create { object: Object::Function {schema,name}, .. } if schema == "explore" && name == "capture_place_removal")));
        for (name, table) in [
            ("place_removal_capture", "explore.place"),
            ("local_world_removal_capture", "explore.place_world_local"),
        ] {
            assert!(planned.iter().any(|p| matches!(&p.step, Step::Create { object: Object::Trigger {name:n,table:t}, .. } if n == name && t == table)));
        }
    }

    #[tokio::test]
    async fn existing_catalogue_migration_installs_removal_triggers_and_seeds_once() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("removal_mig_{}", uuid::Uuid::new_v4().simple());
        let columns = "id text PRIMARY KEY, creator_address text, world boolean NOT NULL DEFAULT false, world_name text, base_position text NOT NULL DEFAULT '0,0', raw jsonb NOT NULL DEFAULT '{}', disabled boolean NOT NULL DEFAULT false";
        let older = format!(
            "CREATE SCHEMA {schema}; CREATE TABLE {schema}.place ({columns}); CREATE TABLE {schema}.place_world_local ({columns}); CREATE TABLE {schema}.place_moderation (place_id text PRIMARY KEY, disabled boolean, reason text); CREATE VIEW {schema}.place_indexed AS SELECT * FROM {schema}.place UNION ALL SELECT * FROM {schema}.place_world_local; INSERT INTO {schema}.place (id, disabled) VALUES ('existing-disabled', true), ('existing-active', false);"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(older.clone()))
            .execute(&pool)
            .await
            .unwrap();
        let additions = crate::SCHEMA_SQL
            .split("-- A sparse removal outbox.")
            .nth(1)
            .unwrap()
            .split("-- ════ schemas/")
            .next()
            .unwrap();
        let newer = format!("{older}\n-- A sparse removal outbox.{additions}")
            .replace("explore.", &format!("{schema}."));
        let done = migrate(&pool, &newer, "").await.unwrap();
        assert!(
            done.created.iter().any(
                |o| matches!(o, Object::Trigger { name, .. } if name == "place_removal_capture")
            )
        );
        let seeded: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.place_removal_pending"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(seeded, 1);
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {schema}.place WHERE id = 'existing-active'"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let captured: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.place_removal_pending"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            captured, 2,
            "trigger captures removals from preexisting tables"
        );
        assert!(migrate(&pool, &newer, "").await.unwrap().is_empty());
        let after: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.place_removal_pending"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, 2, "migration does not replay the initial seed");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn every_step_of_this_binary_is_additive_and_named_in_order() {
        let steps = parse_steps(crate::MIGRATIONS_SQL);
        assert!(
            !steps.is_empty(),
            "schemas/migrations/ holds the closure base's upgrade at least"
        );
        assert_eq!(steps[0].name, "0001_credits_purchases_quantity_source");
        let linted = lint_steps(&steps).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(linted[0].1.len(), 4, "{:?}", linted[0].1);
        assert!(
            matches!(&linted[0].1[2].1, StepStatement::AddConstraint { table, name } if table == "credits.purchases" && name == "purchases_quantity_check")
        );
        assert!(steps.iter().all(|s| s.sha256.len() == 64));
    }

    #[test]
    fn retirement_is_bound_to_the_exact_immutable_step_and_schema_version() {
        let steps = parse_steps(crate::MIGRATIONS_SQL);
        let step = steps.iter().find(|s| s.name == RETIRED_VOICE_STEP).unwrap();
        assert!(!step_retired(step, 2).unwrap());
        assert!(step_retired(step, 3).unwrap());
        assert!(step_retired(step, 4).unwrap());
        assert!(
            !fresh_steps(steps.clone())
                .unwrap()
                .iter()
                .any(|s| s.name == RETIRED_VOICE_STEP)
        );
        let mut changed = step.clone();
        changed.sql.push_str(" -- changed");
        changed.sha256 = hex::encode(Sha256::digest(changed.sql.as_bytes()));
        assert!(matches!(
            step_retired(&changed, 3),
            Err(MigrateError::StepDrift { .. })
        ));
        changed.name.push_str("_another");
        assert!(!step_retired(&changed, 3).unwrap());
    }

    #[test]
    fn streaming_upgrade_uses_actual_additive_step_and_bootstrap_objects() {
        let steps = parse_steps(crate::MIGRATIONS_SQL);
        let step = steps
            .iter()
            .find(|s| s.name == "0008_comms_stream_cleanup")
            .expect("actual streaming migration is embedded");
        let linted = lint_steps(std::slice::from_ref(step)).unwrap();
        assert_eq!(linted[0].1.len(), 3);
        assert!(
            linted[0]
                .1
                .iter()
                .all(|(_, kind)| matches!(kind, StepStatement::Plain))
        );
        // Production classification accepts replay against already-present columns
        // only with PostgreSQL's IF NOT EXISTS no-op semantics.
        for (statement, _) in &linted[0].1 {
            assert_eq!(classify_step(statement).unwrap(), StepStatement::Plain);
            assert!(classify_step(&statement.replace(" IF NOT EXISTS", "")).is_err());
        }
        assert!(step_needs_apply(step, None).unwrap());
        assert!(!step_needs_apply(step, Some(&step.sha256)).unwrap());
        assert!(matches!(
            step_needs_apply(step, Some("different")),
            Err(MigrateError::StepDrift { .. })
        ));
        let objects = plan(crate::SCHEMA_SQL).unwrap();
        let access=objects.iter().find(|p|matches!(&p.step,Step::Create{object:Object::Relation(r),..}if r=="social.scene_stream_access")).unwrap();
        let Step::Create { columns, .. } = &access.step else {
            unreachable!()
        };
        for column in [
            "streaming_state_version",
            "streaming_checked_at",
            "cleanup_retry_at",
        ] {
            assert!(columns.iter().any(|c| c == column));
        }
        assert!(objects.iter().any(|p|matches!(&p.step,Step::Create{object:Object::Relation(r),..}if r=="social.scene_stream_cleanup")));
        assert!(objects.iter().any(|p|matches!(&p.step,Step::Create{object:Object::Index(i),..}if i=="social.scene_stream_access_reconcile_idx")));
    }

    #[test]
    fn a_view_is_replaced_in_place_and_nothing_else_is() {
        assert_eq!(
            view_replacement("CREATE VIEW a.v AS\n    SELECT 1;").as_deref(),
            Some("CREATE OR REPLACE VIEW a.v AS\n    SELECT 1;")
        );
        assert!(view_replacement("CREATE TABLE a.t (c int)").is_none());
        assert!(view_replacement("CREATE MATERIALIZED VIEW a.v AS SELECT 1").is_none());
    }

    #[test]
    fn a_step_lint_takes_the_additive_forms_and_refuses_the_rest() {
        for ok in [
            "ALTER TABLE a.t ADD COLUMN IF NOT EXISTS c integer NOT NULL DEFAULT 0",
            "ALTER TABLE IF EXISTS a.t ADD COLUMN IF NOT EXISTS c text",
            "ALTER TABLE IF EXISTS a.t ADD COLUMN IF NOT EXISTS c integer GENERATED ALWAYS AS (CASE WHEN d ~ '^[0-9]{1,3}$' THEN (d)::int END) STORED, ADD COLUMN IF NOT EXISTS e text",
            "ALTER TABLE a.t DROP CONSTRAINT IF EXISTS t_check",
            "ALTER TABLE a.t ALTER COLUMN c SET DEFAULT 1",
            "ALTER TABLE a.t ALTER COLUMN c DROP NOT NULL",
            "ALTER TYPE a.kind ADD VALUE IF NOT EXISTS 'x'",
            "CREATE OR REPLACE FUNCTION a.f() RETURNS int LANGUAGE sql AS $f$ SELECT 1 $f$",
            "CREATE FUNCTION a.f_47d21316abf2(text) RETURNS int LANGUAGE sql RETURN 1",
            "UPDATE a.t SET c = 1 WHERE c IS NULL",
            "COMMENT ON COLUMN a.t.c IS 'x'",
            "DROP INDEX IF EXISTS a.t_idx",
        ] {
            assert!(classify_step(ok).is_ok(), "{ok}: {:?}", classify_step(ok));
        }
        assert_eq!(
            classify_step("ALTER TABLE a.t ADD CONSTRAINT t_c_check CHECK (c >= 0)"),
            Ok(StepStatement::AddConstraint {
                table: "a.t".into(),
                name: "t_c_check".into()
            })
        );
        assert_eq!(
            classify_step(
                "CREATE FUNCTION a.f_47d21316abf2(text) RETURNS int LANGUAGE sql RETURN 1"
            ),
            Ok(StepStatement::CreateFunction {
                schema: "a".into(),
                name: "f_47d21316abf2".into()
            })
        );
        for (bad, why) in [
            ("ALTER TABLE a.t ADD COLUMN c integer", "IF NOT EXISTS"),
            (
                "ALTER TABLE IF EXISTS a.t ADD COLUMN IF NOT EXISTS c integer GENERATED ALWAYS AS (CASE WHEN d ~ '^[0-9]{1,3}$' THEN (d)::int END) STORED, ADD COLUMN e text",
                "several actions",
            ),
            (
                "ALTER TABLE a.t ADD COLUMN IF NOT EXISTS c integer, ADD CONSTRAINT t_c_check CHECK (c >= 0)",
                "several actions",
            ),
            ("ALTER TABLE a.t DROP COLUMN c", "never drops a column"),
            ("ALTER TABLE a.t DROP CONSTRAINT t_check", "IF EXISTS"),
            (
                "ALTER TABLE a.t ALTER COLUMN c TYPE bigint",
                "never retypes",
            ),
            (
                "ALTER TABLE a.t ALTER COLUMN c SET NOT NULL",
                "SET NOT NULL",
            ),
            ("ALTER TABLE a.t RENAME COLUMN c TO d", "never renames"),
            ("ALTER TABLE a.t RENAME TO u", "never renames"),
            ("DROP TABLE a.t", "never drops"),
            ("DROP SCHEMA a CASCADE", "never drops"),
            ("DELETE FROM a.t WHERE c = 1", "never deletes"),
            ("TRUNCATE a.t", "never deletes"),
            ("CREATE TABLE a.u (id int)", "does not create"),
            (
                "CREATE FUNCTION a.f(text) RETURNS int LANGUAGE sql RETURN 1",
                "hash of its definition",
            ),
            (
                "CREATE FUNCTION f_47d21316abf2(text) RETURNS int LANGUAGE sql RETURN 1",
                "hash of its definition",
            ),
            ("CREATE INDEX IF NOT EXISTS i ON a.t (c)", "does not create"),
            ("DO $$ BEGIN PERFORM 1; END $$", "DO block"),
            (
                "ALTER TYPE a.kind RENAME VALUE 'x' TO 'y'",
                "ADD VALUE IF NOT EXISTS",
            ),
            ("SELECT 1", "does not know"),
        ] {
            let err = classify_step(bad).expect_err(bad);
            assert!(err.contains(why), "{bad}: {err}");
        }
        let unordered = vec![
            MigrationStep {
                name: "0002_b".into(),
                sql: "UPDATE a.t SET c = 1;\n".into(),
                sha256: String::new(),
            },
            MigrationStep {
                name: "0001_a".into(),
                sql: "UPDATE a.t SET c = 1;\n".into(),
                sha256: String::new(),
            },
        ];
        assert!(
            matches!(lint_steps(&unordered), Err(MigrateError::StepRefused { step, .. }) if step == "0001_a")
        );
    }

    #[test]
    fn steps_are_parsed_from_the_rendering_by_marker_and_digested() {
        let rendered = "-- header\n-- more\n\n-- ════ step 0001_a ════\n\n-- why\nUPDATE a.t SET c = 1;\n\n-- ════ step 0002_b ════\n\nALTER TABLE a.t ADD COLUMN IF NOT EXISTS d text;\n";
        let steps = parse_steps(rendered);
        assert_eq!(
            steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["0001_a", "0002_b"]
        );
        assert_eq!(steps[0].sql, "-- why\nUPDATE a.t SET c = 1;\n");
        assert_eq!(
            steps[1].sql,
            "ALTER TABLE a.t ADD COLUMN IF NOT EXISTS d text;\n"
        );
        assert_eq!(
            steps[1].sha256,
            hex::encode(Sha256::digest(steps[1].sql.as_bytes()))
        );
        assert!(parse_steps("-- nothing\n").is_empty());
    }

    #[test]
    fn the_layout_names_the_stamp_the_schema_digest_and_every_step() {
        let rendered = "-- ════ step 0001_a ════\nUPDATE a.t SET c = 1;\n";
        let base = layout("catalyst", 2, "CREATE TABLE a.t ();\n", rendered);
        let lines: Vec<&str> = base.lines().collect();
        assert_eq!(lines[0], "identity catalyst");
        assert_eq!(lines[1], "version 2");
        assert_eq!(
            lines[2],
            format!(
                "schema {}",
                hex::encode(Sha256::digest(b"CREATE TABLE a.t ();\n"))
            )
        );
        assert_eq!(
            lines[3],
            format!("step 0001_a {}", parse_steps(rendered)[0].sha256)
        );
        assert_eq!(lines.len(), 4);
        // a new step, a new table or a new version is another layout
        let ahead = format!("{rendered}-- ════ step 0002_b ════\nUPDATE a.t SET c = 2;\n");
        assert_ne!(
            layout("catalyst", 2, "CREATE TABLE a.t ();\n", &ahead),
            base
        );
        assert_ne!(
            layout(
                "catalyst",
                2,
                "CREATE TABLE a.t (); CREATE TABLE a.u ();\n",
                rendered
            ),
            base
        );
        assert_ne!(
            layout("catalyst", 3, "CREATE TABLE a.t ();\n", rendered),
            base
        );
        assert_eq!(
            layout("catalyst", 2, "CREATE TABLE a.t ();\n", rendered),
            base
        );
    }

    #[test]
    fn hash_named_prefixes_need_twelve_lowercase_hex_digits() {
        assert_eq!(
            hash_named_prefix("unpublish_scene_47d21316abf2"),
            Some("unpublish_scene")
        );
        assert_eq!(hash_named_prefix("decode_coord"), None);
        assert_eq!(hash_named_prefix("f_47D21316ABF2"), None);
        assert_eq!(hash_named_prefix("f_47d21316abf"), None);
    }

    /// A hash-named function the layout replaced is listed for the operator to
    /// drop; the layout's own functions and unrelated ones are not.
    #[tokio::test]
    async fn status_lists_hash_named_functions_the_layout_superseded() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("fns_{}", uuid::Uuid::new_v4().simple());
        let body = "RETURNS int LANGUAGE sql AS $f$ SELECT 1; $f$";
        let layout = format!(
            "CREATE SCHEMA {schema};\nCREATE FUNCTION {schema}.unpublish_scene_bbbbbbbbbbbb(text, text) {body};\n\
             CREATE FUNCTION {schema}.helper() {body};"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "{layout}\nCREATE FUNCTION {schema}.unpublish_scene_aaaaaaaaaaaa(text) {body};\n\
             CREATE FUNCTION {schema}.unpublish_scene_notes() {body};\n\
             CREATE FUNCTION {schema}.helper_aaaaaaaaaaaa() {body};"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let found = superseded_functions(&mut conn, &plan(&layout).unwrap())
            .await
            .unwrap();
        assert_eq!(
            found,
            vec![format!("{schema}.unpublish_scene_aaaaaaaaaaaa(text)")]
        );
        drop(conn);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A hash-named function the layout no longer declares stays off the list
    /// while a generated column, a column default or an index calls it: a step
    /// replaced its body in place, and dropping it would drop the column.
    #[tokio::test]
    async fn status_keeps_hash_named_functions_a_generated_column_or_index_calls() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("fns_{}", uuid::Uuid::new_v4().simple());
        let body = "RETURNS int LANGUAGE sql IMMUTABLE AS $f$ SELECT length($1); $f$";
        let layout = format!(
            "CREATE SCHEMA {schema};\nCREATE FUNCTION {schema}.live_until_bbbbbbbbbbbb(text) {body};"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "{layout}\n\
             CREATE FUNCTION {schema}.live_until_aaaaaaaaaaaa(text) {body};\n\
             CREATE FUNCTION {schema}.live_until_cccccccccccc(text) {body};\n\
             CREATE FUNCTION {schema}.live_until_dddddddddddd(text) {body};\n\
             CREATE FUNCTION {schema}.live_until_eeeeeeeeeeee(text) {body};\n\
             CREATE TABLE {schema}.event (\
               raw text, \
               live_until int GENERATED ALWAYS AS ({schema}.live_until_aaaaaaaaaaaa(raw)) STORED, \
               size int DEFAULT {schema}.live_until_cccccccccccc('default'));\n\
             CREATE INDEX event_live_until_idx ON {schema}.event (({schema}.live_until_dddddddddddd(raw)));"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let found = superseded_functions(&mut conn, &plan(&layout).unwrap())
            .await
            .unwrap();
        assert_eq!(
            found,
            vec![format!("{schema}.live_until_eeeeeeeeeeee(text)")]
        );
        drop(conn);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_older_database_is_brought_up_to_the_layout_and_drift_is_refused() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("mig_{}", uuid::Uuid::new_v4().simple());
        let older = format!(
            "CREATE SCHEMA {schema};\nCREATE TABLE {schema}.kept (id integer PRIMARY KEY);\nINSERT INTO {schema}.kept VALUES (1);"
        );
        let newer = format!(
            "CREATE SCHEMA {schema};\nCREATE TABLE {schema}.kept (id integer PRIMARY KEY);\nINSERT INTO {schema}.kept VALUES (1);\n\
             CREATE SEQUENCE {schema}.s;\nCREATE TABLE {schema}.added (id integer NOT NULL DEFAULT nextval('{schema}.s'::regclass), note text);\n\
             ALTER SEQUENCE {schema}.s OWNED BY {schema}.added.id;\nINSERT INTO {schema}.added (note) VALUES ('seed');\n\
             CREATE INDEX added_note_idx ON {schema}.added (note);\n\
             CREATE FUNCTION {schema}.f() RETURNS int LANGUAGE sql AS $f$ SELECT 1; $f$;"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(older.clone()))
            .execute(&pool)
            .await
            .unwrap();
        let before = gaps(&pool, &newer).await.unwrap();
        assert_eq!(before.fatal().len(), 3, "{:?}", before.fatal());
        assert_eq!(before.indexes().len(), 1);
        let done = migrate(&pool, &newer, "").await.unwrap();
        assert_eq!(done.created.len(), 4, "{done:?}");
        assert!(done.steps.is_empty());
        let seeded: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.added"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        let kept: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.kept"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (seeded, kept),
            (1, 1),
            "a new table's seed runs, an existing table's never again"
        );
        assert!(gaps(&pool, &newer).await.unwrap().fatal().is_empty());
        assert!(
            migrate(&pool, &newer, "").await.unwrap().is_empty(),
            "idempotent"
        );
        let drifted = newer.replace("note text)", "note text, extra boolean)");
        match migrate(&pool, &drifted, "").await {
            Err(MigrateError::Drift(cols)) => assert_eq!(cols, format!("{schema}.added.extra")),
            other => panic!("{other:?}"),
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A column a release added to an existing table arrives through a step:
    /// applied once, recorded with its digest, skipped on the next run, the
    /// constraint it adds skipped where schema.sql already declared it; a
    /// recorded step whose text changed is refused, a refused statement stops
    /// the run before anything is applied, and a drift no step covers rolls
    /// the whole transaction back, steps included.
    #[tokio::test]
    async fn steps_apply_once_are_recorded_and_the_refusals_leave_nothing_applied() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("stp_{}", uuid::Uuid::new_v4().simple());
        let tag = &schema[4..12];
        let older = format!(
            "CREATE SCHEMA {schema};\nCREATE TABLE {schema}.t (id integer PRIMARY KEY);\nINSERT INTO {schema}.t VALUES (1), (2);"
        );
        let newer = format!(
            "CREATE SCHEMA {schema};\nCREATE TABLE {schema}.t (id integer PRIMARY KEY, n integer NOT NULL DEFAULT 0, CONSTRAINT t_n_{tag}_check CHECK (n >= 0));\nINSERT INTO {schema}.t VALUES (1), (2);\nCREATE INDEX t_n_{tag}_idx ON {schema}.t (n);"
        );
        let step = |name: &str, body: &str| format!("-- ════ step {name} ════\n\n{body}\n");
        let body = format!(
            "ALTER TABLE {schema}.t ADD COLUMN IF NOT EXISTS n integer NOT NULL DEFAULT 0;\nALTER TABLE {schema}.t ADD CONSTRAINT t_n_{tag}_check CHECK (n >= 0);\nUPDATE {schema}.t SET n = id WHERE n = 0;"
        );
        let name = format!("9001_{tag}");
        let migrations = step(&name, &body);
        sqlx::raw_sql(sqlx::AssertSqlSafe(older))
            .execute(&pool)
            .await
            .unwrap();

        // without the step the column is drift, and nothing is applied
        match migrate(&pool, &newer, "").await {
            Err(MigrateError::Drift(cols)) => assert_eq!(cols, format!("{schema}.t.n")),
            other => panic!("{other:?}"),
        }
        // a refused statement stops the run before the transaction
        let bad = step(
            &format!("9002_{tag}"),
            &format!("ALTER TABLE {schema}.t DROP COLUMN id;"),
        );
        match migrate(&pool, &newer, &format!("{migrations}\n{bad}")).await {
            Err(MigrateError::StepRefused { step, why }) => {
                assert_eq!(step, format!("9002_{tag}"));
                assert!(why.contains("never drops a column"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        let recorded: i64 =
            sqlx::query_scalar("SELECT count(*) FROM meta.schema_steps WHERE step LIKE $1")
                .bind(format!("900%_{tag}"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(recorded, 0, "a refused run records nothing");
        let has_n: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = to_regclass($1) AND attname = 'n')").bind(format!("{schema}.t")).fetch_one(&pool).await.unwrap();
        assert!(!has_n, "a refused run applies nothing");

        // a drift the steps do not cover rolls the applied steps back too
        let more_drift = newer.replace(
            "n integer NOT NULL DEFAULT 0,",
            "n integer NOT NULL DEFAULT 0, m integer,",
        );
        match migrate(&pool, &more_drift, &migrations).await {
            Err(MigrateError::Drift(cols)) => assert_eq!(cols, format!("{schema}.t.m")),
            other => panic!("{other:?}"),
        }
        let has_n: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = to_regclass($1) AND attname = 'n')").bind(format!("{schema}.t")).fetch_one(&pool).await.unwrap();
        assert!(
            !has_n,
            "the step's column was rolled back with the drift refusal"
        );

        // the upgrade: step, then the objects, recorded
        let done = migrate(&pool, &newer, &migrations).await.unwrap();
        assert_eq!(done.steps, vec![name.clone()]);
        assert_eq!(done.created.len(), 1, "{:?}", done.created);
        let rows: Vec<(i32, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, n FROM {schema}.t ORDER BY id"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(1, 1), (2, 2)],
            "the rows survive and the backfill ran"
        );
        let (digest, fresh): (String, bool) =
            sqlx::query_as("SELECT sha256, created_fresh FROM meta.schema_steps WHERE step = $1")
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(digest, parse_steps(&migrations)[0].sha256);
        assert!(!fresh);
        assert!(gaps(&pool, &newer).await.unwrap().fatal().is_empty());
        assert!(
            migrate(&pool, &newer, &migrations)
                .await
                .unwrap()
                .is_empty(),
            "idempotent: the recorded step is skipped"
        );
        let st = status(&pool, &newer, &migrations).await.unwrap();
        assert_eq!((st.exit_code(), st.pending_steps.len()), (0, 0), "{st:?}");

        // a recorded step whose text changed is refused
        let changed = step(&name, &format!("{body}\nUPDATE {schema}.t SET n = 0;"));
        match migrate(&pool, &newer, &changed).await {
            Err(MigrateError::StepDrift { step, .. }) => assert_eq!(step, name),
            other => panic!("{other:?}"),
        }
        let st = status(&pool, &newer, &changed).await.unwrap();
        assert_eq!(
            (st.exit_code(), st.drifted_steps.clone()),
            (4, vec![name.clone()])
        );

        // a database that already declares the constraint (created by schema.sql) takes the step without a duplicate
        let fresh_schema = format!("{schema}_f");
        let fresh_sql = newer.replace(&schema, &fresh_schema);
        sqlx::raw_sql(sqlx::AssertSqlSafe(fresh_sql.clone()))
            .execute(&pool)
            .await
            .unwrap();
        let fresh_step = step(
            &format!("9003_{tag}"),
            &body.replace(&schema, &fresh_schema),
        );
        let done = migrate(&pool, &fresh_sql, &fresh_step).await.unwrap();
        assert_eq!(done.steps.len(), 1);
        let constraints: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conname = $1")
                .bind(format!("t_n_{tag}_check"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(constraints, 2, "one per table, none duplicated");

        sqlx::query("DELETE FROM meta.schema_steps WHERE step LIKE $1")
            .bind(format!("900%_{tag}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {schema} CASCADE; DROP SCHEMA {fresh_schema} CASCADE"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }

    /// A database that held world scenes before `worlds.world_scene_identity`:
    /// the real step (0010) moves each world's 'world' rows to its newest
    /// scene's identity and the object pass seeds one identity per scene with
    /// the same derived id; a world without a scene keeps its rows parked.
    #[tokio::test]
    async fn the_world_scene_identity_backfill_moves_each_world_scope_to_its_newest_scene() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("wsi_{}", uuid::Uuid::new_v4().simple());
        let tag = &schema[4..12];
        let older = format!(
            "CREATE SCHEMA {schema};
CREATE TABLE {schema}.world_scenes (world_name text NOT NULL, entity_id text NOT NULL, parcels text[] NOT NULL, created_at timestamp without time zone NOT NULL, PRIMARY KEY (world_name, entity_id));
CREATE TABLE {schema}.world_storage (realm text NOT NULL, scene_namespace text NOT NULL, key text NOT NULL, PRIMARY KEY (realm, scene_namespace, key));
CREATE TABLE {schema}.player_storage (realm text NOT NULL, scene_namespace text NOT NULL, player_address text NOT NULL, key text NOT NULL, PRIMARY KEY (realm, scene_namespace, player_address, key));
CREATE TABLE {schema}.env_variables (realm text NOT NULL, scene_namespace text NOT NULL, key text NOT NULL, PRIMARY KEY (realm, scene_namespace, key));
INSERT INTO {schema}.world_scenes VALUES ('solo.dcl.eth', 'bafysolo', '{{0,0}}', '2026-01-01'), ('multi.dcl.eth', 'bafyold', '{{0,0}}', '2026-01-01'), ('multi.dcl.eth', 'bafynew', '{{5,5}}', '2026-02-01');
INSERT INTO {schema}.world_storage VALUES ('solo.dcl.eth', 'world', 'k'), ('multi.dcl.eth', 'world', 'k'), ('gone.dcl.eth', 'world', 'k'), ('main', '1,1', 'k');
INSERT INTO {schema}.player_storage VALUES ('solo.dcl.eth', 'world', '0xabc', 'k');
INSERT INTO {schema}.env_variables VALUES ('multi.dcl.eth', 'world', 'secret');"
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(older.clone()))
            .execute(&pool)
            .await
            .unwrap();
        let from = crate::SCHEMA_SQL
            .find("CREATE TABLE worlds.world_scene_identity")
            .unwrap();
        let seed_end = "WHERE cardinality(parcels) > 0;";
        let to = from + crate::SCHEMA_SQL[from..].find(seed_end).unwrap() + seed_end.len();
        let shared = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/21_world_scene_places.sql"
        ));
        let newer = format!("{older}\n{}\n{shared}", &crate::SCHEMA_SQL[from..to])
            .replace("worlds.", &format!("{schema}."))
            .replace("explore.", &format!("{schema}."));
        let step = parse_steps(crate::MIGRATIONS_SQL)
            .into_iter()
            .find(|s| s.name == "0010_world_scene_storage_identity")
            .expect("the backfill step is embedded");
        let migrations = format!(
            "-- ════ step 9101_{tag} ════\n\n{}\n",
            step.sql.replace("worlds.", &format!("{schema}."))
        );
        let done = migrate(&pool, &newer, &migrations).await.unwrap();
        assert_eq!(done.steps, vec![format!("9101_{tag}")]);

        let scope = |table: &'static str, realm: &'static str| {
            let (pool, schema) = (pool.clone(), schema.clone());
            async move {
                sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                    "SELECT scene_namespace FROM {schema}.{table} WHERE realm = $1"
                )))
                .bind(realm)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let identity = |world: &'static str, entity: &'static str| {
            let (pool, schema) = (pool.clone(), schema.clone());
            async move {
                let derived: String = sqlx::query_scalar("SELECT md5($1 || '/' || $2)::uuid::text")
                    .bind(world)
                    .bind(entity)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                let seeded: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT EXISTS (SELECT 1 FROM {schema}.world_scene_place WHERE world_name = $1 AND identity::text = $2 AND active AND entity_id = $3)"
                )))
                .bind(world)
                .bind(&derived)
                .bind(entity)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert!(seeded, "{world}/{entity} seeded");
                derived
            }
        };
        let (solo, multi_new) = (
            identity("solo.dcl.eth", "bafysolo").await,
            identity("multi.dcl.eth", "bafynew").await,
        );
        identity("multi.dcl.eth", "bafyold").await;
        assert_eq!(scope("world_storage", "solo.dcl.eth").await, solo);
        assert_eq!(scope("player_storage", "solo.dcl.eth").await, solo);
        assert_eq!(
            scope("world_storage", "multi.dcl.eth").await,
            multi_new,
            "a multi-scene world's rows go to its newest scene"
        );
        assert_eq!(scope("env_variables", "multi.dcl.eth").await, multi_new);
        assert_eq!(
            scope("world_storage", "gone.dcl.eth").await,
            "world",
            "parked"
        );
        assert_eq!(scope("world_storage", "main").await, "1,1");
        assert!(
            migrate(&pool, &newer, &migrations)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("DELETE FROM meta.schema_steps WHERE step = $1")
            .bind(format!("9101_{tag}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }
    /// Upgrading an already-used 0010 database preserves random identities and
    /// every storage namespace, while installing an exact deployment binding.
    #[tokio::test]
    async fn shared_places_copy_legacy_identities_once_and_refuse_ambiguous_bindings() {
        let Some(pool) = crate::scratch::pool(2).await else {
            return;
        };
        let schema = format!("place_upgrade_{}", uuid::Uuid::new_v4().simple());
        let older = format!(
            r#"
CREATE SCHEMA {schema};
CREATE TABLE {schema}.world_scenes (world_name text NOT NULL, entity_id text NOT NULL, parcels text[] NOT NULL, PRIMARY KEY(world_name, entity_id));
CREATE TABLE {schema}.world_scene_identity (world_name text NOT NULL, identity uuid PRIMARY KEY, parcels text[] NOT NULL, active boolean NOT NULL, created_at timestamp NOT NULL DEFAULT now(), updated_at timestamp NOT NULL DEFAULT now());
CREATE TABLE {schema}.world_storage (realm text NOT NULL, scene_namespace text NOT NULL, key text NOT NULL, PRIMARY KEY(realm, scene_namespace, key));
CREATE TABLE {schema}.player_storage (realm text NOT NULL, scene_namespace text NOT NULL, key text NOT NULL, PRIMARY KEY(realm, scene_namespace, key));
CREATE TABLE {schema}.env_variables (realm text NOT NULL, scene_namespace text NOT NULL, key text NOT NULL, PRIMARY KEY(realm, scene_namespace, key));
INSERT INTO {schema}.world_scenes VALUES ('a.dcl.eth', 'same-cid', ARRAY['0,0']), ('b.dcl.eth', 'same-cid', ARRAY['0,0']);
INSERT INTO {schema}.world_scene_identity VALUES
 ('a.dcl.eth', '11111111-1111-4111-8111-111111111111', ARRAY['0,0'], true, now(), now()),
 ('b.dcl.eth', '22222222-2222-4222-8222-222222222222', ARRAY['0,0'], true, now(), now()),
 ('a.dcl.eth', '33333333-3333-4333-8333-333333333333', ARRAY['8,8'], false, now(), now()),
 ('gone.dcl.eth', '44444444-4444-4444-8444-444444444444', ARRAY['9,9'], true, now(), now());
INSERT INTO {schema}.world_storage SELECT world_name, identity::text, 'kept' FROM {schema}.world_scene_identity;
INSERT INTO {schema}.player_storage SELECT * FROM {schema}.world_storage;
INSERT INTO {schema}.env_variables SELECT * FROM {schema}.world_storage;
"#
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(older.clone()))
            .execute(&pool)
            .await
            .unwrap();
        let shared = include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/schemas/21_world_scene_places.sql"
        ))
        .replace("worlds.", &format!("{schema}."))
        .replace("explore.", &format!("{schema}."));
        let newer = format!("{older}\n{shared}");
        // A duplicate candidate aborts the whole object pass; it never guesses
        // which live deployment should gain the retained values.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {schema}.world_scenes VALUES ('a.dcl.eth','ambiguous',ARRAY['0,0'])"
        )))
        .execute(&pool)
        .await
        .unwrap();
        assert!(migrate(&pool, &newer, "").await.is_err());
        let absent: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NULL")
            .bind(format!("{schema}.world_scene_place"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(absent, "a failed seed rolls back its new table");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {schema}.world_scenes WHERE entity_id = 'ambiguous'"
        )))
        .execute(&pool)
        .await
        .unwrap();
        migrate(&pool, &newer, "").await.unwrap();
        let rows: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT world_name, identity::text, entity_id, active FROM {schema}.world_scene_place ORDER BY identity")))
            .fetch_all(&pool).await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (&rows[0].0, rows[0].2.as_deref(), rows[0].3),
            (&"a.dcl.eth".to_string(), Some("same-cid"), true)
        );
        assert_eq!(
            (&rows[1].0, rows[1].2.as_deref(), rows[1].3),
            (&"b.dcl.eth".to_string(), Some("same-cid"), true)
        );
        assert_ne!(
            rows[0].1, rows[1].1,
            "the same CID in different worlds has different places"
        );
        assert!(rows[2..].iter().all(|r| r.2.is_none() && !r.3));
        for table in ["world_storage", "player_storage", "env_variables"] {
            let matching: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {schema}.{table} s JOIN {schema}.world_scene_place p ON p.world_name = s.realm AND p.identity::text = s.scene_namespace")))
                .fetch_one(&pool).await.unwrap();
            assert_eq!(matching, 4, "every retained {table} namespace survives");
        }
        let duplicate = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {schema}.world_scene_place (world_name, identity, entity_id, parcels) VALUES ('a.dcl.eth',$1,'same-cid',ARRAY['7,7'])")))
            .bind(uuid::Uuid::new_v4()).execute(&pool).await.unwrap_err();
        assert_eq!(
            duplicate.as_database_error().unwrap().code().as_deref(),
            Some("23505")
        );
        // Runtime state is authoritative after the one-time copy: no replay
        // from a frozen legacy row can resurrect a retired identity.
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {schema}.world_scene_place SET active=false, entity_id=NULL WHERE world_name='a.dcl.eth'"))).execute(&pool).await.unwrap();
        assert!(migrate(&pool, &newer, "").await.unwrap().is_empty());
        let active: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {schema}.world_scene_place WHERE world_name='a.dcl.eth' AND active"))).fetch_one(&pool).await.unwrap();
        assert_eq!(active, 0);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .unwrap();
    }
}
