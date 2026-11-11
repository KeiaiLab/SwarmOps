//! A JSON value checked against a type's JSON Schema, as `swarmops_api::schema_for_all` gives
//! it: [`check`] (the M21 design §2.5).
//!
//! It knows the subset of JSON Schema 2020-12 that `schema_for_all` emits, and nothing else: a
//! keyword it does not know panics, so a schema that grows one fails the tests until this
//! checker learns it, never passes unread. Every keyword of the type's schema is vetted before
//! the value is read, the parts the value never reaches too.
//!
//! ```text
//! schema_for_all() ─▶ { "CommitResp": { "$defs": {..}, "properties": {..}, .. }, .. }
//!                                  │
//! check(all, "CommitResp", value) ─┴▶ each keyword of the type's schema, `$ref` read in its
//!                                     `$defs`; errors as "/result/entries/0/name: <what>"
//! ```

use serde_json::{Map, Value};

/// The keywords that only annotate a schema: they constrain no value.
const ANNOTATIONS: [&str; 5] = ["$schema", "title", "description", "default", "$defs"];

/// The keywords [`check`] applies to a value.
const KEYWORDS: [&str; 14] = [
    "$ref",
    "type",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "oneOf",
    "anyOf",
    "not",
    "minimum",
    "maximum",
    "maxLength",
    "pattern",
];

/// What `$ref` starts with: a schema in the type's own `$defs`, the only references
/// `schema_for_all` makes.
const DEFS_REF: &str = "#/$defs/";

/// The integer formats `schema_for_all` gives, each with its least and greatest value.
const INTEGER_FORMATS: [(&str, i128, i128); 10] = [
    ("uint8", 0, u8::MAX as i128),
    ("uint16", 0, u16::MAX as i128),
    ("uint32", 0, u32::MAX as i128),
    ("uint64", 0, u64::MAX as i128),
    ("uint", 0, usize::MAX as i128),
    ("int8", i8::MIN as i128, i8::MAX as i128),
    ("int16", i16::MIN as i128, i16::MAX as i128),
    ("int32", i32::MIN as i128, i32::MAX as i128),
    ("int64", i64::MIN as i128, i64::MAX as i128),
    ("int", isize::MIN as i128, isize::MAX as i128),
];

/// Checks `value` against the schema of type `ty` in `schema`, the object `schema_for_all`
/// returns (each type's schema under its name): every way it breaks the schema, each as
/// `<JSON pointer>: <what>`.
///
/// Panics on a type `schema` does not have and on a keyword this checker does not know.
pub fn check(schema: &Value, ty: &str, value: &Value) -> Result<(), Vec<String>> {
    let root = schema
        .get(ty)
        .unwrap_or_else(|| panic!("no type {ty} in the schema"));
    vet(root, ty);
    let mut errors = Vec::new();
    Checker { root }.check(root, value, "", &mut errors);
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors)
}

/// Panics on a keyword of `schema`, or of any schema inside it, that [`check`] does not know,
/// and on a pattern or a format it cannot read: `where_` names the place for the message.
fn vet(schema: &Value, where_: &str) {
    let Value::Object(map) = schema else {
        assert!(
            schema.is_boolean(),
            "{where_}: a schema is an object or a boolean"
        );
        return;
    };
    for (key, arg) in map {
        let place = format!("{where_}/{key}");
        match key.as_str() {
            "properties" | "$defs" => {
                let inner = arg.as_object().expect("an object of schemas");
                inner
                    .iter()
                    .for_each(|(name, inner)| vet(inner, &format!("{place}/{name}")));
            }
            "oneOf" | "anyOf" => {
                let inner = arg.as_array().expect("an array of schemas");
                for inner in inner {
                    vet(inner, &place);
                }
            }
            "items" | "additionalProperties" | "not" => vet(arg, &place),
            "pattern" => {
                parse(arg.as_str().expect("a pattern is a string"));
            }
            "format" => {
                let name = arg.as_str().expect("a format is a string");
                let known = INTEGER_FORMATS.iter().any(|(format, _, _)| *format == name);
                assert!(known, "{place}: unknown format {name}");
            }
            _ if ANNOTATIONS.contains(&key.as_str()) || KEYWORDS.contains(&key.as_str()) => {}
            other => panic!("{where_}: unknown keyword {other}"),
        }
    }
}

/// One type's check: its schema, which `$ref`s are read against.
struct Checker<'a> {
    root: &'a Value,
}

impl Checker<'_> {
    /// Checks `value`, at pointer `at`, against `schema`, a part of the root's; adds what fails
    /// to `errors`.
    fn check(&self, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
        // 1. A boolean schema allows every value or none.
        let map = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                errors.push(format!("{at}: allowed by no schema"));
                return;
            }
            Value::Object(map) => map,
            other => panic!("{at}: a schema is an object or a boolean, not {other}"),
        };

        // 2. Every keyword, each on its own: 2020-12 applies `$ref` beside its siblings.
        for (key, arg) in map {
            self.keyword(key, arg, map, value, at, errors);
        }
    }

    /// Applies one keyword of `map`, `key` with its argument `arg`, to `value`.
    fn keyword(
        &self,
        key: &str,
        arg: &Value,
        map: &Map<String, Value>,
        value: &Value,
        at: &str,
        errors: &mut Vec<String>,
    ) {
        match key {
            _ if ANNOTATIONS.contains(&key) => {}
            "$ref" => self.check(self.target(arg, at), value, at, errors),
            "type" => type_is(arg, value, at, errors),
            "const" => {
                if value != arg {
                    errors.push(format!("{at}: {value} is not {arg}"));
                }
            }
            "properties" => self.properties(arg, value, at, errors),
            "required" => required(arg, value, at, errors),
            "additionalProperties" => self.additional(arg, map, value, at, errors),
            "items" => self.items(arg, value, at, errors),
            "oneOf" | "anyOf" => self.branches(key, arg, value, at, errors),
            "not" => {
                let mut inner = Vec::new();
                self.check(arg, value, at, &mut inner);
                if inner.is_empty() {
                    errors.push(format!("{at}: matches the schema of not"));
                }
            }
            "minimum" | "maximum" => bound(key, arg, value, at, errors),
            "maxLength" => max_length(arg, value, at, errors),
            "pattern" => {
                let pattern = arg.as_str().expect("a pattern is a string");
                let matched = value.as_str().is_none_or(|text| matches(pattern, text));
                if !matched {
                    errors.push(format!("{at}: {value} does not match {pattern}"));
                }
            }
            "format" => format_is(arg, value, at, errors),
            other => panic!("{at}: unknown keyword {other}"),
        }
    }

    /// The schema `$ref` `arg` names in the root's `$defs`.
    fn target(&self, arg: &Value, at: &str) -> &Value {
        let name = arg
            .as_str()
            .and_then(|reference| reference.strip_prefix(DEFS_REF))
            .unwrap_or_else(|| panic!("{at}: a $ref outside {DEFS_REF}: {arg}"));
        self.root["$defs"]
            .get(name)
            .unwrap_or_else(|| panic!("{at}: $ref {name} is not in $defs"))
    }

    /// `properties`: each field present against its schema.
    fn properties(&self, arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
        let Some(fields) = value.as_object() else {
            return;
        };
        let schemas = arg.as_object().expect("properties is an object");
        for (name, schema) in schemas {
            if let Some(field) = fields.get(name) {
                self.check(schema, field, &child(at, name), errors);
            }
        }
    }

    /// `additionalProperties`: each field `properties` does not name, against `arg`.
    fn additional(
        &self,
        arg: &Value,
        map: &Map<String, Value>,
        value: &Value,
        at: &str,
        errors: &mut Vec<String>,
    ) {
        let Some(fields) = value.as_object() else {
            return;
        };
        let named = map.get("properties").and_then(Value::as_object);
        for (name, field) in fields {
            if named.is_some_and(|named| named.contains_key(name)) {
                continue;
            }
            self.check(arg, field, &child(at, name), errors);
        }
    }

    /// `items` (2020-12: one schema): each element against it.
    fn items(&self, arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
        assert!(!arg.is_array(), "{at}: items as an array is not 2020-12");
        let Some(elements) = value.as_array() else {
            return;
        };
        for (index, element) in elements.iter().enumerate() {
            self.check(arg, element, &child(at, &index.to_string()), errors);
        }
    }

    /// `oneOf` (exactly one branch matches) or `anyOf` (at least one does).
    fn branches(&self, key: &str, arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
        let branches = arg.as_array().expect("oneOf and anyOf are arrays");
        let matched = (branches.iter())
            .filter(|branch| {
                let mut inner = Vec::new();
                self.check(branch, value, at, &mut inner);
                inner.is_empty()
            })
            .count();
        let total = branches.len();
        if key == "oneOf" && matched != 1 {
            errors.push(format!(
                "{at}: matches {matched} of the {total} schemas of oneOf, not 1"
            ));
        }
        if key == "anyOf" && matched == 0 {
            errors.push(format!(
                "{at}: matches none of the {total} schemas of anyOf"
            ));
        }
    }
}

/// The pointer of `name` below `at`, escaped as RFC 6901 says: `a/b` → `/a~1b`.
fn child(at: &str, name: &str) -> String {
    format!("{at}/{}", name.replace('~', "~0").replace('/', "~1"))
}

/// `type`: one type name or a list of them.
fn type_is(arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let names: Vec<&str> = match arg {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names
            .iter()
            .map(|name| name.as_str().expect("a type name is a string"))
            .collect(),
        other => panic!("{at}: type is a name or a list, not {other}"),
    };
    if names.iter().any(|name| has_type(name, value)) {
        return;
    }
    errors.push(format!(
        "{at}: {value} is not of type {}",
        names.join(" or ")
    ));
}

/// Whether `value` is of JSON Schema type `name`; an integer is a number without a fraction.
fn has_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => integer(value).is_some(),
        other => panic!("unknown type {other}"),
    }
}

/// `value` as an integer, when it is a number without a fraction.
fn integer(value: &Value) -> Option<i128> {
    if let Some(unsigned) = value.as_u64() {
        return Some(i128::from(unsigned));
    }
    if let Some(signed) = value.as_i64() {
        return Some(i128::from(signed));
    }
    let float = value.as_f64()?;
    #[allow(clippy::cast_possible_truncation)]
    (float.fract() == 0.0).then_some(float as i128)
}

/// `required`: each named field present in an object.
fn required(arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(fields) = value.as_object() else {
        return;
    };
    let names = arg.as_array().expect("required is an array");
    for name in names {
        let name = name.as_str().expect("a required name is a string");
        if !fields.contains_key(name) {
            errors.push(format!("{}: missing, which is required", child(at, name)));
        }
    }
}

/// `minimum` or `maximum`: a number no smaller, or no larger, than `arg`.
fn bound(key: &str, arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    if !value.is_number() {
        return;
    }

    // Integers compare exactly; anything with a fraction as floats.
    let below = match (integer(value), integer(arg)) {
        (Some(value), Some(arg)) => value < arg,
        _ => value.as_f64() < arg.as_f64(),
    };
    let above = match (integer(value), integer(arg)) {
        (Some(value), Some(arg)) => value > arg,
        _ => value.as_f64() > arg.as_f64(),
    };
    if key == "minimum" && below {
        errors.push(format!("{at}: {value} is less than {arg}"));
    }
    if key == "maximum" && above {
        errors.push(format!("{at}: {value} is more than {arg}"));
    }
}

/// `maxLength`: a string of at most `arg` characters.
fn max_length(arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(text) = value.as_str() else {
        return;
    };
    let most = arg.as_u64().expect("maxLength is a count");
    let length = text.chars().count() as u64;
    if length > most {
        errors.push(format!("{at}: {length} characters, more than {most}"));
    }
}

/// `format`: an integer format's range, for a number.
fn format_is(arg: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    let name = arg.as_str().expect("a format is a string");
    let (_, least, greatest) = INTEGER_FORMATS
        .iter()
        .find(|(format, _, _)| *format == name)
        .unwrap_or_else(|| panic!("{at}: unknown format {name}"));
    if !value.is_number() {
        return;
    }
    let fits = integer(value).is_some_and(|number| (*least..=*greatest).contains(&number));
    if !fits {
        errors.push(format!("{at}: {value} is not a {name}"));
    }
}

// ---- Patterns ----

/// One step of a pattern: the characters it takes, and how many times in a row.
struct Atom {
    set: CharSet,
    least: usize,
    most: Option<usize>,
}

/// The characters one step takes.
enum CharSet {
    Any,
    One(char),
    Class {
        negated: Negated,
        ranges: Vec<(char, char)>,
    },
}

/// Whether a class takes the characters outside its ranges (`[^a-z]`).
#[derive(Clone, Copy, PartialEq)]
enum Negated {
    No,
    Yes,
}

impl CharSet {
    /// Whether the set takes `c`.
    fn has(&self, c: char) -> bool {
        match self {
            CharSet::Any => c != '\n',
            CharSet::One(one) => *one == c,
            CharSet::Class { negated, ranges } => {
                let inside = ranges.iter().any(|(low, high)| (*low..=*high).contains(&c));
                inside != (*negated == Negated::Yes)
            }
        }
    }
}

/// A parsed pattern: anchored at its start (`^`), at its end (`$`), and its steps.
struct Pattern {
    start: bool,
    end: bool,
    atoms: Vec<Atom>,
}

/// Whether `text` matches the ECMA-262 `pattern`, of the subset `schema_for_all` uses:
/// `^`, `$`, `.`, literal and escaped punctuation, classes with ranges, and the quantifiers
/// `*`, `+`, `?` and `{n}`, `{n,}`, `{n,m}`. Anything else panics.
fn matches(pattern: &str, text: &str) -> bool {
    let parsed = parse(pattern);
    let chars: Vec<char> = text.chars().collect();
    let last = if parsed.start { 0 } else { chars.len() };
    (0..=last).any(|from| parsed.match_at(0, &chars[from..]))
}

impl Pattern {
    /// Whether steps `step..` match the start of `rest` (all of it, when anchored at the end),
    /// each taking as many characters as it can and giving them back one at a time.
    fn match_at(&self, step: usize, rest: &[char]) -> bool {
        let Some(atom) = self.atoms.get(step) else {
            return !self.end || rest.is_empty();
        };
        let most = (rest.iter())
            .take(atom.most.unwrap_or(usize::MAX))
            .take_while(|c| atom.set.has(**c))
            .count();
        if most < atom.least {
            return false;
        }
        (atom.least..=most)
            .rev()
            .any(|taken| self.match_at(step + 1, &rest[taken..]))
    }
}

/// Parses `pattern`; panics on what [`matches()`] does not know.
fn parse(pattern: &str) -> Pattern {
    let chars: Vec<char> = pattern.chars().collect();
    let start = chars.first() == Some(&'^');
    let mut at = usize::from(start);
    let mut end = false;
    let mut atoms = Vec::new();
    while let Some(&c) = chars.get(at) {
        assert!(!end, "pattern {pattern}: `$` before its end");
        at += 1;
        let set = match c {
            '$' => {
                end = true;
                continue;
            }
            '.' => CharSet::Any,
            '\\' => CharSet::One(escaped(&chars, &mut at, pattern)),
            '[' => class(&chars, &mut at, pattern),
            '(' | ')' | '|' | '*' | '+' | '?' | '{' | '}' | '^' | ']' => {
                panic!("pattern {pattern}: unknown construct {c}")
            }
            c => CharSet::One(c),
        };
        let (least, most) = quantifier(&chars, &mut at, pattern);
        atoms.push(Atom { set, least, most });
    }
    Pattern { start, end, atoms }
}

/// The punctuation an escape at `at` (past its `\`) stands for: `\.` → `.`; a letter's
/// escape (`\d`, `\w`) is not known.
fn escaped(chars: &[char], at: &mut usize, pattern: &str) -> char {
    let c = *chars
        .get(*at)
        .unwrap_or_else(|| panic!("pattern {pattern}: ends in \\"));
    assert!(
        c.is_ascii_punctuation(),
        "pattern {pattern}: unknown escape \\{c}"
    );
    *at += 1;
    c
}

/// A class from `at`, past its `[`, through its `]`: `[a-z0-9._-]`, `[^/]`.
fn class(chars: &[char], at: &mut usize, pattern: &str) -> CharSet {
    let negated = if chars.get(*at) == Some(&'^') {
        *at += 1;
        Negated::Yes
    } else {
        Negated::No
    };
    let mut ranges = Vec::new();
    loop {
        let c = *chars
            .get(*at)
            .unwrap_or_else(|| panic!("pattern {pattern}: an unclosed class"));
        *at += 1;
        let low = match c {
            ']' => return CharSet::Class { negated, ranges },
            '\\' => escaped(chars, at, pattern),
            '[' => panic!("pattern {pattern}: unknown construct [ in a class"),
            c => c,
        };

        // A range, `a-z`; a `-` before the `]` is itself.
        let span = chars.get(*at) == Some(&'-') && chars.get(*at + 1).is_some_and(|c| *c != ']');
        if !span {
            ranges.push((low, low));
            continue;
        }
        let high = chars[*at + 1];
        assert!(
            high != '\\' && low <= high,
            "pattern {pattern}: range {low}-{high}"
        );
        *at += 2;
        ranges.push((low, high));
    }
}

/// The quantifier at `at`, if any: how many times the step before it repeats.
fn quantifier(chars: &[char], at: &mut usize, pattern: &str) -> (usize, Option<usize>) {
    let bounds = match chars.get(*at) {
        Some('*') => (0, None),
        Some('+') => (1, None),
        Some('?') => (0, Some(1)),
        Some('{') => {
            let close = (chars[*at..].iter())
                .position(|c| *c == '}')
                .unwrap_or_else(|| panic!("pattern {pattern}: an unclosed {{"));
            let inside: String = chars[*at + 1..*at + close].iter().collect();
            *at += close;
            counts(&inside, pattern)
        }
        _ => return (1, Some(1)),
    };
    *at += 1;
    let lazy = matches!(chars.get(*at), Some('?' | '+'));
    assert!(
        !lazy,
        "pattern {pattern}: unknown lazy or possessive quantifier"
    );
    bounds
}

/// `{n}`, `{n,}` or `{n,m}`'s inside: the least and most repeats.
fn counts(inside: &str, pattern: &str) -> (usize, Option<usize>) {
    let number = |digits: &str| {
        digits
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("pattern {pattern}: {{{inside}}}"))
    };
    match inside.split_once(',') {
        None => (number(inside), Some(number(inside))),
        Some((least, "")) => (number(least), None),
        Some((least, most)) => (number(least), Some(number(most))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{check, matches};

    /// A schema in `schema_for_all`'s shape: one type, `Thing`, with a field of each kind the
    /// checks need and an enum in `$defs`.
    fn schema() -> Value {
        json!({
            "Thing": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Thing",
                "description": "A thing.",
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Its name."},
                    "count": {"type": "integer", "format": "uint32", "minimum": 0},
                    "kind": {"$ref": "#/$defs/Kind"},
                },
                "required": ["name", "kind"],
                "$defs": {
                    "Kind": {
                        "oneOf": [
                            {"const": "file", "type": "string"},
                            {"const": "dir", "type": "string"},
                        ],
                    },
                },
            },
        })
    }

    /// A field `required` names, missing.
    #[test]
    fn the_checker_refuses_a_missing_field() {
        let all = schema();
        assert_eq!(
            check(&all, "Thing", &json!({"name": "a", "kind": "file"})),
            Ok(())
        );
        let errors = check(&all, "Thing", &json!({"kind": "file"})).unwrap_err();
        assert_eq!(errors, ["/name: missing, which is required"]);
    }

    /// A string where an integer goes, and an integer past its format's range.
    #[test]
    fn the_checker_refuses_a_wrong_type() {
        let all = schema();
        let wrong = json!({"name": "a", "kind": "dir", "count": "7"});
        let errors = check(&all, "Thing", &wrong).unwrap_err();
        assert_eq!(errors, ["/count: \"7\" is not of type integer"]);
        let past = json!({"name": "a", "kind": "dir", "count": 4_294_967_296_u64});
        let errors = check(&all, "Thing", &past).unwrap_err();
        assert_eq!(errors, ["/count: 4294967296 is not a uint32"]);
    }

    /// A string none of an enum's constants is.
    #[test]
    fn the_checker_refuses_a_string_outside_the_enum() {
        let all = schema();
        let outside = json!({"name": "a", "kind": "symlink"});
        let errors = check(&all, "Thing", &outside).unwrap_err();
        assert_eq!(
            errors,
            ["/kind: matches 0 of the 2 schemas of oneOf, not 1"]
        );
    }

    /// A keyword the checker does not know is a panic, never a pass.
    #[test]
    #[should_panic(expected = "unknown keyword uniqueItems")]
    fn an_unknown_keyword_panics() {
        let mut all = schema();
        all["Thing"]["properties"]["name"]["uniqueItems"] = json!(true);
        let _ = check(&all, "Thing", &json!({"name": "a", "kind": "file"}));
    }

    /// A keyword the value never reaches is vetted too: one in an unused `$defs` entry.
    #[test]
    #[should_panic(expected = "unknown keyword if")]
    fn an_unreached_unknown_keyword_panics() {
        let mut all = schema();
        all["Thing"]["$defs"]["Unused"] = json!({"if": {"type": "string"}});
        let _ = check(&all, "Thing", &json!({"name": "a", "kind": "file"}));
    }

    /// The other keywords `schema_for_all` emits: a nullable field (`anyOf` with `null`), no
    /// field but the named ones, `not`, bounds and a length.
    #[test]
    fn the_checker_reads_every_keyword_it_knows() {
        let all = json!({
            "Page": {
                "type": "object",
                "properties": {
                    "next": {"anyOf": [{"$ref": "#/$defs/Cursor"}, {"type": "null"}]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 32},
                    "name": {"type": ["string", "null"], "maxLength": 3},
                    "tags": {"type": "array", "items": {"type": "string"}},
                },
                "additionalProperties": false,
                "not": {"required": ["error"]},
                "$defs": {"Cursor": {"type": "string", "pattern": "^[0-9]{1,20}:[0-9a-f]{8}$"}},
            },
        });
        let good = json!({"next": "12:0123abcd", "limit": 32, "name": null, "tags": ["a"]});
        assert_eq!(check(&all, "Page", &good), Ok(()));
        let bad = json!({
            "next": "12:XYZ", "limit": 33, "name": "abcd", "tags": [1], "error": {},
        });
        let errors = check(&all, "Page", &bad).unwrap_err();

        // In the keywords' order, which is their names'; the root's pointer is empty.
        assert_eq!(
            errors,
            [
                "/error: allowed by no schema",
                ": matches the schema of not",
                "/limit: 33 is more than 32",
                "/name: 4 characters, more than 3",
                "/next: matches none of the 2 schemas of anyOf",
                "/tags/0: 1 is not of type string",
            ]
        );
    }

    /// The two patterns `schema_for_all` holds, a git remote's name and a sync cursor.
    #[test]
    fn the_checker_reads_its_patterns() {
        let remote = "^[a-z0-9-]+/[A-Za-z0-9_-][A-Za-z0-9._-]*$";
        assert!(matches(remote, "github-com/acme.repo_1"));
        assert!(!matches(remote, "github-com/.acme"));
        assert!(!matches(remote, "GitHub/acme"));
        assert!(!matches(remote, "github/acme/x"));
        let cursor = "^[0-9]{1,20}:[0-9a-f]{8}$";
        assert!(matches(cursor, "0:deadbeef"));
        assert!(!matches(cursor, "0:deadbee"));
        assert!(!matches(cursor, "123456789012345678901:deadbeef"));
        assert!(matches("b+c?$", "abbb"), "unanchored at its start");
    }

    /// A pattern construct the checker does not know is a panic, never a pass.
    #[test]
    #[should_panic(expected = "unknown escape \\d")]
    fn an_unknown_pattern_construct_panics() {
        matches("^\\d+$", "1");
    }
}
