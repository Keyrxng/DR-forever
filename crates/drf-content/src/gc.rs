// SPDX-License-Identifier: GPL-3.0-or-later

//! The GC config language (`*.gc`) parser.
//!
//! The language is a set of named blocks, each optionally `extends`-ing a
//! dotted target, with `Name = value, value;` property rows and nested blocks.
//! The reader is tolerant: it never fails on content, and a truncated file
//! simply parses short. Comments (`//` and `/* */`) and the anonymous-entry
//! marker `*` are removed before tokenising, exactly as the reference
//! `class_corpus.Grammar` does.

/// One parsed block.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub extends: Option<String>,
    pub props: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Node {
    fn new(name: &str, extends: Option<String>) -> Node {
        Node {
            name: name.to_string(),
            extends,
            props: Vec::new(),
            children: Vec::new(),
        }
    }

    /// The value of a `Name = ...;` row, or `None`.
    pub fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse a `.gc` file body (latin1-decoded bytes are acceptable).
pub fn parse(text: &str) -> Node {
    let tokens = tokenize(text);
    let mut parser = Parser { tokens, pos: 0 };
    let mut root = Node::new("<file>", None);
    parser.body(&mut root);
    root
}

/// True when a block names an `ItemModifier` child (its name or `extends`
/// target contains `mod`, case-folded). A `Description { }` block never does.
pub fn is_modifier(node: &Node) -> bool {
    let hay = format!("{} {}", node.name, node.extends.clone().unwrap_or_default()).to_lowercase();
    hay.contains("mod")
}

/// Every descendant of `node`, depth-first, in order.
pub fn descendants<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    for child in &node.children {
        out.push(child);
        descendants(child, out);
    }
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self, ahead: usize) -> Option<&str> {
        self.tokens.get(self.pos + ahead).map(|s| s.as_str())
    }

    fn body(&mut self, node: &mut Node) {
        while self.pos < self.tokens.len() {
            let token = match self.peek(0) {
                Some(t) => t.to_string(),
                None => return,
            };
            match token.as_str() {
                "}" => {
                    self.pos += 1;
                    return;
                }
                "{" | "=" | ";" => {
                    self.pos += 1;
                    continue;
                }
                "extends" => {
                    self.pos += 1;
                    let target = self.peek(0).map(str::to_string);
                    self.pos += 1;
                    let mut child = Node::new("<anonymous>", target);
                    node.children.push(child.clone());
                    if self.peek(0) == Some("{") {
                        self.pos += 1;
                        self.body(&mut child);
                        let idx = node.children.len() - 1;
                        node.children[idx] = child;
                    }
                    continue;
                }
                _ => {}
            }
            if self.peek(1) == Some("=") {
                self.pos += 2;
                let value = self.value();
                node.props.push((token, value));
                continue;
            }
            self.pos += 1;
            let name = token;
            let mut extends = None;
            if self.peek(0) == Some("extends") {
                self.pos += 1;
                extends = self.peek(0).map(str::to_string);
                self.pos += 1;
            }
            let mut child = Node::new(&name, extends);
            let has_body = self.peek(0) == Some("{");
            if has_body {
                self.pos += 1;
            }
            node.children.push(child.clone());
            let idx = node.children.len() - 1;
            if has_body {
                self.body(&mut child);
            }
            node.children[idx] = child;
        }
    }

    fn value(&mut self) -> String {
        let mut out: Vec<String> = Vec::new();
        while self.pos < self.tokens.len() && self.peek(0) != Some(";") {
            out.push(self.tokens[self.pos].clone());
            self.pos += 1;
        }
        self.pos += 1;
        out.join(" ")
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let stripped = strip_comments(text);
    let bytes: Vec<char> = stripped.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            '{' | '}' | '=' | ';' => {
                out.push(c.to_string());
                i += 1;
            }
            '*' => {
                // Anonymous-entry marker: dropped, like the reference replace.
                i += 1;
            }
            '"' => {
                // A string literal is ONE token, quotes included, exactly as
                // the reference `_Parser` tokenises it. Commas, `//` and `#`
                // inside a quest `Label` are content, not punctuation.
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != '"' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                out.push(bytes[start..i].iter().collect());
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                out.push(bytes[start..i].iter().collect());
            }
            '-' if i + 1 < bytes.len()
                && (bytes[i + 1].is_ascii_digit() || bytes[i + 1] == '.') =>
            {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                out.push(bytes[start..i].iter().collect());
            }
            '.' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                out.push(bytes[start..i].iter().collect());
            }
            _ => i += 1,
        }
    }
    out
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

fn strip_comments(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '"' {
            // A `//` inside a quoted value is prose, not a comment.
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != '"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            out.extend(&bytes[start..i]);
        } else if bytes[i] == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
            i += 2;
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
        } else if bytes[i] == '/' && i + 1 < bytes.len() && bytes[i + 1] == '*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blocks_props_and_extends() {
        let src = r#"
            // a comment
            Root extends base.Thing {
                Name = Value, 1;
                Child extends other.Leaf { X = 2; }
                * extends anon.Target { Y = 3; }
            }
        "#;
        let root = parse(src);
        assert_eq!(root.children.len(), 1);
        let r = &root.children[0];
        assert_eq!(r.name, "Root");
        assert_eq!(r.extends.as_deref(), Some("base.Thing"));
        assert_eq!(r.prop("Name"), Some("Value 1"));
        assert_eq!(r.children.len(), 2);
        assert_eq!(r.children[0].name, "Child");
        assert_eq!(r.children[1].name, "<anonymous>");
        assert_eq!(r.children[1].extends.as_deref(), Some("anon.Target"));
    }

    #[test]
    fn quoted_strings_are_one_token_and_keep_their_punctuation() {
        let root = parse("X {\n Label = \"Ticket #54,000,601\";\n Url = \"a//b\";\n}\n");
        let x = &root.children[0];
        assert_eq!(x.prop("Label"), Some("\"Ticket #54,000,601\""));
        assert_eq!(x.prop("Url"), Some("\"a//b\""));
    }

    #[test]
    fn leading_dot_floats_are_one_token() {
        let root = parse("X { GoldValue = .2; }");
        assert_eq!(root.children[0].prop("GoldValue"), Some(".2"));
    }

    #[test]
    fn negative_numbers_keep_their_sign() {
        let root = parse("X { ItemLevelDeltaNormal = -12; Y = -0.5; }");
        let x = &root.children[0];
        assert_eq!(x.prop("ItemLevelDeltaNormal"), Some("-12"));
        assert_eq!(x.prop("Y"), Some("-0.5"));
    }

    #[test]
    fn is_modifier_matches_name_or_target() {
        assert!(is_modifier(&Node::new("Mod101", None)));
        assert!(is_modifier(&Node::new(
            "x",
            Some("ItemAttributeModifier".into())
        )));
        assert!(!is_modifier(&Node::new("Description", None)));
    }
}
