//! GML loader: reads a Graph Modeling Language file into a [`GraphSource`].
//!
//! # What GML is
//!
//! GML (Graph Modeling Language) is the de-facto interchange format for
//! academic graph datasets. The Stanford SNAP collection, Mark Newman's
//! curated network corpus, the LFR benchmark generators, and NetworkX's
//! `write_gml` / `read_gml` all use it. A file looks like:
//!
//! ```text
//! Creator "anyone"
//! graph
//! [
//!   directed 0
//!   node
//!   [
//!     id 1
//!     label "Mr Hi"
//!   ]
//!   node
//!   [
//!     id 2
//!   ]
//!   edge
//!   [
//!     source 1
//!     target 2
//!     weight 4.0
//!   ]
//! ]
//! ```
//!
//! The grammar is simple: a top-level sequence of `key value` pairs,
//! where `value` is either a scalar (integer, float, quoted string) or
//! a nested `[ ... ]` list of more key-value pairs. Comments are lines
//! that start with `#`.
//!
//! # What this module implements
//!
//! Enough of the grammar to load Zachary's karate club, Stanford SNAP
//! datasets, and similar academic graphs. Specifically:
//!
//! * Identifiers, integers, floats, quoted strings, brackets, comments,
//!   arbitrary whitespace between tokens.
//! * Top-level keys other than `graph` are silently skipped (`Creator`,
//!   `Version`, etc.).
//! * Inside the `graph` list, every `node [ id N ... ]` entry mints a
//!   fresh [`Uuid7`] mapped from `N`, and every `edge [ source A target B ... ]`
//!   entry creates a new [`Edge`] linking the corresponding [`Uuid7`]s.
//! * Other fields on nodes and edges (`label`, `weight`, `value`, …)
//!   are parsed and discarded — the index only needs the structural
//!   skeleton.
//! * Other keys at the graph level (`directed`, `Version`, etc.) are
//!   parsed and discarded.
//!
//! # What this module deliberately does not do
//!
//! * **No payload preservation.** A node's `label "Mr Hi"` is parsed
//!   and thrown away. Payloads belong in the application's primary
//!   store, not the index — see the `node` module doc.
//! * **No multi-line strings.** GML allows `&quot;`-style escape
//!   sequences inside quoted strings; we don't decode them. Test
//!   fixtures should keep strings simple.
//! * **No directed-edge handling.** swindex's [`Edge`] is always
//!   directed (`source` -> `target`). GML's `directed 0` flag is
//!   parsed and ignored; an undirected GML file becomes a directed
//!   swindex graph with one edge per GML edge. If that's a problem
//!   for your data, the caller should add the reverse edge.

use crate::id::Uuid7;
use crate::node::{Edge, EdgeKind, Node, NodeId, NodeKind};
use crate::source::GraphSource;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// Errors that can occur while loading a GML file.
#[derive(Debug)]
pub enum GmlError {
    /// The file could not be read (missing, permission denied, mid-read
    /// IO error). Wraps the underlying [`std::io::Error`].
    Io(io::Error),

    /// The file's contents could not be parsed as GML. `line` is the
    /// 1-indexed source line where the lexer or parser gave up; `message`
    /// describes what was expected.
    Parse { line: usize, message: String },

    /// An `edge` block referenced a `source` or `target` id that no
    /// preceding `node` block had declared. `id` is the offending GML
    /// integer; `line` is the line on which the bad edge started.
    DanglingEdge { line: usize, id: i64 },
}

impl fmt::Display for GmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GmlError::Io(e) => write!(f, "gml I/O error: {e}"),
            GmlError::Parse { line, message } => {
                write!(f, "gml parse error at line {line}: {message}")
            }
            GmlError::DanglingEdge { line, id } => write!(
                f,
                "gml dangling edge at line {line}: references unknown node id {id}"
            ),
        }
    }
}

impl std::error::Error for GmlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GmlError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for GmlError {
    fn from(e: io::Error) -> Self {
        GmlError::Io(e)
    }
}

/// A [`GraphSource`] populated from a GML file or string.
///
/// `GmlSource` parses eagerly on construction — by the time you have a
/// `GmlSource` value, all nodes and edges are already in memory as
/// `Vec<Node>` / `Vec<Edge>`. That makes iteration cheap (a `slice::Iter`
/// clone) and trivially repeatable, satisfying [`GraphSource`]'s
/// re-iteration contract.
///
/// For graphs that don't fit in memory (10⁸+ nodes), this is the wrong
/// implementation — a streaming GML loader is the better choice and can
/// be a separate `StreamingGmlSource` type later. For test fixtures and
/// benchmark graphs in the 10⁴-10⁶ range, this is plenty.
#[derive(Debug)]
pub struct GmlSource {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl GmlSource {
    /// Load a GML file from disk.
    ///
    /// `node_kind` and `edge_kind` are stamped onto every parsed node and
    /// edge — GML has no native type system, so the caller picks the
    /// vocabulary. E.g. for Zachary karate you might use
    /// `NodeKind::new("member")` / `EdgeKind::new("friendship")`.
    ///
    /// # Errors
    ///
    /// * [`GmlError::Io`] — file not found, permission denied, or a
    ///   mid-read IO failure.
    /// * [`GmlError::Parse`] — the file isn't valid GML.
    /// * [`GmlError::DanglingEdge`] — an edge references an unknown id.
    pub fn from_path(
        path: impl AsRef<Path>,
        node_kind: &NodeKind,
        edge_kind: &EdgeKind,
    ) -> Result<Self, GmlError> {
        let text = fs::read_to_string(path)?;
        Self::from_str(&text, node_kind, edge_kind)
    }

    /// Parse a GML document from an in-memory string.
    ///
    /// Useful for unit tests and for callers that already have the GML
    /// text in hand (e.g. fetched over HTTP).
    ///
    /// # Errors
    ///
    /// Same as [`GmlSource::from_path`] minus the IO variants — a string
    /// is already in memory, so no [`GmlError::Io`] can occur.
    pub fn from_str(
        text: &str,
        node_kind: &NodeKind,
        edge_kind: &EdgeKind,
    ) -> Result<Self, GmlError> {
        let (nodes, edges) = parse(text, node_kind, edge_kind)?;
        Ok(Self { nodes, edges })
    }

    /// Number of parsed nodes — handy for tests and quick stat dumps.
    #[must_use]
    pub fn node_len(&self) -> usize {
        self.nodes.len()
    }

    /// Number of parsed edges.
    #[must_use]
    pub fn edge_len(&self) -> usize {
        self.edges.len()
    }
}

impl GraphSource for GmlSource {
    fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        // The `Vec` is already in memory; cloning each Node on iteration
        // is cheap (Uuid7 by value + a small String) and satisfies the
        // trait's "yields owned values" contract.
        self.nodes.iter().cloned()
    }

    fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.edges.iter().cloned()
    }

    fn node_count_hint(&self) -> Option<usize> {
        // Exact, not approximate — we know the count from parsing.
        Some(self.nodes.len())
    }

    fn edge_count_hint(&self) -> Option<usize> {
        Some(self.edges.len())
    }
}

// =========================================================================
// Parser internals — not part of the public API
// =========================================================================

/// A single lexical token, as produced by [`Lexer::next_token`].
///
/// Identifiers and strings borrow from the source text to avoid
/// allocation during lexing — the parser later clones the few strings
/// it actually keeps (top-level "graph" key checks, etc.).
#[derive(Debug)]
enum Tok<'a> {
    /// A bare word like `graph`, `node`, `id`, `source`.
    Ident(&'a str),
    /// A signed 64-bit integer literal.
    Int(i64),
    /// A floating-point literal (parsed via `str::parse::<f64>`).
    Float(f64),
    /// The contents of a `"..."` literal (without the surrounding quotes).
    /// Escape sequences are not decoded.
    Str(&'a str),
    /// `[`
    LBracket,
    /// `]`
    RBracket,
}

/// Byte-level cursor over the source text, with line tracking for errors.
struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        // Lines are 1-indexed for human-readable error messages.
        Self {
            src,
            pos: 0,
            line: 1,
        }
    }

    /// The current source line, useful for tagging errors.
    fn line(&self) -> usize {
        self.line
    }

    /// Advance past whitespace and `#`-to-end-of-line comments.
    /// Returns when `pos` either points at a real token byte or is past
    /// the end of input.
    fn skip_whitespace_and_comments(&mut self) {
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            match b {
                b'\n' => {
                    self.line += 1;
                    self.pos += 1;
                }
                b' ' | b'\t' | b'\r' => {
                    self.pos += 1;
                }
                b'#' => {
                    // Consume to end of line (the '\n' itself is handled
                    // on the next loop iteration so the line counter is
                    // bumped exactly once).
                    while self.pos < bytes.len() && bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Return the next token, or `Ok(None)` if input is exhausted.
    ///
    /// `Ok(Some(tok))` always corresponds to bytes that have been
    /// consumed; the caller does not need to advance the cursor.
    fn next_token(&mut self) -> Result<Option<Tok<'a>>, GmlError> {
        self.skip_whitespace_and_comments();
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() {
            return Ok(None);
        }

        let b = bytes[self.pos];
        match b {
            b'[' => {
                self.pos += 1;
                Ok(Some(Tok::LBracket))
            }
            b']' => {
                self.pos += 1;
                Ok(Some(Tok::RBracket))
            }
            b'"' => {
                // Quoted string. Consume up to the next unescaped quote.
                // We don't decode escape sequences — for the fixtures we
                // care about, raw bytes suffice.
                self.pos += 1; // opening quote
                let start = self.pos;
                while self.pos < bytes.len() && bytes[self.pos] != b'"' {
                    if bytes[self.pos] == b'\n' {
                        self.line += 1;
                    }
                    self.pos += 1;
                }
                if self.pos >= bytes.len() {
                    return Err(GmlError::Parse {
                        line: self.line,
                        message: "unterminated string literal".into(),
                    });
                }
                let s = &self.src[start..self.pos];
                self.pos += 1; // closing quote
                Ok(Some(Tok::Str(s)))
            }
            b'-' | b'+' | b'0'..=b'9' => self.next_number(),
            _ if b.is_ascii_alphabetic() || b == b'_' => Ok(Some(self.next_ident())),
            _ => Err(GmlError::Parse {
                line: self.line,
                message: format!("unexpected character {:?}", b as char),
            }),
        }
    }

    /// Lex an integer or float starting at the current byte.
    ///
    /// We greedily consume a run of characters that *might* be part of a
    /// number (digits, sign, decimal point, exponent), then ask Rust's
    /// number parser to validate it. If it parses as `i64` we yield
    /// `Tok::Int`; otherwise we try `f64` and yield `Tok::Float`. If
    /// neither works, we error out at the original line.
    fn next_number(&mut self) -> Result<Option<Tok<'a>>, GmlError> {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        // Optional leading sign
        if matches!(bytes[self.pos], b'+' | b'-') {
            self.pos += 1;
        }
        let mut saw_dot = false;
        let mut saw_exp = false;
        while self.pos < bytes.len() {
            let c = bytes[self.pos];
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' if !saw_dot && !saw_exp => {
                    saw_dot = true;
                    self.pos += 1;
                }
                b'e' | b'E' if !saw_exp => {
                    saw_exp = true;
                    self.pos += 1;
                    // Allow a sign right after the exponent marker.
                    if self.pos < bytes.len() && matches!(bytes[self.pos], b'+' | b'-') {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let raw = &self.src[start..self.pos];
        // Try integer first since most GML fields we care about (id,
        // source, target) are integers — integer parsing is also
        // strictly cheaper than float parsing.
        if let Ok(n) = raw.parse::<i64>() {
            return Ok(Some(Tok::Int(n)));
        }
        if let Ok(f) = raw.parse::<f64>() {
            return Ok(Some(Tok::Float(f)));
        }
        Err(GmlError::Parse {
            line: self.line,
            message: format!("invalid number literal {raw:?}"),
        })
    }

    /// Lex an identifier — a leading alphabetic-or-underscore byte, then
    /// any run of alphanumeric-or-underscore bytes.
    fn next_ident(&mut self) -> Tok<'a> {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        while self.pos < bytes.len() {
            let c = bytes[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        Tok::Ident(&self.src[start..self.pos])
    }
}

/// A parsed GML value — scalar or list.
///
/// Only used internally; we collect everything into this tree shape
/// during parsing, then walk it once to produce nodes and edges.
#[derive(Debug)]
enum Value {
    Int(i64),
    #[allow(dead_code)] // accepted but currently discarded
    Float(f64),
    #[allow(dead_code)] // accepted but currently discarded
    Str(String),
    List(Vec<(String, Value)>),
}

/// Parse a complete GML document into a list of nodes and edges.
///
/// This is the top-level entry point that [`GmlSource::from_str`] calls.
/// It does three things in sequence:
///
/// 1. Skip top-level keys until we hit `graph` (lets `Creator "..."`,
///    `Version "1.0"`, etc. precede the actual graph block as they do
///    in real fixtures).
/// 2. Parse the `graph [ ... ]` value into a `Value::List`.
/// 3. Walk the list, building a `BTreeMap<gml_id, NodeId>` as we see
///    `node` entries, then resolving edge endpoints through that map.
fn parse(
    text: &str,
    node_kind: &NodeKind,
    edge_kind: &EdgeKind,
) -> Result<(Vec<Node>, Vec<Edge>), GmlError> {
    let mut lex = Lexer::new(text);

    // ----- Phase 1: find the top-level `graph` value -----
    let entries = loop {
        let key_line = lex.line();
        match lex.next_token()? {
            None => {
                return Err(GmlError::Parse {
                    line: key_line,
                    message: "expected `graph` block, reached end of file".into(),
                });
            }
            Some(Tok::Ident("graph")) => {
                let v = parse_value(&mut lex)?;
                match v {
                    Value::List(entries) => break entries,
                    _ => {
                        return Err(GmlError::Parse {
                            line: key_line,
                            message: "`graph` must be a list".into(),
                        });
                    }
                }
            }
            Some(Tok::Ident(_)) => {
                // Top-level key other than `graph` — parse and discard
                // its value so the lexer cursor advances past it. This
                // is how we tolerate the leading `Creator "..."` line
                // common in real files.
                let _ = parse_value(&mut lex)?;
            }
            Some(other) => {
                return Err(GmlError::Parse {
                    line: key_line,
                    message: format!("expected top-level key, got {other:?}"),
                });
            }
        }
    };

    // ----- Phase 2: walk the graph entries, materialising nodes/edges -----

    // gml integer id -> swindex Uuid7. BTreeMap rather than HashMap so
    // the eventual node ordering is deterministic across runs (BTreeMap
    // iterates in key-order; tests rely on stable iteration).
    let mut id_to_uuid: BTreeMap<i64, NodeId> = BTreeMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    // We need line numbers for DanglingEdge errors, but parse_value
    // doesn't preserve them per-entry. The compromise: report the lexer
    // line at the point we finish parsing the graph (close enough for
    // debugging; perfectionist fix is to thread a line through Value).
    // Track a best-effort "current entry line" instead.
    let mut current_line = lex.line();

    for (key, value) in entries {
        // Walk through each top-level entry inside the `graph [ ... ]`.
        match key.as_str() {
            "node" => {
                // Expect a list-shaped value with at least an `id` field.
                let Value::List(fields) = value else {
                    return Err(GmlError::Parse {
                        line: current_line,
                        message: "`node` value must be a list".into(),
                    });
                };
                let id = find_int(&fields, "id").ok_or_else(|| GmlError::Parse {
                    line: current_line,
                    message: "`node` block missing required `id` field".into(),
                })?;
                // Mint a fresh Uuid7 and record the mapping so subsequent
                // edges that reference this gml id can resolve to it.
                // Uuid7::now() is monotonic within the process, so even
                // a tight loop of mints produces strictly increasing
                // values — see id::Uuid7::now docs.
                let uuid = Uuid7::now();
                id_to_uuid.insert(id, uuid);
                nodes.push(Node::new(uuid, node_kind.clone()));
            }
            "edge" => {
                let Value::List(fields) = value else {
                    return Err(GmlError::Parse {
                        line: current_line,
                        message: "`edge` value must be a list".into(),
                    });
                };
                let source = find_int(&fields, "source").ok_or_else(|| GmlError::Parse {
                    line: current_line,
                    message: "`edge` block missing required `source` field".into(),
                })?;
                let target = find_int(&fields, "target").ok_or_else(|| GmlError::Parse {
                    line: current_line,
                    message: "`edge` block missing required `target` field".into(),
                })?;
                // Resolve both endpoints. A dangling edge is a hard error
                // — silently dropping it would produce a graph that
                // doesn't match the source file, which is worse than
                // failing loudly here.
                let s_uuid = id_to_uuid
                    .get(&source)
                    .copied()
                    .ok_or(GmlError::DanglingEdge {
                        line: current_line,
                        id: source,
                    })?;
                let t_uuid = id_to_uuid
                    .get(&target)
                    .copied()
                    .ok_or(GmlError::DanglingEdge {
                        line: current_line,
                        id: target,
                    })?;
                edges.push(Edge::fresh(s_uuid, t_uuid, edge_kind.clone()));
            }
            _ => {
                // Other graph-level keys like `directed`, `Version`, and
                // any payload fields we don't care about. Skip silently
                // — the lexer already consumed them.
            }
        }
        // Best-effort line tracking — we lost the per-entry line in the
        // Value tree, so use the lexer's current line as an approximation.
        current_line = lex.line();
    }

    Ok((nodes, edges))
}

/// Look up the first `Value::Int` associated with a given key in a
/// list-of-pairs (the shape `parse_value` returns for a GML list).
/// Returns `None` if the key is absent or its value isn't an integer.
fn find_int(fields: &[(String, Value)], key: &str) -> Option<i64> {
    fields.iter().find_map(|(k, v)| {
        if k == key {
            match v {
                Value::Int(n) => Some(*n),
                _ => None,
            }
        } else {
            None
        }
    })
}

/// Parse a single value: scalar token or a `[ key value ... ]` list.
///
/// The lexer position must be at the start of the value (whitespace
/// allowed).
fn parse_value(lex: &mut Lexer<'_>) -> Result<Value, GmlError> {
    match lex.next_token()? {
        Some(Tok::Int(n)) => Ok(Value::Int(n)),
        Some(Tok::Float(f)) => Ok(Value::Float(f)),
        Some(Tok::Str(s)) => Ok(Value::Str(s.to_string())),
        Some(Tok::LBracket) => parse_list(lex),
        Some(Tok::RBracket) => Err(GmlError::Parse {
            line: lex.line(),
            message: "unexpected `]` while expecting a value".into(),
        }),
        Some(Tok::Ident(name)) => Err(GmlError::Parse {
            line: lex.line(),
            message: format!("expected a value, found identifier `{name}`"),
        }),
        None => Err(GmlError::Parse {
            line: lex.line(),
            message: "unexpected end of input while parsing a value".into(),
        }),
    }
}

/// Parse the body of a list: a sequence of `key value` pairs followed
/// by a closing `]`. The opening `[` is assumed to have already been
/// consumed by the caller.
fn parse_list(lex: &mut Lexer<'_>) -> Result<Value, GmlError> {
    let mut entries = Vec::new();
    loop {
        match lex.next_token()? {
            Some(Tok::RBracket) => return Ok(Value::List(entries)),
            Some(Tok::Ident(k)) => {
                let key = k.to_string();
                let value = parse_value(lex)?;
                entries.push((key, value));
            }
            Some(other) => {
                return Err(GmlError::Parse {
                    line: lex.line(),
                    message: format!("expected key or `]` inside list, got {other:?}"),
                });
            }
            None => {
                return Err(GmlError::Parse {
                    line: lex.line(),
                    message: "unexpected end of input inside list".into(),
                });
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::{GmlError, GmlSource};
    use crate::node::{EdgeKind, NodeKind};
    use crate::source::GraphSource;

    /// Tiny inline GML: 2 nodes, 1 edge, plus a top-level `Creator`
    /// scalar that must be tolerated.
    const MINI_GML: &str = r#"
        Creator "tests"
        graph [
            directed 0
            node [ id 1 ]
            node [ id 2 ]
            edge [ source 1 target 2 weight 1.0 ]
        ]
    "#;

    #[test]
    fn empty_graph_yields_no_nodes_or_edges() {
        // `graph [ ]` is a valid (if useless) GML document. Must parse
        // cleanly and produce a source that iterates to empty.
        let src =
            GmlSource::from_str("graph [ ]", &NodeKind::new("n"), &EdgeKind::new("e")).unwrap();
        assert_eq!(src.node_len(), 0);
        assert_eq!(src.edge_len(), 0);
        assert_eq!(src.nodes().count(), 0);
        assert_eq!(src.edges().count(), 0);
    }

    #[test]
    fn minimal_graph_parses_correctly() {
        // The tiny fixture above must yield exactly the right counts and
        // an edge whose endpoints match the parsed nodes.
        let src = GmlSource::from_str(MINI_GML, &NodeKind::new("person"), &EdgeKind::new("knows"))
            .unwrap();
        assert_eq!(src.node_len(), 2);
        assert_eq!(src.edge_len(), 1);

        let nodes: Vec<_> = src.nodes().collect();
        let edges: Vec<_> = src.edges().collect();

        assert_eq!(nodes[0].kind.as_str(), "person");
        assert_eq!(edges[0].kind.as_str(), "knows");
        // The single edge must reference both nodes — the loader
        // resolves source/target through the gml_id -> Uuid7 map.
        assert_eq!(edges[0].source, nodes[0].id);
        assert_eq!(edges[0].target, nodes[1].id);
    }

    #[test]
    fn iteration_is_repeatable() {
        // GraphSource contract clause (3): nodes() and edges() can be
        // called multiple times and yield the same sequence each time.
        let src = GmlSource::from_str(MINI_GML, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap();
        let a: Vec<_> = src.nodes().collect();
        let b: Vec<_> = src.nodes().collect();
        assert_eq!(a, b);
    }

    #[test]
    fn size_hints_are_exact() {
        // GmlSource has parsed everything upfront so the hint is tight.
        let src = GmlSource::from_str(MINI_GML, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap();
        assert_eq!(src.node_count_hint(), Some(2));
        assert_eq!(src.edge_count_hint(), Some(1));
    }

    #[test]
    fn missing_id_field_errors() {
        // A node without `id` is a parse error — not a silent fill-in.
        // We surface this so test fixtures that drift from the GML spec
        // fail loudly rather than produce a corrupt graph.
        let bad = "graph [ node [ label \"no id here\" ] ]";
        let err = GmlSource::from_str(bad, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap_err();
        match err {
            GmlError::Parse { message, .. } => {
                assert!(
                    message.contains("`id`"),
                    "expected id-missing error, got: {message}"
                );
            }
            other => panic!("expected GmlError::Parse, got {other:?}"),
        }
    }

    #[test]
    fn edge_references_unknown_node_errors() {
        // Edge referencing a node id that wasn't declared must surface
        // as a DanglingEdge error. Silently dropping it would produce a
        // graph that disagrees with the source file.
        let bad = "graph [ node [ id 1 ] edge [ source 1 target 99 ] ]";
        let err = GmlSource::from_str(bad, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap_err();
        match err {
            GmlError::DanglingEdge { id, .. } => assert_eq!(id, 99),
            other => panic!("expected DanglingEdge, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_string_errors() {
        // The lexer must surface unterminated strings rather than
        // running off the end of the buffer.
        let bad = r#"Creator "no closing quote"#;
        let err = GmlSource::from_str(bad, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap_err();
        match err {
            GmlError::Parse { message, .. } => {
                assert!(message.contains("unterminated"));
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn comments_and_extra_whitespace_are_ignored() {
        // GML allows `#` line-comments. The lexer's whitespace skipper
        // must consume them transparently.
        let with_comments = r#"
            # a top-level comment
            Creator "x"
            graph [   # comment after open
                node [ id 1 ]   # trailing
                # entire-line comment between entries
                node [ id 2 ]
                edge [ source 1 target 2 ]
            ]
        "#;
        let src =
            GmlSource::from_str(with_comments, &NodeKind::new("n"), &EdgeKind::new("e")).unwrap();
        assert_eq!(src.node_len(), 2);
        assert_eq!(src.edge_len(), 1);
    }

    /// Loads the Zachary karate club fixture from disk.
    ///
    /// This is the canonical small community-detection benchmark:
    /// 34 nodes, 78 undirected edges, two-faction ground-truth split.
    /// It's what Leiden gets tested against in PR #6/#7.
    #[test]
    fn zachary_karate_club_loads() {
        let src = GmlSource::from_path(
            "tests/fixtures/karate.gml",
            &NodeKind::new("member"),
            &EdgeKind::new("friendship"),
        )
        .unwrap();
        // Published counts: 34 nodes, 78 edges. If either drifts, the
        // fixture was edited and we want to know.
        assert_eq!(src.node_len(), 34);
        assert_eq!(src.edge_len(), 78);
        // The trait surface must work — this is the canary for the
        // future SwIndex::build_from_source(GmlSource::from_path(...)).
        assert_eq!(src.node_count_hint(), Some(34));
        assert_eq!(src.edge_count_hint(), Some(78));
        assert_eq!(src.nodes().count(), 34);
        assert_eq!(src.edges().count(), 78);
        // Every kind should be the one we asked for at load time.
        for n in src.nodes() {
            assert_eq!(n.kind.as_str(), "member");
        }
        for e in src.edges() {
            assert_eq!(e.kind.as_str(), "friendship");
        }
    }
}
