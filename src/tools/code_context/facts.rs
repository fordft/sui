//! Cached syntax facts only. Every caller still walks policy and reads/hash-checks
//! the current source before using these ranges.
use anyhow::Result;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tree_sitter::{Node, ParseOptions, Parser};

use super::super::inventory::{self, Scan};
use super::MAX_NODES;

#[derive(Clone, Copy, Debug)]
pub(super) struct Region {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub(super) struct Definition {
    pub kind: &'static str,
    pub name: String,
    pub body: Region,
    pub attached_start: usize,
    pub parent: Option<Region>,
}

#[derive(Debug)]
pub(super) struct Import {
    pub region: Region,
    pub scope: Region,
}

#[derive(Debug, Default)]
pub(super) struct Facts {
    pub definitions: Vec<Definition>,
    pub imports: Vec<Import>,
    pub nodes: usize,
    pub syntax_error: bool,
}

impl Facts {
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.definitions.capacity() * std::mem::size_of::<Definition>()
            + self
                .definitions
                .iter()
                .map(|d| d.name.capacity())
                .sum::<usize>()
            + self.imports.capacity() * std::mem::size_of::<Import>()
    }
}

fn import_scope(node: Node<'_>) -> Region {
    let mut current = node;
    while let Some(parent) = current.parent() {
        current = parent;
        if matches!(
            current.kind(),
            "declaration_list" | "block" | "statement_block" | "module" | "program" | "source_file"
        ) {
            break;
        }
    }
    region(current)
}

fn region(node: Node<'_>) -> Region {
    let start = node.start_position().row + 1;
    // Tree-sitter's end point is exclusive; a closing newline belongs to the
    // preceding line, not the next sibling's first line.
    let point = node.end_position();
    let end = point.row + usize::from(point.column != 0);
    Region {
        start,
        end: end.max(start),
    }
}

fn attached_start(mut node: Node<'_>) -> usize {
    while let Some(parent) = node.parent() {
        let wrapper = matches!(parent.kind(), "decorated_definition" | "export_statement")
            || (matches!(
                parent.kind(),
                "lexical_declaration" | "variable_declaration"
            ) && parent
                .named_children(&mut parent.walk())
                .filter(|child| child.kind() == "variable_declarator")
                .count()
                == 1);
        if !wrapper {
            break;
        }
        node = parent;
    }
    let mut start = region(node).start;
    while let Some(previous) = node.prev_named_sibling() {
        let previous_region = region(previous);
        if !matches!(
            previous.kind(),
            "attribute_item" | "decorator" | "comment" | "line_comment" | "block_comment"
        ) || previous_region.end.saturating_add(1) < start
        {
            break;
        }
        start = previous_region.start;
        node = previous;
    }
    start
}

fn parent_header(node: Node<'_>) -> Option<Region> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "impl_item"
                | "trait_item"
                | "class_definition"
                | "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "interface_declaration"
        ) {
            let body = parent.child_by_field_name("body");
            let start = parent
                .parent()
                .filter(|wrapper| wrapper.kind() == "decorated_definition")
                .map_or(region(parent).start, |wrapper| region(wrapper).start);
            let end = body.map_or(region(parent).start, |body| {
                if parent.kind() == "class_definition" && body.kind() == "block" {
                    // Python blocks begin at their first statement, unlike
                    // braced bodies whose opening delimiter is header context.
                    body.start_position().row.max(region(parent).start)
                } else {
                    region(body).start
                }
            });
            return Some(Region { start, end });
        }
        current = parent.parent();
    }
    None
}

pub(super) fn parse(
    path: &Path,
    source: &str,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<Option<Facts>> {
    let Some(language) = inventory::language(path) else {
        return Ok(None);
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    );
    let Some(tree) = tree else {
        if !cancel.load(Ordering::Relaxed) {
            stats.stopped = Some("time_limit");
        }
        return Ok(None);
    };
    stats.parsed += 1;
    let mut facts = Facts {
        syntax_error: tree.root_node().has_error(),
        ..Facts::default()
    };
    if facts.syntax_error {
        stats.syntax_errors += 1;
    }
    let mut cursor = tree.walk();
    loop {
        if !inventory::checkpoint(stats, cancel, deadline) {
            return Ok(None);
        }
        if stats.nodes >= MAX_NODES {
            stats.stopped = Some("node_limit");
            return Ok(None);
        }
        stats.nodes += 1;
        facts.nodes += 1;
        let node = cursor.node();
        if let Some((kind, name)) = inventory::syntax_definition(node, source.as_bytes()) {
            facts.definitions.push(Definition {
                kind,
                name: name.to_owned(),
                body: region(node),
                attached_start: attached_start(node),
                parent: parent_header(node),
            });
        }
        if matches!(
            node.kind(),
            "use_declaration" | "import_statement" | "import_from_statement" | "import_declaration"
        ) {
            facts.imports.push(Import {
                region: region(node),
                scope: import_scope(node),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(Some(facts));
            }
        }
    }
}
