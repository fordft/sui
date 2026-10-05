//! Syntax definitions and direct bindings, without resolving calls or references.
use tree_sitter::Node;

pub(super) fn definition<'a>(node: Node<'_>, source: &'a [u8]) -> Option<(&'static str, &'a str)> {
    let (kind, name) = match node.kind() {
        "variable_declarator"
        | "assignment_expression"
        | "pair"
        | "field_definition"
        | "public_field_definition" => binding(node)?,
        "type_alias_statement" => {
            let left = node.child_by_field_name("left")?;
            let name = left.named_child(0)?;
            let name = if name.kind() == "generic_type" {
                name.named_child(0)?
            } else {
                name
            };
            ("type", name)
        }
        "function_expression" | "generator_function" | "class" => {
            let name = node.child_by_field_name("name")?;
            // `const run = function run() {}` names one location twice. Distinct
            // inner expression names still define their own lexical binding.
            if repeats_binding(node, name, source) {
                return None;
            }
            (
                if node.kind() == "class" {
                    "class"
                } else {
                    "function"
                },
                name,
            )
        }
        kind => {
            let kind = match kind {
                "function_item"
                | "function_signature_item"
                | "function_signature"
                | "function_declaration"
                | "function_definition"
                | "generator_function_declaration" => "function",
                "method_definition"
                | "method_declaration"
                | "method_signature"
                | "abstract_method_signature" => "method",
                "struct_item" => "struct",
                "union_item" => "union",
                "enum_item" | "enum_declaration" => "enum",
                "trait_item" | "interface_declaration" => "interface",
                "class_definition" | "class_declaration" | "abstract_class_declaration" => "class",
                "type_item" | "type_spec" | "type_alias" | "type_alias_declaration" => "type",
                "mod_item" | "module" | "internal_module" => "module",
                "macro_definition" => "macro",
                "const_item" | "const_spec" | "static_item" => "constant",
                _ => return None,
            };
            (kind, node.child_by_field_name("name")?)
        }
    };
    Some((kind, name_text(name, source)?))
}

fn name_text<'a>(name: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if name.is_missing()
        || !matches!(
            name.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "field_identifier"
                | "private_property_identifier"
        )
    {
        return None;
    }
    let text = name.utf8_text(source).ok()?;
    (!text.is_empty()).then_some(text)
}

fn binding(node: Node<'_>) -> Option<(&'static str, Node<'_>)> {
    let (name, value, method) = match node.kind() {
        "variable_declarator" => (
            node.child_by_field_name("name")?,
            node.child_by_field_name("value")?,
            false,
        ),
        "pair" => (
            node.child_by_field_name("key")?,
            node.child_by_field_name("value")?,
            false,
        ),
        "assignment_expression" => {
            let left = expression(node.child_by_field_name("left")?);
            let name = if left.kind() == "member_expression" {
                left.child_by_field_name("property")?
            } else {
                left
            };
            (name, node.child_by_field_name("right")?, false)
        }
        "field_definition" | "public_field_definition" => (
            node.child_by_field_name("property")
                .or_else(|| node.child_by_field_name("name"))?,
            node.child_by_field_name("value")?,
            true,
        ),
        _ => return None,
    };
    let kind = match expression(value).kind() {
        "arrow_function" | "function_expression" | "generator_function" => {
            if method {
                "method"
            } else {
                "function"
            }
        }
        "class" => "class",
        _ => return None,
    };
    Some((kind, name))
}

/// Only wrappers that preserve the initializer itself. Calls, comma expressions,
/// conditional expressions and destructuring require semantics, so stop there.
fn expression(mut node: Node<'_>) -> Node<'_> {
    while let Some(inner) = wrapped_expression(node) {
        node = inner;
    }
    node
}

fn wrapped_expression(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "parenthesized_expression"
        | "as_expression"
        | "satisfies_expression"
        | "non_null_expression" => (0..node.named_child_count())
            .filter_map(|i| u32::try_from(i).ok().and_then(|i| node.named_child(i)))
            .find(|child| child.kind() != "comment"),
        // `<Handler>value` puts type_arguments before its expression.
        "type_assertion" => (0..node.named_child_count())
            .filter_map(|i| u32::try_from(i).ok().and_then(|i| node.named_child(i)))
            .find(|child| !matches!(child.kind(), "comment" | "type_arguments")),
        _ => None,
    }
}

fn repeats_binding(mut node: Node<'_>, name: Node<'_>, source: &[u8]) -> bool {
    while let Some(parent) = node.parent() {
        if wrapped_expression(parent) == Some(node) {
            node = parent;
            continue;
        }
        return binding(parent)
            .and_then(|(_, binding_name)| name_text(binding_name, source))
            .is_some_and(|binding_name| name_text(name, source) == Some(binding_name));
    }
    false
}
