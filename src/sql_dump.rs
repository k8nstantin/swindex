//! Parse a `mysqldump --no-data` output into a [`GraphSource`].
//!
//! # What this module ships
//!
//! [`SqlDumpSource`] — a `GraphSource` impl that ingests a MySQL
//! schema dump and emits:
//!
//! * **One node per table.** `NodeKind = "table"`. Fresh `Uuid7` per
//!   table; the table's backtick-quoted name is *not* stored in
//!   `swindex` today (the index works on structural identity only —
//!   `[`Uuid7`]` is the source of truth). We keep an internal
//!   `BTreeMap<table_name, Uuid7>` so callers can look up "which uuid
//!   is the `customer` table?" via [`SqlDumpSource::uuid_of_table`].
//! * **One edge per foreign key.** `EdgeKind = "fk"`. Undirected
//!   (the structural relationship for clustering is symmetric, even
//!   though the FK has a direction in SQL).
//!
//! # Why hand-rolled and not a real SQL parser
//!
//! `mysqldump --no-data` output is *much* more constrained than
//! general SQL:
//!
//! * Comments are `-- ...` (line) or `/*!... */` (MySQL extension blocks).
//! * Each table is one self-contained `CREATE TABLE \`name\` ( ... )
//!   ENGINE=...;` statement, optionally followed by detached
//!   `ALTER TABLE ... ADD CONSTRAINT ...` for FKs.
//! * No subqueries, no procedures (we disable those at the dump
//!   step), no triggers.
//!
//! A regex or hand-roller covers this in ~250 LOC. Pulling in `sqlparser`
//! or similar (~50K LOC of dependency) would be overkill — and we'd
//! still write integration code at the FK extraction layer.
//!
//! # Privacy
//!
//! Schemas can be sensitive (table names alone leak business domains).
//! The repo's `.gitignore` blocks every `*.sql` file at every depth;
//! the *parser* commits, the schemas don't. Operator workflow:
//!
//! 1. `mysqldump --no-data … > data/schema.sql`  (data/ gitignored)
//! 2. `SqlDumpSource::from_path("data/schema.sql")?`
//! 3. Index built and queried locally; schema never reaches GitHub.
//!
//! # Known limitations
//!
//! * Multi-column foreign keys are still recorded as one edge (the
//!   structural relationship between two tables doesn't care which
//!   column count is involved).
//! * Tables referenced by an FK but not declared in the same dump
//!   are **dropped silently** with a `tracing` warning — common when
//!   a dump excludes some tables or uses cross-database FKs. We don't
//!   error because the surrounding good edges should still cluster.
//! * Self-referential FKs (table FK-ing itself) ARE captured. They
//!   become self-loops in the in-memory `Graph` (counted once in
//!   degree by the existing convention).
//! * MySQL `VIEW` definitions in the dump are skipped (no FKs, no
//!   structural role in clustering).

use crate::id::Uuid7;
use crate::node::{Edge, EdgeKind, Node, NodeKind};
use crate::source::GraphSource;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tracing::{debug, info, warn};

/// Errors from parsing a SQL dump.
#[derive(Debug)]
pub enum SqlDumpError {
    /// Reading the file failed (missing file, permission denied, etc.).
    Io(std::io::Error),
    /// The parser hit something it couldn't make sense of. The string
    /// names the failing construct + line number where possible.
    Parse(String),
}

impl fmt::Display for SqlDumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlDumpError::Io(e) => write!(f, "sql dump io error: {e}"),
            SqlDumpError::Parse(s) => write!(f, "sql dump parse error: {s}"),
        }
    }
}

impl std::error::Error for SqlDumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlDumpError::Io(e) => Some(e),
            SqlDumpError::Parse(_) => None,
        }
    }
}

impl From<std::io::Error> for SqlDumpError {
    fn from(e: std::io::Error) -> Self {
        SqlDumpError::Io(e)
    }
}

/// Parsed MySQL schema, ready to plug into [`SwIndex::build_from_source`].
///
/// Construct with [`SqlDumpSource::from_path`] or
/// [`SqlDumpSource::from_sql`]; pass to `build_from_source` like any
/// other [`GraphSource`]. Use [`SqlDumpSource::uuid_of_table`] to map
/// human-readable table names to their `Uuid7` for query inputs.
pub struct SqlDumpSource {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    /// `table_name` → `Uuid7` so the caller can look up "which uuid is
    /// the `customer` table?" for queries. Sorted by name (BTreeMap)
    /// for deterministic iteration.
    name_to_uuid: BTreeMap<String, Uuid7>,
}

impl SqlDumpSource {
    /// Parse a SQL dump file from disk.
    ///
    /// # Errors
    /// * [`SqlDumpError::Io`] if the file can't be opened or read.
    /// * [`SqlDumpError::Parse`] if the content isn't recognizable as
    ///   `mysqldump --no-data` output.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SqlDumpError> {
        let path = path.as_ref();
        debug!(?path, "reading sql dump");
        let text = std::fs::read_to_string(path)?;
        Self::from_sql(&text)
    }

    /// Parse a SQL dump from an in-memory string. Same content rules
    /// as [`Self::from_path`].
    ///
    /// Named `from_sql` rather than `from_str` so it doesn't get
    /// confused with the [`std::str::FromStr`] convention (which
    /// implies parsing any value from `&str`, not specifically a SQL
    /// dump).
    ///
    /// # Errors
    /// See [`Self::from_path`].
    pub fn from_sql(sql: &str) -> Result<Self, SqlDumpError> {
        let mut p = Parser::new(sql);
        p.run()?;

        // Resolve FK references: drop any whose target table isn't in
        // the dump. We log a warning per skip — common when the dump
        // is partial or has cross-database FKs.
        let mut name_to_uuid: BTreeMap<String, Uuid7> = BTreeMap::new();
        let mut nodes = Vec::with_capacity(p.tables.len());
        for name in &p.tables {
            let node = Node::fresh(NodeKind::new("table"));
            name_to_uuid.insert(name.clone(), node.id);
            nodes.push(node);
        }

        let mut edges = Vec::with_capacity(p.foreign_keys.len());
        let mut dropped = 0_usize;
        for (src, dst) in &p.foreign_keys {
            let (Some(&src_id), Some(&dst_id)) = (name_to_uuid.get(src), name_to_uuid.get(dst))
            else {
                dropped += 1;
                warn!(src = %src, dst = %dst, "FK references unknown table — skipping");
                continue;
            };
            edges.push(Edge::fresh(src_id, dst_id, EdgeKind::new("fk")));
        }

        info!(
            tables = nodes.len(),
            fks = edges.len(),
            dropped_fks = dropped,
            "sql dump parsed"
        );

        Ok(Self {
            nodes,
            edges,
            name_to_uuid,
        })
    }

    /// Number of tables ingested.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of foreign-key edges ingested (after dropping dangling
    /// references).
    #[must_use]
    pub fn fk_count(&self) -> usize {
        self.edges.len()
    }

    /// Look up the `Uuid7` assigned to a table by its dump name. Useful
    /// for feeding query seeds into `SwIndex::query`.
    ///
    /// Returns `None` if the table wasn't in the dump.
    #[must_use]
    pub fn uuid_of_table(&self, name: &str) -> Option<Uuid7> {
        self.name_to_uuid.get(name).copied()
    }

    /// Iterate `(table_name, uuid)` pairs in sorted name order. Used by
    /// the diagnostic test harness to label cluster members.
    pub fn tables_named(&self) -> impl Iterator<Item = (&str, Uuid7)> + '_ {
        self.name_to_uuid
            .iter()
            .map(|(name, &id)| (name.as_str(), id))
    }
}

impl GraphSource for SqlDumpSource {
    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        self.nodes.iter().cloned()
    }

    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().cloned()
    }

    fn node_count_hint(&self) -> Option<usize> {
        Some(self.nodes.len())
    }

    fn edge_count_hint(&self) -> Option<usize> {
        Some(self.edges.len())
    }

    /// Reverse the internal `name_to_uuid` map at call time to return
    /// the qualified table name (`db.table`) for this uuid. O(N) scan
    /// per call — fine because `build_from_source` calls this once per
    /// node during a build, never on the query hot path.
    fn label_of(&self, node_id: Uuid7) -> Option<String> {
        self.name_to_uuid
            .iter()
            .find_map(|(name, &id)| (id == node_id).then(|| name.clone()))
    }
}

// =========================================================================
// Parser internals — line-oriented scan of mysqldump output.
// =========================================================================

#[derive(Debug)]
struct Parser<'a> {
    src: &'a str,
    tables: Vec<String>,
    /// `(src_table, ref_table)` — order is "child references parent."
    /// Edges in the resulting graph are undirected, but we keep the
    /// source ordering here for diagnostic warning messages on
    /// dangling FKs.
    foreign_keys: Vec<(String, String)>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            tables: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// Walk the input once, extracting tables and FKs. Today this
    /// returns `Result` for forward-compat with parse errors we don't
    /// yet surface (e.g. future structural validations).
    #[allow(clippy::unnecessary_wraps)]
    fn run(&mut self) -> Result<(), SqlDumpError> {
        // We scan line-by-line. Three modes:
        // * `Outside`: looking for the start of a CREATE TABLE or a
        //   standalone ALTER TABLE.
        // * `InsideTable(name)`: inside a CREATE TABLE body, looking
        //   for inline CONSTRAINT ... FOREIGN KEY clauses. Exit when
        //   we see the closing paren of the body.
        // * `InsideRoutine`: inside a stored procedure / function /
        //   trigger / event body. mysqldump emits these between
        //   `DELIMITER //` (or `DELIMITER ;;`) ... `DELIMITER ;`
        //   markers. We skip everything in this mode — routine bodies
        //   contain SQL that LOOKS like CREATE TABLE / `)` lines but
        //   is internal to the routine. The user's real Gryphon
        //   schema has 1575 routines with ~7600 FROM/JOIN references
        //   inside them — without this skip, we miscount tables by
        //   ~19% and produce spurious table closes.
        //
        // A single line can BOTH open and close a CREATE TABLE
        // (single-line table declarations are common in dumps from
        // small schemas). The loop handles that by re-checking the
        // same line for InsideTable conditions after a transition.
        let mut mode = Mode::Outside;
        let mut seen_tables: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        // Track the current database via `USE \`db\`;` statements.
        // Multi-database dumps reuse table names across DBs — e.g. the
        // user's real schema has `tbl_WirelessBlockID` in 4 databases —
        // and those are genuinely *different* entities. We qualify
        // each table name as `<db>.<table>` so they get distinct
        // Uuid7s. Empty string means "no database context yet" which
        // we treat as a bare table name.
        let mut current_db: String = String::new();

        for raw_line in self.src.lines() {
            let line = strip_inline_comment(raw_line).trim_end();
            if line.is_empty() {
                continue;
            }

            // Step -1: `USE \`db\`;` updates the current database
            // context. Comes BEFORE all other handling so multi-DB
            // dumps qualify subsequent CREATE TABLE names correctly.
            if let Some(db_part) = line.trim_start().strip_prefix("USE ") {
                let trimmed = db_part.trim_end_matches(';').trim();
                if let Some((name, _)) = parse_backtick_identifier(trimmed) {
                    current_db = name;
                } else {
                    // Unquoted db name (rare in mysqldump but legal).
                    current_db = trimmed.to_string();
                }
                continue;
            }

            // Step 0: DELIMITER tracking. Switches in/out of routine mode.
            //
            // `DELIMITER //` (or `DELIMITER ;;`, `DELIMITER $$`, etc.)
            // signals the start of a non-standard-delimiter region —
            // mysqldump always wraps routine bodies this way. The
            // matching `DELIMITER ;` ends it.
            //
            // We check this BEFORE the mode dispatch so it works
            // regardless of what mode we think we're in (defensive
            // against state desync from malformed dumps).
            if let Some(delim) = line.trim_start().strip_prefix("DELIMITER ") {
                if delim.trim() == ";" {
                    mode = Mode::Outside;
                } else {
                    mode = Mode::InsideRoutine;
                }
                continue;
            }

            // Step 0.5: in InsideRoutine mode, skip everything. We
            // don't even look for table mentions here — that's the
            // job of a future MysqlProcedureSource that wants
            // co-occurrence edges, not table identity.
            if matches!(mode, Mode::InsideRoutine) {
                continue;
            }

            // Step 1: in Outside mode, look for a CREATE TABLE start
            // or a standalone ALTER TABLE FK declaration. If we find
            // a CREATE TABLE start, flip mode to InsideTable and FALL
            // THROUGH to step 2 — the *same* line may also contain
            // the table's body and closing paren.
            //
            // Also handle non-delimitered routines: `CREATE PROCEDURE`,
            // `CREATE FUNCTION`, `CREATE TRIGGER`, `CREATE EVENT`,
            // `CREATE DEFINER=...` followed by any of those. Some
            // dumps emit single-line routine signatures; we'd parse
            // them as tables otherwise.
            if matches!(mode, Mode::Outside) {
                if is_non_table_create(line) {
                    // Skip this single-line routine declaration.
                    continue;
                }
                if let Some(name) = parse_create_table_start(line) {
                    let qualified = qualify_table_name(&current_db, &name);
                    if seen_tables.insert(qualified.clone()) {
                        self.tables.push(qualified.clone());
                    }
                    mode = Mode::InsideTable(qualified);
                    // Fall through to InsideTable handling on this same line.
                } else {
                    if let Some((src_tbl, ref_tbl)) = parse_alter_table_add_fk(line) {
                        self.foreign_keys.push((
                            qualify_table_name(&current_db, &src_tbl),
                            qualify_table_name(&current_db, &ref_tbl),
                        ));
                    }
                    continue;
                }
            }

            // Step 2: in InsideTable mode (possibly just entered on
            // this same line), scan for inline FKs and for the
            // closing paren that ends the body.
            if let Mode::InsideTable(current) = mode.clone() {
                if let Some(ref_tbl) = parse_inline_foreign_key(line) {
                    self.foreign_keys
                        .push((current, qualify_table_name(&current_db, &ref_tbl)));
                }
                if line_closes_create_table(line) {
                    mode = Mode::Outside;
                }
            }
        }
        Ok(())
    }
}

/// True iff this line contains the closing paren of a CREATE TABLE
/// body. Matches:
/// * `) ENGINE=...;` (the common form)
/// * `)ENGINE=...;`
/// * a bare `)` or `);` at the start of the line (multi-line tables
///   that put `ENGINE=` on a separate line — rare but legal)
fn line_closes_create_table(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with(')') {
        return true;
    }
    contains_ci(line, ") ENGINE=") || contains_ci(line, ")ENGINE=")
}

#[derive(Clone)]
enum Mode {
    Outside,
    InsideTable(String),
    InsideRoutine,
}

/// Qualify a raw table name with the current database context.
///
/// * If `name` already contains a `.` (cross-database reference like
///   `other_db.tbl_foo`), return it unchanged — the dump explicitly
///   said which DB the table lives in.
/// * Otherwise, return `<db>.<name>` if a current database is set, or
///   `<name>` alone if we haven't seen a `USE` statement yet.
///
/// This is what separates `core_manager.tbl_WirelessBlockID` from
/// `cr_debug.tbl_WirelessBlockID` — same table name, different DB,
/// distinct entity for clustering purposes.
fn qualify_table_name(current_db: &str, name: &str) -> String {
    if name.contains('.') || current_db.is_empty() {
        name.to_string()
    } else {
        format!("{current_db}.{name}")
    }
}

/// True if this line looks like the start of a non-table CREATE
/// statement (PROCEDURE, FUNCTION, TRIGGER, EVENT, VIEW, DATABASE)
/// or a DEFINER-prefixed routine. Used to filter routine signatures
/// that aren't wrapped in `DELIMITER` blocks.
fn is_non_table_create(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Strip optional `DEFINER=...` clause that precedes the kind keyword.
    let after_definer = if let Some(rest) = trimmed.strip_prefix("CREATE DEFINER=") {
        // Skip past the definer spec (`user`@`host`) to the actual kind.
        // Format: CREATE DEFINER=`u`@`h` PROCEDURE/FUNCTION/...
        let rest = rest.trim_start();
        // Walk past two backtick-quoted identifiers separated by '@'.
        if let Some((_, after_first)) = parse_backtick_identifier(rest) {
            if let Some(after_at) = after_first.trim_start().strip_prefix('@') {
                if let Some((_, after_second)) = parse_backtick_identifier(after_at) {
                    after_second.trim_start()
                } else {
                    after_at.trim_start()
                }
            } else {
                after_first.trim_start()
            }
        } else {
            rest
        }
    } else if let Some(rest) = trimmed.strip_prefix("CREATE ") {
        rest
    } else {
        return false;
    };

    // mysqldump uppercases keywords; tolerate lowercase too.
    for kw in [
        "PROCEDURE ",
        "FUNCTION ",
        "TRIGGER ",
        "EVENT ",
        "VIEW ",
        "DATABASE ",
        "procedure ",
        "function ",
        "trigger ",
        "event ",
        "view ",
        "database ",
    ] {
        if after_definer.starts_with(kw) {
            return true;
        }
    }
    false
}

/// Strip a line-trailing `-- ...` comment. Does NOT handle `/* ... */`
/// (mysqldump only uses those for `/*!...*/` directives we ignore
/// wholesale via the line-level filter).
fn strip_inline_comment(line: &str) -> &str {
    // Avoid stripping inside backtick-quoted identifiers. mysqldump
    // doesn't put `--` inside identifiers in practice, but we err on
    // the side of caution: only strip when `--` is preceded by whitespace.
    if let Some(idx) = line.find(" -- ") {
        return &line[..idx];
    }
    if let Some(stripped) = line.strip_prefix("-- ") {
        return &stripped[..0];
    }
    if line.starts_with("--") {
        return "";
    }
    // Block comments `/* ... */` — handle the common case of a
    // wholly-block-commented line.
    if let Some(rest) = line.trim_start().strip_prefix("/*") {
        if rest.contains("*/") {
            return "";
        }
    }
    line
}

/// If the line starts a `CREATE TABLE \`name\` (` construct, return
/// the table name.
fn parse_create_table_start(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // Case-insensitive match on the `CREATE TABLE` prefix. mysqldump
    // always emits uppercase but be tolerant.
    let after_kw = trimmed
        .strip_prefix("CREATE TABLE ")
        .or_else(|| trimmed.strip_prefix("create table "))?;
    // `IF NOT EXISTS` clause is rare in dumps but accepted.
    let after_kw = after_kw.strip_prefix("IF NOT EXISTS ").unwrap_or(after_kw);
    parse_backtick_identifier(after_kw).map(|(name, _)| name)
}

/// Parse a standalone `ALTER TABLE \`x\` ADD CONSTRAINT \`...\`
/// FOREIGN KEY (\`...\`) REFERENCES \`y\` (\`...\`);` line. Returns
/// `Some((x, y))` if it's a recognized ALTER+FK; `None` otherwise.
/// Multi-line ALTER TABLE statements would land here only if the
/// whole thing happens to fit on one line — mysqldump usually does
/// emit them single-line.
fn parse_alter_table_add_fk(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let after_alter = trimmed
        .strip_prefix("ALTER TABLE ")
        .or_else(|| trimmed.strip_prefix("alter table "))?;

    let (src_tbl, after_src) = parse_backtick_identifier(after_alter)?;

    // Look for "ADD CONSTRAINT ... FOREIGN KEY ... REFERENCES `<dst>`"
    // We search rather than require a strict format — mysqldump may
    // include `ON DELETE`/`ON UPDATE`/etc. clauses.
    if !contains_ci(after_src, "FOREIGN KEY") {
        return None;
    }
    let ref_pos = find_references(after_src)?;
    let after_ref = &after_src[ref_pos..];
    let (dst_tbl, _) = parse_backtick_identifier(after_ref)?;
    Some((src_tbl, dst_tbl))
}

/// Parse an inline `CONSTRAINT \`...\` FOREIGN KEY (\`...\`)
/// REFERENCES \`y\` (\`...\`)` line within a CREATE TABLE body. Also
/// accepts the rarer `FOREIGN KEY ... REFERENCES \`y\` ...` without a
/// CONSTRAINT prefix.
fn parse_inline_foreign_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // Both forms are rooted by the literal `FOREIGN KEY`.
    if !contains_ci(trimmed, "FOREIGN KEY") {
        return None;
    }
    let ref_pos = find_references(trimmed)?;
    let after_ref = &trimmed[ref_pos..];
    parse_backtick_identifier(after_ref).map(|(name, _)| name)
}

/// Find the byte offset right after the `REFERENCES ` keyword in
/// `line`. Case-insensitive. Returns the position of the character
/// after the trailing space — i.e., where the referenced identifier
/// starts.
fn find_references(line: &str) -> Option<usize> {
    // Walk the line looking for "REFERENCES" (case-insensitive) with
    // a whitespace boundary on either side.
    let bytes = line.as_bytes();
    let needle = b"REFERENCES";
    let n = needle.len();
    let mut i = 0;
    while i + n <= bytes.len() {
        if bytes[i..i + n]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            // Confirm boundary: previous char must be non-alphanumeric
            // (or BOL), next char must be whitespace.
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let next_ok = bytes.get(i + n).is_some_and(u8::is_ascii_whitespace);
            if prev_ok && next_ok {
                // Skip the keyword + the whitespace separator(s).
                let mut j = i + n;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                return Some(j);
            }
        }
        i += 1;
    }
    None
}

/// Parse a leading backtick-quoted identifier from `s`. Returns the
/// identifier (without backticks) and the remaining string slice
/// starting after the closing backtick.
fn parse_backtick_identifier(s: &str) -> Option<(String, &str)> {
    let rest = s.trim_start();
    let after_open = rest.strip_prefix('`')?;
    let end = after_open.find('`')?;
    let name = after_open[..end].to_string();
    let tail = &after_open[end + 1..];
    Some((name, tail))
}

/// Case-insensitive `contains`. Avoids pulling in `regex` for this
/// one operation.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    for i in 0..=h.len() - n.len() {
        if h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::SqlDumpSource;
    use crate::source::GraphSource;

    /// A trimmed-down sakila-style fixture covering the common cases:
    /// inline FKs, ON DELETE / ON UPDATE clauses, multi-line CREATE
    /// TABLE, multiple FKs per table, comments. This is the
    /// "shoebox" parser-correctness test.
    const SHOEBOX_SCHEMA: &str = r"
-- Test schema: 4 tables, 4 FKs, 1 self-reference.

DROP TABLE IF EXISTS `customer`;
CREATE TABLE `customer` (
  `customer_id` int NOT NULL AUTO_INCREMENT,
  `first_name` varchar(45) NOT NULL,
  PRIMARY KEY (`customer_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

DROP TABLE IF EXISTS `address`;
CREATE TABLE `address` (
  `address_id` int NOT NULL AUTO_INCREMENT,
  `customer_id` int NOT NULL,
  PRIMARY KEY (`address_id`),
  CONSTRAINT `fk_address_customer` FOREIGN KEY (`customer_id`) REFERENCES `customer` (`customer_id`) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

DROP TABLE IF EXISTS `order_t`;
CREATE TABLE `order_t` (
  `order_id` int NOT NULL AUTO_INCREMENT,
  `customer_id` int NOT NULL,
  `parent_order_id` int DEFAULT NULL,
  PRIMARY KEY (`order_id`),
  CONSTRAINT `fk_order_customer` FOREIGN KEY (`customer_id`) REFERENCES `customer` (`customer_id`),
  CONSTRAINT `fk_order_parent` FOREIGN KEY (`parent_order_id`) REFERENCES `order_t` (`order_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

DROP TABLE IF EXISTS `order_item`;
CREATE TABLE `order_item` (
  `order_id` int NOT NULL,
  `product_id` int NOT NULL,
  PRIMARY KEY (`order_id`, `product_id`),
  CONSTRAINT `fk_oi_order` FOREIGN KEY (`order_id`) REFERENCES `order_t` (`order_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
";

    #[test]
    fn shoebox_schema_parses_4_tables() {
        let src = SqlDumpSource::from_sql(SHOEBOX_SCHEMA).unwrap();
        assert_eq!(src.table_count(), 4);
        // Sorted names are deterministic via BTreeMap.
        let names: Vec<&str> = src.tables_named().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["address", "customer", "order_item", "order_t"]);
    }

    #[test]
    fn shoebox_schema_parses_4_foreign_keys() {
        let src = SqlDumpSource::from_sql(SHOEBOX_SCHEMA).unwrap();
        // address->customer (1) + order_t->customer (1) +
        // order_t->order_t self-ref (1) + order_item->order_t (1) = 4.
        assert_eq!(src.fk_count(), 4);
    }

    #[test]
    fn graph_source_round_trip_works() {
        let src = SqlDumpSource::from_sql(SHOEBOX_SCHEMA).unwrap();
        // Iter contracts: nodes() yields exactly the table count;
        // edges() yields exactly the fk count; both repeatable.
        let n1: Vec<_> = src.nodes().collect();
        let n2: Vec<_> = src.nodes().collect();
        assert_eq!(n1.len(), 4);
        assert_eq!(n1, n2);
        let e1: Vec<_> = src.edges().collect();
        let e2: Vec<_> = src.edges().collect();
        assert_eq!(e1.len(), 4);
        assert_eq!(e1, e2);
    }

    #[test]
    fn dangling_fk_to_unknown_table_is_dropped_with_warning() {
        let sql = r"
CREATE TABLE `a` (
  `id` int NOT NULL,
  PRIMARY KEY (`id`),
  CONSTRAINT `fk_a_phantom` FOREIGN KEY (`id`) REFERENCES `phantom` (`id`)
) ENGINE=InnoDB;
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        assert_eq!(src.table_count(), 1);
        assert_eq!(
            src.fk_count(),
            0,
            "FK to undeclared table should be silently dropped"
        );
    }

    #[test]
    fn self_referential_fk_creates_self_loop() {
        let sql = r"
CREATE TABLE `tree` (
  `id` int NOT NULL,
  `parent_id` int DEFAULT NULL,
  PRIMARY KEY (`id`),
  CONSTRAINT `fk_tree_parent` FOREIGN KEY (`parent_id`) REFERENCES `tree` (`id`)
) ENGINE=InnoDB;
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        assert_eq!(src.table_count(), 1);
        assert_eq!(src.fk_count(), 1);
        let edges: Vec<_> = src.edges().collect();
        // Self-loop: source == target.
        assert_eq!(edges[0].source, edges[0].target);
    }

    #[test]
    fn standalone_alter_table_fk_parses() {
        let sql = r"
CREATE TABLE `a` ( `id` int NOT NULL, PRIMARY KEY (`id`) ) ENGINE=InnoDB;
CREATE TABLE `b` ( `id` int NOT NULL, `a_id` int, PRIMARY KEY (`id`) ) ENGINE=InnoDB;
ALTER TABLE `b` ADD CONSTRAINT `fk_b_a` FOREIGN KEY (`a_id`) REFERENCES `a` (`id`);
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        assert_eq!(src.table_count(), 2);
        assert_eq!(src.fk_count(), 1);
    }

    #[test]
    fn comments_and_block_directives_are_skipped() {
        let sql = r"
-- This is a line comment
/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;

CREATE TABLE `t` ( `id` int NOT NULL, PRIMARY KEY (`id`) ) ENGINE=InnoDB;
-- trailing comment
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        assert_eq!(src.table_count(), 1);
        assert_eq!(src.fk_count(), 0);
    }

    #[test]
    fn empty_dump_parses_to_empty_source() {
        let src = SqlDumpSource::from_sql("").unwrap();
        assert_eq!(src.table_count(), 0);
        assert_eq!(src.fk_count(), 0);
    }

    #[test]
    fn uuid_of_table_lookup_round_trip() {
        let src = SqlDumpSource::from_sql(SHOEBOX_SCHEMA).unwrap();
        let customer_uuid = src.uuid_of_table("customer").expect("customer table");
        let nodes: Vec<_> = src.nodes().collect();
        // The customer uuid should appear in the node list.
        assert!(nodes.iter().any(|n| n.id == customer_uuid));
        assert!(src.uuid_of_table("nonexistent").is_none());
    }

    /// Real-world finding from the Gryphon schema: same table name in
    /// multiple databases (4 copies of `tbl_WirelessBlockID`). They are
    /// **different entities** and must each become a distinct node.
    /// The parser tracks `USE \`db\`;` and qualifies every subsequent
    /// table as `db.table`.
    #[test]
    fn multi_database_dump_qualifies_table_names() {
        let sql = r"
CREATE DATABASE `db_a`;
USE `db_a`;
CREATE TABLE `shared_name` (
  `id` int NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
CREATE TABLE `a_specific` (
  `id` int NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;

CREATE DATABASE `db_b`;
USE `db_b`;
CREATE TABLE `shared_name` (
  `id` int NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
CREATE TABLE `b_specific` (
  `id` int NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        // 4 distinct entities even though there are only 3 distinct base names.
        assert_eq!(src.table_count(), 4);

        // The two `shared_name` tables get qualified names.
        let names: Vec<&str> = src.tables_named().map(|(n, _)| n).collect();
        assert!(names.contains(&"db_a.shared_name"));
        assert!(names.contains(&"db_b.shared_name"));
        assert!(names.contains(&"db_a.a_specific"));
        assert!(names.contains(&"db_b.b_specific"));

        // They have distinct uuids — proving they're separate nodes.
        let u_a = src.uuid_of_table("db_a.shared_name").unwrap();
        let u_b = src.uuid_of_table("db_b.shared_name").unwrap();
        assert_ne!(u_a, u_b);
    }

    #[test]
    fn delimiter_blocks_skip_routine_bodies() {
        // The single most consequential parser correctness test for
        // real-world dumps. mysqldump wraps procedures/functions in
        // `DELIMITER //;` ... `DELIMITER ;` blocks. Routine bodies
        // contain SQL that looks like CREATE TABLE / `)` lines but
        // is internal to the routine and must NOT count as a real
        // table. The user's Gryphon schema has 1575 routines.
        let sql = r"
USE `live`;
CREATE TABLE `real_table` ( `id` int NOT NULL, PRIMARY KEY (`id`) ) ENGINE=InnoDB;

DELIMITER ;;
CREATE DEFINER=`root`@`localhost` PROCEDURE `make_temp`()
BEGIN
  CREATE TABLE temp_inside_routine ( id int );
  INSERT INTO temp_inside_routine SELECT 1;
END ;;
DELIMITER ;

CREATE TABLE `real_table_2` ( `id` int NOT NULL, PRIMARY KEY (`id`) ) ENGINE=InnoDB;
";
        let src = SqlDumpSource::from_sql(sql).unwrap();
        // ONLY the two real tables; the temp inside the routine must
        // not be counted.
        assert_eq!(src.table_count(), 2);
        let names: Vec<&str> = src.tables_named().map(|(n, _)| n).collect();
        assert!(names.contains(&"live.real_table"));
        assert!(names.contains(&"live.real_table_2"));
        assert!(!names.iter().any(|n| n.contains("temp_inside_routine")));
    }
}
