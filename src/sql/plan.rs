//! The operator tree shown by EXPLAIN and measured by EXPLAIN ANALYZE.
//!
//! The executor materialises each operator's output before the next one
//! runs, so every operator is one timed phase. While a statement is being
//! explained, each operator registers a node (label, estimated rows, input
//! nodes) and, when it finishes, records what it actually did. Running the
//! same operator again (a correlated subquery, the inner side of an index
//! nested-loop join) adds to the same node, so its counts become totals over
//! `loops` executions.
//!
//! Estimates come from the planner (`stats.rs`) before the operator runs.
//! Measurements (rows, rows examined, index entries read, time) are counted
//! while it runs. Plain EXPLAIN prints only estimates; EXPLAIN ANALYZE
//! prints both, labelled separately, and never fills a measurement from an
//! estimate.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) type NodeId = usize;

#[derive(Default)]
pub(super) struct Node {
    pub label: String,
    pub details: Vec<String>,
    /// Estimated rows produced per execution.
    pub est: Option<f64>,
    pub inputs: Vec<NodeId>,
    /// The operator whose expressions ran this one (subqueries), if any.
    pub parent: Option<NodeId>,
    /// Used as an input of another node.
    pub consumed: bool,
    // measurements
    pub loops: u64,
    pub rows: u64,
    pub examined: u64,
    pub index_entries: u64,
    pub time: Duration,
}

#[derive(Default)]
pub(super) struct Profile {
    pub analyze: bool,
    pub nodes: Vec<Node>,
    pub(super) by_key: HashMap<(Option<NodeId>, String, Vec<NodeId>, usize), NodeId>,
    /// Per parent, how often each (key, inputs) was registered during the
    /// parent's current execution: two identical operators in one execution
    /// (the branches of `SELECT x FROM a UNION SELECT x FROM a`) are two
    /// nodes, while the same operator in the parent's next execution is the
    /// same node, run again.
    pub(super) seen: HashMap<Option<NodeId>, HashMap<(String, Vec<NodeId>), usize>>,
    /// Operators currently running, innermost last.
    pub stack: Vec<NodeId>,
    /// Statement-level notes printed after the tree.
    pub notes: Vec<String>,
    /// Tables read, with their analyzed row count (None: no statistics).
    pub tables: std::collections::BTreeMap<String, Option<f64>>,
}

/// A running operator: pass back to `QCtx::done` with what it produced.
pub(super) struct Running {
    pub id: Option<NodeId>,
    pub start: Option<Instant>,
}

/// Work counted while an operator runs (beyond the rows it outputs).
#[derive(Default, Clone, Copy)]
pub(super) struct Work {
    /// Rows read from table storage, or candidate rows / pairs tested.
    pub examined: u64,
    /// Index entries read.
    pub index_entries: u64,
}

impl Profile {
    /// Find or create the node for an operator. `key` identifies it among its
    /// siblings (normally the label), so repeated executions share a node.
    pub fn node(&mut self, key: String, label: String, est: Option<f64>, inputs: Vec<NodeId>) -> NodeId {
        self.node_with(key, label, est, inputs, false)
    }

    /// A node that its parent runs repeatedly within one execution (a
    /// subquery evaluated per row): every registration is the same node.
    pub fn repeated_node(&mut self, key: String, label: String) -> NodeId {
        self.node_with(key, label, None, Vec::new(), true)
    }

    fn node_with(&mut self, key: String, label: String, est: Option<f64>, inputs: Vec<NodeId>, repeated: bool) -> NodeId {
        let parent = self.stack.last().copied();
        let occurrence = if repeated {
            0
        } else {
            let count = self.seen.entry(parent).or_default().entry((key.clone(), inputs.clone())).or_insert(0);
            *count += 1;
            *count - 1
        };
        let k = (parent, key, inputs.clone(), occurrence);
        if let Some(&id) = self.by_key.get(&k) {
            return id;
        }
        for &i in &inputs {
            self.nodes[i].consumed = true;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            label,
            est,
            inputs,
            parent,
            ..Node::default()
        });
        self.by_key.insert(k, id);
        id
    }

    /// `id` starts a new execution: what runs under it is counted afresh.
    pub fn started(&mut self, id: NodeId) {
        self.seen.remove(&Some(id));
    }

    pub fn finish(&mut self, id: NodeId, rows: usize, work: Work, elapsed: Option<Duration>) {
        let n = &mut self.nodes[id];
        n.loops += 1;
        n.rows += rows as u64;
        n.examined += work.examined;
        n.index_entries += work.index_entries;
        if let Some(e) = elapsed {
            n.time += e;
        }
    }

    pub fn detail(&mut self, id: NodeId, line: String) {
        let d = &mut self.nodes[id].details;
        if !d.contains(&line) {
            d.push(line);
        }
    }

    pub fn note(&mut self, line: String) {
        if !self.notes.contains(&line) {
            self.notes.push(line);
        }
    }

    /// The plan as text lines, root first, children indented under "-> ".
    pub fn render(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (id, n) in self.nodes.iter().enumerate() {
            if n.parent.is_none() && !n.consumed {
                self.render_node(id, 0, "", &mut out);
            }
        }
        if !self.tables.is_empty() {
            let parts: Vec<String> = self
                .tables
                .iter()
                .map(|(t, st)| match st {
                    Some(r) => format!("{t} (analyzed, {} rows)", fmt_rows(*r)),
                    None => format!("{t} (not analyzed: default estimates)"),
                })
                .collect();
            out.push(format!("Statistics: {}", parts.join(", ")));
        }
        out.extend(self.notes.iter().cloned());
        out
    }

    fn render_node(&self, id: NodeId, depth: usize, tag: &str, out: &mut Vec<String>) {
        let n = &self.nodes[id];
        let pad = "   ".repeat(depth);
        let arrow = if depth == 0 { "" } else { "-> " };
        let mut line = format!("{pad}{arrow}{tag}{}", n.label);
        if let Some(e) = n.est {
            line += &format!("  (est. rows={})", fmt_rows(e));
        }
        if self.analyze {
            if n.loops == 0 {
                line += "  (never executed)";
            } else {
                let ms = n.time.as_secs_f64() * 1e3;
                line += &format!("  (actual rows={} loops={} time={ms:.3} ms", n.rows, n.loops);
                if n.examined > 0 {
                    line += &format!(" examined={}", n.examined);
                }
                if n.index_entries > 0 {
                    line += &format!(" index_entries={}", n.index_entries);
                }
                line += ")";
            }
        }
        out.push(line);
        let dpad = format!("{pad}{}", if depth == 0 { "  " } else { "     " });
        for d in &n.details {
            out.push(format!("{dpad}{d}"));
        }
        for &i in &n.inputs {
            self.render_node(i, depth + 1, "", out);
        }
        for (cid, c) in self.nodes.iter().enumerate() {
            if c.parent == Some(id) && !c.consumed {
                self.render_node(cid, depth + 1, "", out);
            }
        }
    }
}

fn fmt_rows(e: f64) -> String {
    if !e.is_finite() {
        return "?".into();
    }
    let r = e.round();
    if r < 1.0 && e > 0.0 {
        "1".into()
    } else {
        format!("{}", r as u64)
    }
}
