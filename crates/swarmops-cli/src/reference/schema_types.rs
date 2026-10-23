//! `docs/reference/schema-types.md`: every request, response and value type of the API, from
//! the JSON Schemas `swarmops schema` prints ([`schema_for_all`]): an index, then one section a
//! type with its description, its properties and, for an enumeration, its values.
//!
//! ```text
//! schema_for_all ─▶ ## Index                  one row a type: kind, first sentence
//!                   ## <Type>                 description, kind
//!                     | Property | Type | …    an object's properties
//!                     | Value | Description |  an enumeration's values
//!                     - one shape a line       a choice of objects, or of shapes
//! ```
//!
//! The doc comments the schemas carry cite the design documents in a few places (`(operations
//! spec §3)`): the page leaves those citations out ([`uncited`]), since it is public text.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::{BIN, GENERATED, Page, cell, first_sentence, paragraphs, slug, trim_end, yes_no};

/// The page's name in `docs/reference/`.
pub(super) const FILE: &str = "schema-types.md";

/// What a type's schema is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A string of a fixed list: `oneOf` constants.
    Enumeration,
    /// One of several objects: `oneOf` objects, no properties of its own.
    Choice,
    /// An object with properties, maybe constrained to one of several shapes.
    Object,
    /// A string.
    Text,
}

impl Kind {
    /// The kind of `schema`.
    fn of(schema: &Value) -> Kind {
        let alternatives = schema["oneOf"].as_array();
        let has_properties = schema.get("properties").is_some();

        // 1. A list of constants, or of objects.
        if let Some(alternatives) = alternatives.filter(|_| !has_properties) {
            if alternatives.iter().all(|alt| alt.get("const").is_some()) {
                return Kind::Enumeration;
            }
            return Kind::Choice;
        }

        // 2. A string, or an object.
        if schema["type"] == "string" {
            return Kind::Text;
        }
        Kind::Object
    }

    /// The kind as the page names it.
    fn name(self) -> &'static str {
        match self {
            Kind::Enumeration => "enumeration",
            Kind::Choice => "one of",
            Kind::Object => "object",
            Kind::Text => "string",
        }
    }
}

/// Renders `schema-types.md`.
pub(super) fn page() -> Page {
    Page {
        name: FILE.to_owned(),
        text: render(),
    }
}

/// The page's text.
fn render() -> String {
    let all = swarmops_api::schema_for_all();
    let types = all.as_object().cloned().unwrap_or_default();
    let mut out = String::new();

    // 1. Title and how to read the page.
    let _ = writeln!(out, "{GENERATED}\n");
    let _ = writeln!(out, "# Schema types\n");
    let _ = writeln!(
        out,
        "The {} request, response and value types of the API, as `{BIN} schema` prints their \
         JSON Schemas (2020-12); `{BIN} schema <TYPE>` prints one. The command line, the MCP \
         tools and the store exchange exactly this JSON.\n",
        types.len()
    );

    // 2. The index.
    let _ = writeln!(out, "## Index\n");
    let _ = writeln!(out, "| Type | Kind | Description |");
    let _ = writeln!(out, "| --- | --- | --- |");
    for (name, schema) in &types {
        let description = uncited(schema["description"].as_str().unwrap_or_default());
        let _ = writeln!(
            out,
            "| [`{name}`](#{}) | {} | {} |",
            slug(name),
            Kind::of(schema).name(),
            cell(first_sentence(&description))
        );
    }
    out.push('\n');

    // 3. One section a type.
    for (name, schema) in &types {
        type_section(&mut out, name, schema);
    }
    trim_end(out)
}

/// One type's section: its description and kind, then what its kind lists.
fn type_section(out: &mut String, name: &str, schema: &Value) {
    let kind = Kind::of(schema);
    let _ = writeln!(out, "## {name}\n");
    paragraphs(
        out,
        &uncited(schema["description"].as_str().unwrap_or_default()),
    );
    let _ = writeln!(
        out,
        "Kind: {}. `{BIN} schema {name}` prints its JSON Schema.\n",
        kind.name()
    );

    // What the kind lists.
    match kind {
        Kind::Enumeration => values(out, schema),
        Kind::Choice => choices(out, schema),
        Kind::Object => {
            properties(out, schema);
            shapes(out, schema);
        }
        Kind::Text => {}
    }
}

/// An enumeration's values, each with its description.
fn values(out: &mut String, schema: &Value) {
    let _ = writeln!(out, "| Value | Description |");
    let _ = writeln!(out, "| --- | --- |");
    for alt in &alternatives(schema) {
        let text = uncited(alt["description"].as_str().unwrap_or_default());
        let _ = writeln!(out, "| `{}` | {} |", alt["const"], cell(&text));
    }
    out.push('\n');
}

/// A choice of objects: one line each, its properties and its description.
fn choices(out: &mut String, schema: &Value) {
    let _ = writeln!(out, "One of:\n");
    for alt in &alternatives(schema) {
        let fields: Vec<String> = members(alt)
            .iter()
            .map(|(field, property)| format!("`{field}`: {}", type_name(property)))
            .collect();
        let text = uncited(alt["description"].as_str().unwrap_or_default());
        let _ = writeln!(out, "- {}. {}", fields.join(", "), cell(&text));
    }
    out.push('\n');
}

/// An object's properties: name, type, whether required, and description with its bounds.
fn properties(out: &mut String, schema: &Value) {
    let members = members(schema);
    if members.is_empty() {
        let _ = writeln!(out, "It has no properties.\n");
        return;
    }
    let required = required(schema);
    let _ = writeln!(out, "| Property | Type | Required | Description |");
    let _ = writeln!(out, "| --- | --- | --- | --- |");
    for (field, property) in &members {
        let needed = yes_no(required.contains(field));
        let _ = writeln!(
            out,
            "| `{field}` | {} | {needed} | {} |",
            type_name(property),
            description(property)
        );
    }
    out.push('\n');
}

/// The shapes an object must take, one of which holds: `result` is present, `ok` is `true`.
fn shapes(out: &mut String, schema: &Value) {
    let alternatives = alternatives(schema);
    if alternatives.is_empty() {
        return;
    }
    let always = required(schema);
    let _ = writeln!(out, "Exactly one of these shapes holds:\n");
    for alt in &alternatives {
        let _ = writeln!(out, "- {}.", shape(alt, &always));
    }
    out.push('\n');
}

/// One shape: which properties it requires and forbids, then what its properties hold; a
/// property neither it nor the object (`always`) requires holds that only if given.
fn shape(alt: &Value, always: &[String]) -> String {
    let mut parts = Vec::new();
    let needed = required(alt);

    // 1. Which are present, and which are not.
    for field in &needed {
        parts.push(format!("`{field}` is present"));
    }
    for field in required(&alt["not"]) {
        parts.push(format!("`{field}` is absent"));
    }

    // 2. What each named property holds: `create`, if given, is `false`.
    for (field, property) in members(alt) {
        let holds = if let Some(value) = property.get("const") {
            format!("`{value}`")
        } else if property["type"] == "null" {
            "null".to_owned()
        } else {
            format!("a {}", type_name(&property))
        };
        let present = needed.contains(&field) || always.contains(&field);
        if present {
            parts.push(format!("`{field}` is {holds}"));
        } else {
            parts.push(format!("`{field}`, if given, is {holds}"));
        }
    }
    parts.join("; ")
}

/// A property's description, without citations, then its default and bounds.
fn description(property: &Value) -> String {
    let mut text = cell(&uncited(
        property["description"].as_str().unwrap_or_default(),
    ));

    // Each fact the schema states besides, as a sentence.
    let mut facts = Vec::new();
    if let Some(default) = property.get("default") {
        facts.push(format!("Default: `{default}`."));
    }
    if let Some(min) = property["minimum"].as_u64().filter(|min| *min > 0) {
        facts.push(format!("At least {min}."));
    }
    if let Some(max) = property["maximum"].as_u64() {
        facts.push(format!("At most {max}."));
    }
    if let Some(max) = property["maxLength"].as_u64() {
        facts.push(format!("At most {max} characters."));
    }
    if let Some(pattern) = property["pattern"].as_str() {
        facts.push(format!("Matches `{pattern}`."));
    }
    for fact in facts {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&cell(&fact));
    }
    text
}

/// A property's type: `` `string` or `null` ``, ``array of [`Rev`](#rev)``, `` `integer`
/// (uint64) ``.
fn type_name(schema: &Value) -> String {
    // 1. A reference names its type, linked to its section.
    if let Some(reference) = schema["$ref"].as_str() {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        return format!("[`{name}`](#{})", slug(name));
    }

    // 2. A choice, by its variants.
    if let Some(variants) = schema["anyOf"].as_array() {
        let names: Vec<String> = variants.iter().map(type_name).collect();
        return names.join(" or ");
    }

    // 3. A JSON type, or several; an array by its items, a map by its values.
    let types: Vec<&str> = match &schema["type"] {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
        _ => return "any".to_owned(),
    };
    let shown: Vec<String> = types.iter().map(|name| json_type(name, schema)).collect();
    shown.join(" or ")
}

/// One JSON type of `schema`: an array by its items, an object by its values, an integer by its
/// format.
fn json_type(name: &str, schema: &Value) -> String {
    match name {
        "array" => format!("array of {}", type_name(&schema["items"])),
        "object" => match &schema["additionalProperties"] {
            Value::Object(_) => format!(
                "map of `string` to {}",
                type_name(&schema["additionalProperties"])
            ),
            _ => "`object`".to_owned(),
        },
        "integer" => match schema["format"].as_str() {
            Some(format) => format!("`integer` ({format})"),
            None => "`integer`".to_owned(),
        },
        _ => format!("`{name}`"),
    }
}

/// The `oneOf` alternatives of `schema`, or none.
fn alternatives(schema: &Value) -> Vec<Value> {
    schema["oneOf"].as_array().cloned().unwrap_or_default()
}

/// The properties of `schema`, in its order.
fn members(schema: &Value) -> Vec<(String, Value)> {
    let properties = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_else(Map::new);
    properties.into_iter().collect()
}

/// The names `schema` requires.
fn required(schema: &Value) -> Vec<String> {
    let names = schema["required"].as_array().cloned().unwrap_or_default();
    names
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

// ---- Citations ----

/// `text` without its citations of the design documents: a parenthesis that opens with one
/// (`(operations spec §3)`, `(operations spec §3, resolution path (b))`) goes with the space
/// before it, but for what follows a `: ` in it (`(storage spec §5: each …)` keeps `(each …)`);
/// a clause that cites one (`: the storage spec (§2 rule 3, §3) says what each repair is`) goes
/// up to the end of its sentence.
fn uncited(text: &str) -> String {
    let mut out = text.to_owned();
    while let Some(at) = out.find(" spec ") {
        // The words before ` spec `: a parenthesis that opens with the document's name, or a
        // clause that names it.
        let open = out[..at]
            .rfind('(')
            .filter(|open| !out[*open..at].contains(' '));
        let clause = out[..at].rfind(": the ").filter(|start| {
            let words = &out[start + ": the ".len()..at];
            !words.contains(' ')
        });
        out = match (open, clause) {
            (Some(open), _) => without_parenthesis(&out, open),
            (None, Some(start)) => without_clause(&out, start),
            (None, None) => panic!("an unknown citation form: {out}"),
        };
    }
    out
}

/// `text` without the parenthesis opening at `open`, or with only what follows a `: ` in it.
fn without_parenthesis(text: &str, open: usize) -> String {
    let close = closing(text, open);
    let inner = &text[open + 1..close];

    // A citation followed by words of its own keeps the words.
    if let Some((_, kept)) = inner.split_once(": ") {
        return format!("{}({kept}){}", &text[..open], &text[close + 1..]);
    }
    let before = text[..open].trim_end_matches(' ');
    format!("{before}{}", &text[close + 1..])
}

/// `text` without the clause from `start` (its `: `) to the end of its sentence.
fn without_clause(text: &str, start: usize) -> String {
    let end = text[start..]
        .find('.')
        .map_or(text.len(), |dot| start + dot);
    format!("{}{}", &text[..start], &text[end..])
}

/// Where the parenthesis opening at `open` closes, nested ones counted.
fn closing(text: &str, open: usize) -> usize {
    let mut depth = 0;
    for (at, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return open + at;
        }
    }
    panic!("an unclosed parenthesis: {text}")
}

// ---- Tests ----

/// Every type has an index row and a section, and no citation is left.
#[test]
fn every_type_is_on_the_page() {
    let text = render();
    let all = swarmops_api::schema_for_all();
    let types = all.as_object().unwrap();
    for name in types.keys() {
        assert!(
            text.contains(&format!("\n## {name}\n")),
            "{name}: no section"
        );
        assert!(
            text.contains(&format!("| [`{name}`](#{}) |", slug(name))),
            "{name}: no index row"
        );
    }
    assert!(!text.contains('§'), "a citation is left");
    assert!(!text.contains(" spec "), "a citation is left");
}

/// Every link to a type names a type of the page.
#[test]
fn type_links_resolve() {
    let text = render();
    let all = swarmops_api::schema_for_all();
    let anchors: Vec<String> = all
        .as_object()
        .unwrap()
        .keys()
        .map(|name| slug(name))
        .collect();
    for part in text.split("](#").skip(1) {
        let anchor = part.split(')').next().unwrap();
        assert!(anchors.iter().any(|a| a == anchor), "#{anchor}: no type");
    }
}

/// Kinds, types and citations read as the page needs.
#[test]
fn parts_are_sound() {
    use serde_json::json;

    assert_eq!(
        Kind::of(&json!({"oneOf": [{"const": "a"}, {"const": "b"}]})),
        Kind::Enumeration
    );
    assert_eq!(
        Kind::of(&json!({"oneOf": [{"type": "object"}]})),
        Kind::Choice
    );
    assert_eq!(Kind::of(&json!({"type": "string"})), Kind::Text);
    assert_eq!(
        type_name(&json!({"type": ["string", "null"]})),
        "`string` or `null`"
    );
    assert_eq!(
        type_name(&json!({"type": "array", "items": {"$ref": "#/$defs/Rev"}})),
        "array of [`Rev`](#rev)"
    );
    assert_eq!(
        type_name(&json!({"type": "integer", "format": "uint64"})),
        "`integer` (uint64)"
    );
    assert_eq!(uncited("Who (operations spec §5)."), "Who.");
    assert_eq!(
        uncited("Values (storage spec §5: each list) of them."),
        "Values (each list) of them."
    );
    assert_eq!(
        uncited("Decided (operations spec §3, path (b)). Each"),
        "Decided. Each"
    );
    assert_eq!(
        uncited("No paths: the storage spec (§2 rule 3, §3) says what. Next"),
        "No paths. Next"
    );
}
