use super::{Diagnostic, Location, PathSegment, redacted_path};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::mem::MaybeUninit;
use std::{slice, str};

pub const MAX_BYTES: usize = 256 * 1024;
const MAX_NODES: usize = 8_192;
const MAX_DEPTH: usize = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputFormat {
    Yaml,
    Json,
}

#[derive(Debug)]
pub(super) struct Parsed {
    pub(super) value: Value,
    positions: Positions,
}

#[derive(Debug)]
struct Positions {
    location: Location,
    children: Children,
}

#[derive(Debug)]
enum Children {
    None,
    Object(BTreeMap<String, Positions>),
    Array(Vec<Positions>),
}

impl Parsed {
    fn new(value: Value, location: Location, children: Children) -> Self {
        Self {
            value,
            positions: Positions { location, children },
        }
    }

    pub(super) fn diagnostic_location(&self, pointer: &str) -> (String, Location) {
        let mut node = Some(&self.positions);
        let mut location = self.positions.location;
        let mut path = Vec::new();
        for segment in pointer.split('/').skip(1) {
            let key = segment.replace("~1", "/").replace("~0", "~");
            let (next, child) = match node.map(|node| &node.children) {
                Some(Children::Array(items)) => {
                    let item = key
                        .parse::<usize>()
                        .ok()
                        .filter(|index| index.to_string() == key)
                        .and_then(|index| items.get(index).map(|item| (index, item)));
                    match item {
                        Some((index, item)) => (PathSegment::Index(index), Some(item)),
                        None => (PathSegment::Key(key), None),
                    }
                }
                Some(Children::Object(fields)) => {
                    let child = fields.get(&key);
                    (PathSegment::Key(key), child)
                }
                _ => (PathSegment::Key(key), None),
            };
            path.push(next);
            node = child;
            if let Some(child) = child {
                location = child.location;
            }
        }
        (redacted_path(&path), location)
    }
}

fn start() -> Location {
    Location { line: 1, column: 1 }
}

pub(super) fn parse(bytes: &[u8], format: InputFormat) -> Result<Parsed, Diagnostic> {
    let state = State::default();
    if bytes.len() > MAX_BYTES {
        return Err(state.error(
            "INPUT_TOO_LARGE",
            format!("Strategy input exceeds the {MAX_BYTES}-byte limit"),
            start(),
        ));
    }
    let source = str::from_utf8(bytes).map_err(|error| {
        let prefix = str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix");
        let mut cursor = TextPosition::new();
        cursor.advance(prefix);
        state.error(
            "INVALID_UTF8",
            "Strategy input must be UTF-8",
            cursor.location,
        )
    })?;
    match format {
        InputFormat::Yaml => YamlParser::new(source)?.parse(),
        InputFormat::Json => JsonParser::new(source).parse(),
    }
}

#[derive(Default)]
struct State {
    path: Vec<PathSegment>,
    nodes: usize,
}

impl State {
    fn error(
        &self,
        code: &'static str,
        message: impl Into<String>,
        location: Location,
    ) -> Diagnostic {
        Diagnostic {
            path: redacted_path(&self.path),
            code,
            message: message.into(),
            location,
        }
    }

    fn count_node(&mut self, location: Location) -> Result<(), Diagnostic> {
        if self.path.len() >= MAX_DEPTH {
            return Err(self.error(
                "MAX_DEPTH",
                format!("Strategy input exceeds the {MAX_DEPTH}-level nesting limit"),
                location,
            ));
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(self.error(
                "TOO_MANY_NODES",
                format!("Strategy input exceeds the {MAX_NODES}-node limit"),
                location,
            ));
        }
        Ok(())
    }

    fn check_key(
        &mut self,
        key: &str,
        location: Location,
        map: &Map<String, Value>,
    ) -> Result<(), Diagnostic> {
        self.count_node(location)?;
        if matches!(
            key,
            "$ref" | "$dynamicRef" | "$recursiveRef" | "$include" | "<<"
        ) {
            return Err(self.error(
                "EXTERNAL_REFERENCE",
                "Strategy input cannot contain reference directives or YAML merges",
                location,
            ));
        }
        if map.contains_key(key) {
            return Err(self.error(
                "DUPLICATE_KEY",
                "Mapping key appears more than once",
                location,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct TextPosition {
    location: Location,
    after_cr: bool,
}

impl TextPosition {
    fn new() -> Self {
        Self {
            location: start(),
            after_cr: false,
        }
    }

    fn advance(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' if self.after_cr => {}
                '\r' | '\n' => {
                    self.location.line += 1;
                    self.location.column = 1;
                }
                _ => self.location.column += 1,
            }
            self.after_cr = ch == '\r';
        }
    }
}

struct JsonParser<'a> {
    state: State,
    rest: &'a str,
    position: TextPosition,
}

impl<'a> JsonParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            state: State::default(),
            rest: source,
            position: TextPosition::new(),
        }
    }

    fn parse(mut self) -> Result<Parsed, Diagnostic> {
        let parsed = self.node()?;
        self.skip_whitespace();
        if !self.rest.is_empty() {
            return Err(self.syntax("Unexpected data after the single JSON document"));
        }
        Ok(parsed)
    }

    fn syntax(&self, message: impl Into<String>) -> Diagnostic {
        self.state
            .error("JSON_SYNTAX", message, self.position.location)
    }

    fn advance(&mut self, bytes: usize) {
        let (consumed, rest) = self.rest.split_at(bytes);
        self.position.advance(consumed);
        self.rest = rest;
    }

    fn skip_whitespace(&mut self) {
        let stripped = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
        self.advance(self.rest.len() - stripped.len());
    }

    fn eat(&mut self, token: u8) -> bool {
        self.skip_whitespace();
        if self.rest.as_bytes().first() == Some(&token) {
            self.advance(1);
            return true;
        }
        false
    }

    fn need(&mut self, token: u8) -> Result<(), Diagnostic> {
        if !self.eat(token) {
            return Err(self.syntax(format!("Expected JSON delimiter '{}'", char::from(token))));
        }
        Ok(())
    }

    fn node(&mut self) -> Result<Parsed, Diagnostic> {
        self.skip_whitespace();
        let location = self.position.location;
        self.state.count_node(location)?;
        match self.rest.as_bytes().first() {
            Some(b'{') => self.object(location),
            Some(b'[') => self.array(location),
            Some(b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n') => {
                let value = self.scalar()?;
                Ok(Parsed::new(value, location, Children::None))
            }
            _ => Err(self.syntax("Expected a JSON value")),
        }
    }

    fn object(&mut self, location: Location) -> Result<Parsed, Diagnostic> {
        let mut values = Map::new();
        let mut positions = BTreeMap::new();
        self.advance(1);
        if !self.eat(b'}') {
            loop {
                self.skip_whitespace();
                let key_location = self.position.location;
                if !self.rest.starts_with('"') {
                    return Err(self.syntax("Expected a double-quoted JSON mapping key"));
                }
                let Value::String(key) = self.scalar()? else {
                    unreachable!("double-quoted JSON scalar is a string")
                };
                self.state.path.push(PathSegment::Key(key.clone()));
                self.state.check_key(&key, key_location, &values)?;
                self.need(b':')?;
                let child = self.node()?;
                self.state.path.pop();
                values.insert(key.clone(), child.value);
                positions.insert(key, child.positions);
                if self.eat(b'}') {
                    break;
                }
                self.need(b',')?;
            }
        }
        Ok(Parsed::new(
            Value::Object(values),
            location,
            Children::Object(positions),
        ))
    }

    fn array(&mut self, location: Location) -> Result<Parsed, Diagnostic> {
        let mut values = Vec::new();
        let mut positions = Vec::new();
        self.advance(1);
        if !self.eat(b']') {
            loop {
                self.state.path.push(PathSegment::Index(values.len()));
                let child = self.node()?;
                self.state.path.pop();
                values.push(child.value);
                positions.push(child.positions);
                if self.eat(b']') {
                    break;
                }
                self.need(b',')?;
            }
        }
        Ok(Parsed::new(
            Value::Array(values),
            location,
            Children::Array(positions),
        ))
    }

    fn scalar(&mut self) -> Result<Value, Diagnostic> {
        // Let serde_json decode the scalar syntax, including JSON Unicode escapes. The
        // container traversal retains source locations and sees keys before they are inserted.
        let mut stream = serde_json::Deserializer::from_str(self.rest).into_iter::<Value>();
        let value = stream
            .next()
            .ok_or_else(|| self.syntax("Expected a JSON scalar"))?;
        match value {
            Ok(value) => {
                self.advance(stream.byte_offset());
                Ok(match value {
                    Value::Number(number) => Value::Number(integral_form(number)),
                    value => value,
                })
            }
            Err(error) => {
                let mut position = self.position;
                let offset = self
                    .rest
                    .split_inclusive('\n')
                    .take(error.line().saturating_sub(1))
                    .map(str::len)
                    .sum::<usize>();
                let mut end = (offset + error.column().saturating_sub(1)).min(self.rest.len());
                while !self.rest.is_char_boundary(end) {
                    end -= 1;
                }
                position.advance(&self.rest[..end]);
                let message = if error.is_eof() {
                    "Unexpected end of input while decoding a JSON scalar"
                } else {
                    "Invalid JSON scalar"
                };
                Err(self.state.error("JSON_SYNTAX", message, position.location))
            }
        }
    }
}

struct YamlParser<'a> {
    state: State,
    events: YamlEvents<'a>,
}

impl<'a> YamlParser<'a> {
    fn new(source: &'a str) -> Result<Self, Diagnostic> {
        let state = State::default();
        let events = YamlEvents::new(source, &state)?;
        Ok(Self { state, events })
    }

    fn parse(mut self) -> Result<Parsed, Diagnostic> {
        self.expect_event(YamlKind::StreamStart)?;
        let next = self.events.next(&self.state)?;
        match next.kind {
            YamlKind::DocumentStart {
                has_tag_directive: true,
            } => {
                return Err(self.state.error(
                    "YAML_TAG",
                    "YAML tag directives are not supported",
                    next.location,
                ));
            }
            YamlKind::DocumentStart {
                has_tag_directive: false,
            } => {}
            _ => {
                return Err(self.state.error(
                    "YAML_SYNTAX",
                    "Expected one YAML document",
                    next.location,
                ));
            }
        }
        let root = self.events.next(&self.state)?;
        let parsed = self.node(root)?;
        self.expect_event(YamlKind::DocumentEnd)?;
        let next = self.events.next(&self.state)?;
        match next.kind {
            YamlKind::StreamEnd => Ok(parsed),
            YamlKind::DocumentStart { .. } => Err(self.state.error(
                "MULTIPLE_DOCUMENTS",
                "Strategy input must contain exactly one YAML document",
                next.location,
            )),
            _ => Err(self.state.error(
                "YAML_SYNTAX",
                "Expected the end of the YAML stream",
                next.location,
            )),
        }
    }

    fn expect_event(&mut self, kind: YamlKind) -> Result<(), Diagnostic> {
        let actual = self.events.next(&self.state)?;
        if actual.kind != kind {
            return Err(self.state.error(
                "YAML_SYNTAX",
                "Unexpected YAML document boundary",
                actual.location,
            ));
        }
        Ok(())
    }

    fn node(&mut self, event: YamlEvent) -> Result<Parsed, Diagnostic> {
        self.state.count_node(event.location)?;
        event.check_properties(&self.state)?;
        let location = event.location;
        match event.kind {
            YamlKind::MappingStart => self.object(location),
            YamlKind::SequenceStart => self.array(location),
            YamlKind::Scalar { text, plain } => {
                let value = yaml_scalar(text, plain)
                    .map_err(|message| self.state.error("YAML_SYNTAX", message, location))?;
                Ok(Parsed::new(value, location, Children::None))
            }
            _ => Err(self
                .state
                .error("YAML_SYNTAX", "Expected a YAML value", location)),
        }
    }

    fn object(&mut self, location: Location) -> Result<Parsed, Diagnostic> {
        let mut values = Map::new();
        let mut positions = BTreeMap::new();
        loop {
            let key_event = self.events.next(&self.state)?;
            if key_event.kind == YamlKind::MappingEnd {
                break;
            }
            let YamlKind::Scalar { text, plain } = &key_event.kind else {
                key_event.check_properties(&self.state)?;
                return Err(self.state.error(
                    "NON_STRING_KEY",
                    "Mapping keys must be strings",
                    key_event.location,
                ));
            };
            self.state.path.push(PathSegment::Key(text.clone()));
            key_event.check_properties(&self.state)?;
            let key = match yaml_scalar(text.clone(), *plain) {
                Ok(Value::String(value)) => value,
                _ => {
                    return Err(self.state.error(
                        "NON_STRING_KEY",
                        "Mapping keys must be strings",
                        key_event.location,
                    ));
                }
            };
            self.state.check_key(&key, key_event.location, &values)?;
            let value_event = self.events.next(&self.state)?;
            let child = self.node(value_event)?;
            self.state.path.pop();
            values.insert(key.clone(), child.value);
            positions.insert(key, child.positions);
        }
        Ok(Parsed::new(
            Value::Object(values),
            location,
            Children::Object(positions),
        ))
    }

    fn array(&mut self, location: Location) -> Result<Parsed, Diagnostic> {
        let mut values = Vec::new();
        let mut positions = Vec::new();
        loop {
            self.state.path.push(PathSegment::Index(values.len()));
            let event = self.events.next(&self.state)?;
            if event.kind == YamlKind::SequenceEnd {
                self.state.path.pop();
                break;
            }
            let child = self.node(event)?;
            self.state.path.pop();
            values.push(child.value);
            positions.push(child.positions);
        }
        Ok(Parsed::new(
            Value::Array(values),
            location,
            Children::Array(positions),
        ))
    }
}

#[derive(PartialEq)]
enum YamlKind {
    StreamStart,
    StreamEnd,
    DocumentStart { has_tag_directive: bool },
    DocumentEnd,
    Alias,
    Scalar { text: String, plain: bool },
    SequenceStart,
    SequenceEnd,
    MappingStart,
    MappingEnd,
    Unknown,
}

struct YamlEvent {
    kind: YamlKind,
    location: Location,
    has_tag: bool,
    has_anchor: bool,
}

impl YamlEvent {
    fn check_properties(&self, state: &State) -> Result<(), Diagnostic> {
        if self.kind == YamlKind::Alias {
            return Err(state.error(
                "YAML_ALIAS",
                "YAML aliases are not supported",
                self.location,
            ));
        }
        if self.has_anchor {
            return Err(state.error(
                "YAML_ANCHOR",
                "YAML anchors are not supported",
                self.location,
            ));
        }
        if self.has_tag {
            return Err(state.error(
                "YAML_TAG",
                "Explicit YAML tags are not supported",
                self.location,
            ));
        }
        Ok(())
    }
}

struct YamlEvents<'a> {
    // libyaml stores an internal pointer back to the parser when the string input is set.
    parser: Box<MaybeUninit<unsafe_libyaml::yaml_parser_t>>,
    source: &'a str,
}

impl<'a> YamlEvents<'a> {
    fn new(source: &'a str, state: &State) -> Result<Self, Diagnostic> {
        let mut parser = Box::new(MaybeUninit::uninit());
        let ptr = parser.as_mut_ptr();
        // SAFETY: libyaml initializes the parser in its stable allocation, and the input
        // remains borrowed for the lifetime of that parser. Failed initialization owns nothing.
        unsafe {
            if unsafe_libyaml::yaml_parser_initialize(ptr).fail {
                return Err(state.error(
                    "YAML_SYNTAX",
                    "Could not initialize the YAML parser",
                    start(),
                ));
            }
            unsafe_libyaml::yaml_parser_set_encoding(ptr, unsafe_libyaml::YAML_UTF8_ENCODING);
            unsafe_libyaml::yaml_parser_set_input_string(ptr, source.as_ptr(), source.len() as u64);
        }
        Ok(Self { parser, source })
    }

    fn next(&mut self, state: &State) -> Result<YamlEvent, Diagnostic> {
        let parser = self.parser.as_mut_ptr();
        let mut event = MaybeUninit::<unsafe_libyaml::yaml_event_t>::uninit();
        // SAFETY: the parser is live and exclusively borrowed. A successful parse initializes
        // the event. Its contents are copied before releasing the event's allocation.
        unsafe {
            if unsafe_libyaml::yaml_parser_parse(parser, event.as_mut_ptr()).fail {
                return Err(self.error(state));
            }
            let mut event = event.assume_init();
            let converted = Self::convert(&event);
            unsafe_libyaml::yaml_event_delete(&mut event);
            Ok(converted)
        }
    }

    fn error(&self, state: &State) -> Diagnostic {
        // SAFETY: the parser is live and initialized.
        let parser = unsafe { self.parser.assume_init_ref() };
        let message = match parser.error {
            unsafe_libyaml::YAML_READER_ERROR => "Invalid character in YAML input",
            unsafe_libyaml::YAML_SCANNER_ERROR => "Invalid YAML token",
            _ => "Malformed YAML document",
        };
        let location = if parser.error == unsafe_libyaml::YAML_READER_ERROR {
            let mut position = TextPosition::new();
            let end = self
                .source
                .floor_char_boundary((parser.problem_offset as usize).min(self.source.len()));
            position.advance(&self.source[..end]);
            position.location
        } else {
            mark_location(parser.problem_mark)
        };
        state.error("YAML_SYNTAX", message, location)
    }

    fn convert(event: &unsafe_libyaml::yaml_event_t) -> YamlEvent {
        let location = mark_location(event.start_mark);
        // SAFETY: libyaml initializes the union variant indicated by the event type. Scalar
        // text is valid UTF-8 and is copied while the owning event remains live.
        let (kind, has_tag, has_anchor) = unsafe {
            match event.type_ {
                unsafe_libyaml::YAML_SCALAR_EVENT => {
                    let data = event.data.scalar;
                    let bytes = slice::from_raw_parts(data.value, data.length as usize);
                    let text = str::from_utf8(bytes)
                        .expect("libyaml scalar is UTF-8")
                        .to_owned();
                    let plain = data.style == unsafe_libyaml::YAML_PLAIN_SCALAR_STYLE;
                    (
                        YamlKind::Scalar { text, plain },
                        !data.tag.is_null(),
                        !data.anchor.is_null(),
                    )
                }
                unsafe_libyaml::YAML_DOCUMENT_START_EVENT => {
                    let directives = event.data.document_start.tag_directives;
                    let has_tag_directive = directives.start != directives.end;
                    (YamlKind::DocumentStart { has_tag_directive }, false, false)
                }
                unsafe_libyaml::YAML_SEQUENCE_START_EVENT => {
                    let data = event.data.sequence_start;
                    (
                        YamlKind::SequenceStart,
                        !data.tag.is_null(),
                        !data.anchor.is_null(),
                    )
                }
                unsafe_libyaml::YAML_MAPPING_START_EVENT => {
                    let data = event.data.mapping_start;
                    (
                        YamlKind::MappingStart,
                        !data.tag.is_null(),
                        !data.anchor.is_null(),
                    )
                }
                kind => (
                    match kind {
                        unsafe_libyaml::YAML_STREAM_START_EVENT => YamlKind::StreamStart,
                        unsafe_libyaml::YAML_STREAM_END_EVENT => YamlKind::StreamEnd,
                        unsafe_libyaml::YAML_DOCUMENT_END_EVENT => YamlKind::DocumentEnd,
                        unsafe_libyaml::YAML_ALIAS_EVENT => YamlKind::Alias,
                        unsafe_libyaml::YAML_SEQUENCE_END_EVENT => YamlKind::SequenceEnd,
                        unsafe_libyaml::YAML_MAPPING_END_EVENT => YamlKind::MappingEnd,
                        _ => YamlKind::Unknown,
                    },
                    false,
                    false,
                ),
            }
        };
        YamlEvent {
            kind,
            location,
            has_tag,
            has_anchor,
        }
    }
}

impl Drop for YamlEvents<'_> {
    fn drop(&mut self) {
        // SAFETY: only successfully initialized parsers are owned by this type.
        unsafe { unsafe_libyaml::yaml_parser_delete(self.parser.as_mut_ptr()) };
    }
}

fn mark_location(mark: unsafe_libyaml::yaml_mark_t) -> Location {
    Location {
        line: mark.line as usize + 1,
        column: mark.column as usize + 1,
    }
}

fn yaml_scalar(text: String, plain: bool) -> Result<Value, &'static str> {
    if !plain {
        return Ok(Value::String(text));
    }
    match text.as_str() {
        "" | "null" | "Null" | "NULL" | "~" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(&text);
    if matches!(
        unsigned,
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF"
    ) {
        return Err("Non-finite YAML numbers are not supported");
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = unsigned.strip_prefix(prefix)
            && !digits.is_empty()
            && digits.chars().all(|ch| ch.is_digit(radix))
        {
            let value = u64::from_str_radix(digits, radix)
                .ok()
                .and_then(|number| form_integer(number, text.starts_with('-')))
                .ok_or("YAML integer is outside the supported range")?;
            return Ok(Value::Number(value));
        }
    }
    if let Some(number) = yaml_decimal(&text) {
        let number = number
            .parse::<Number>()
            .map_err(|_| "Invalid YAML number")?;
        return Ok(Value::Number(integral_form(number)));
    }
    Ok(Value::String(text))
}

fn yaml_decimal(text: &str) -> Option<String> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, None), |(m, e)| (m, Some(e)));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let whole = whole.trim_start_matches('0');
    let mut numeric = String::new();
    if text.starts_with('-') {
        numeric.push('-');
    }
    numeric.push_str(if whole.is_empty() { "0" } else { whole });
    if !fraction.is_empty() {
        numeric.push('.');
        numeric.push_str(fraction);
    }
    if let Some(exponent) = exponent {
        numeric.push('e');
        numeric.push_str(exponent);
    }
    Some(numeric)
}

fn form_integer(value: u64, negative: bool) -> Option<Number> {
    if negative {
        i64::try_from(-i128::from(value)).ok().map(Number::from)
    } else {
        Some(Number::from(value))
    }
}

// JSON Schema's integer type accepts mathematically integral numbers, including exponent
// notation. Normalize exactly within the integer range, without a binary float conversion.
fn integral_form(number: Number) -> Number {
    fn exact_integer(text: &str) -> Option<Number> {
        let negative = text.starts_with('-');
        let unsigned = text.strip_prefix('-').unwrap_or(text);
        let (mantissa, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = format!("{whole}{fraction}");
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Some(Number::from(0));
        }
        let shift = i64::from(exponent.parse::<i32>().ok()?) - fraction.len() as i64;
        let value = if shift < 0 {
            let split = digits.len().checked_sub((-shift) as usize)?;
            let (integral, discarded) = digits.split_at(split);
            if !discarded.bytes().all(|b| b == b'0') {
                return None;
            }
            integral.parse::<u64>().ok()?
        } else {
            let multiplier = 10u64.checked_pow(shift as u32)?;
            digits.parse::<u64>().ok()?.checked_mul(multiplier)?
        };
        form_integer(value, negative)
    }

    if number.is_u64() || number.is_i64() {
        return number;
    }
    exact_integer(number.as_str()).unwrap_or(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_and_json_share_exact_value_types() {
        let yaml = parse(
            br#"a: [01, 1E0, +001.10e1, .5, "0.25", 1d, True, ~, 0x10, -0.0]"#,
            InputFormat::Yaml,
        )
        .unwrap();
        let json = parse(
            br#"{"a": [1, 1.0, 1.10e1, 0.5, "0.25", "1d", true, null, 16, 0]}"#,
            InputFormat::Json,
        )
        .unwrap();
        assert_eq!(yaml.value, json.value);
        let items = json.value["a"].as_array().unwrap();
        assert_eq!(items[1].as_u64(), Some(1));
        assert_eq!(items[2].as_u64(), Some(11));
        assert_eq!(items[3].as_u64(), None);
        let precise = parse(b"1.00000000000000000000000000000001", InputFormat::Json).unwrap();
        assert_eq!(precise.value.as_u64(), None);

        let yaml = parse(b"s: [yes, off, 2026-11-26]", InputFormat::Yaml).unwrap();
        assert_eq!(yaml.value, json!({"s": ["yes", "off", "2026-11-26"]}));
        let error = parse(b"s: .inf", InputFormat::Yaml).unwrap_err();
        assert_eq!(error.code, "YAML_SYNTAX");
        assert_eq!(error.path, "/<key>");
        assert_eq!(error.message, "Non-finite YAML numbers are not supported");
    }

    #[test]
    fn duplicate_keys_report_the_second_key_after_unescaping() {
        for (source, format) in [
            (
                "nested:\n  'a~/b': first\n  \"a~/b\": second\n",
                InputFormat::Yaml,
            ),
            (
                "{\"nested\": {\n  \"a~/b\": 1,\n  \"a~\\u002fb\": 2\n}}",
                InputFormat::Json,
            ),
        ] {
            let error = parse(source.as_bytes(), format).unwrap_err();
            assert_eq!(error.code, "DUPLICATE_KEY");
            assert_eq!(error.path, "/<key>/<key>");
            assert_eq!(error.location, Location { line: 3, column: 3 });
        }
    }

    #[test]
    fn source_positions_follow_values_and_fall_back_to_their_parent() {
        let parsed = parse(
            "描述:\n  - 'a'\n  - {\"a/b\": true}\n".as_bytes(),
            InputFormat::Yaml,
        )
        .unwrap();
        assert_eq!(
            parsed.diagnostic_location("/描述/0"),
            ("/<key>/0".to_owned(), Location { line: 2, column: 5 })
        );
        assert_eq!(
            parsed.diagnostic_location("/描述/1/a~1b"),
            (
                "/<key>/1/<key>".to_owned(),
                Location {
                    line: 3,
                    column: 13
                }
            )
        );
        assert_eq!(
            parsed.diagnostic_location("/描述/1/a~1b/missing"),
            (
                "/<key>/1/<key>/<key>".to_owned(),
                parsed.diagnostic_location("/描述/1/a~1b").1
            )
        );
        let parsed = parse(br#"{"a":"\uD83D\uDE00"}"#, InputFormat::Json).unwrap();
        assert_eq!(parsed.value, json!({"a": "😀"}));
        assert_eq!(
            parsed.diagnostic_location("/a"),
            ("/<key>".to_owned(), Location { line: 1, column: 6 })
        );
    }

    #[test]
    fn numeric_object_keys_are_never_rendered_as_array_indices() {
        let pointer = "/spec/portfolio/sleeves/987654321";
        let parsed = parse(
            b"spec:\n  portfolio:\n    sleeves:\n      \"987654321\": hidden\n",
            InputFormat::Yaml,
        )
        .unwrap();
        assert_eq!(
            parsed.diagnostic_location(pointer),
            (
                "/spec/portfolio/sleeves/<key>".to_owned(),
                Location {
                    line: 4,
                    column: 20
                }
            )
        );
        let parsed = parse(
            b"spec:\n  portfolio:\n    sleeves: [hidden]\n",
            InputFormat::Yaml,
        )
        .unwrap();
        assert_eq!(
            parsed.diagnostic_location(pointer),
            (
                "/spec/portfolio/sleeves/<key>".to_owned(),
                Location {
                    line: 3,
                    column: 14
                }
            )
        );
    }

    #[test]
    fn yaml_rejects_graphs_tags_reference_directives_and_extra_documents() {
        for (input, expected_code, expected_path) in [
            ("v: *link", "YAML_ALIAS", "/<key>"),
            ("v: [&link 1]", "YAML_ANCHOR", "/<key>/0"),
            ("v: !loader [1]", "YAML_TAG", "/<key>"),
            ("v: !!str 1", "YAML_TAG", "/<key>"),
            ("v: ! 1", "YAML_TAG", "/<key>"),
            (
                "%TAG !doc! tag:example.org,2026:\n---\nv: 1\n",
                "YAML_TAG",
                "",
            ),
            (
                "v:\n  $ref: file:///tmp/input\n",
                "EXTERNAL_REFERENCE",
                "/<key>/<key>",
            ),
            (
                "v:\n  <<: {external: data}\n",
                "EXTERNAL_REFERENCE",
                "/<key>/<key>",
            ),
            ("v: 1\n---\nv: 2\n", "MULTIPLE_DOCUMENTS", ""),
            ("v: 1\n...\n---\n", "MULTIPLE_DOCUMENTS", ""),
            ("? [abc]\n: false\n", "NON_STRING_KEY", ""),
            ("v:\n  true: value\n", "NON_STRING_KEY", "/<key>/<key>"),
        ] {
            let error = parse(input.as_bytes(), InputFormat::Yaml).unwrap_err();
            assert_eq!(error.code, expected_code, "{input}");
            assert_eq!(error.path, expected_path, "{input}");
            assert!(
                error.location.line > 0 && error.location.column > 0,
                "{input}"
            );
        }
    }

    #[test]
    fn json_rejects_yaml_syntax_trailing_data_and_reference_directives() {
        for input in [
            "{'a': 'foo'}",
            "{a: 42}",
            "[1,]",
            "{\"a\": 1,}",
            "{\"a\": +1}",
            "{\"a\": 01}",
            "{\"a\": NaN}",
            "{\"a\": 1 // comment\n}",
            "{\"a\": 1} {}",
        ] {
            let error = parse(input.as_bytes(), InputFormat::Json).unwrap_err();
            assert_eq!(error.code, "JSON_SYNTAX", "{input}");
        }
        let error = parse(
            br#"{"nested":{"$dynamicRef":"https://example.org/schema"}}"#,
            InputFormat::Json,
        )
        .unwrap_err();
        assert_eq!(error.code, "EXTERNAL_REFERENCE");
        assert_eq!(error.path, "/<key>/<key>");
        let error = parse(b"{\n  \"nested\": [\"\\q\"]\n}", InputFormat::Json).unwrap_err();
        assert_eq!(error.code, "JSON_SYNTAX");
        assert_eq!(error.path, "/<key>/0");
        assert_eq!(
            error.location,
            Location {
                line: 2,
                column: 16
            }
        );
        assert_eq!(error.message, "Invalid JSON scalar");
    }

    #[test]
    fn malformed_input_diagnostics_keep_locations_without_payload() {
        let marker = "fixture-raw-payload";
        let yaml = format!(r#""{marker}\q""#);
        let error = parse(yaml.as_bytes(), InputFormat::Yaml).unwrap_err();
        assert_eq!(error.code, "YAML_SYNTAX");
        assert_eq!(error.path, "");
        assert_eq!(error.message, "Invalid YAML token");
        assert_eq!(
            error.location,
            Location {
                line: 1,
                column: marker.len() + 2
            }
        );
        assert!(!serde_json::to_string(&error).unwrap().contains(marker));

        let json = format!("{{\n  \"{marker}\": [\"{marker}\\q\"]\n}}");
        let error = parse(json.as_bytes(), InputFormat::Json).unwrap_err();
        assert_eq!(error.code, "JSON_SYNTAX");
        assert_eq!(error.path, "/<key>/0");
        assert_eq!(error.message, "Invalid JSON scalar");
        assert_eq!(
            error.location,
            Location {
                line: 2,
                column: 2 * marker.len() + 10
            }
        );
        assert!(!serde_json::to_string(&error).unwrap().contains(marker));

        let mut invalid_encoding = format!(r#"{{"{marker}":""#).into_bytes();
        let column = invalid_encoding.len() + 1;
        invalid_encoding.push(0xff);
        let error = parse(&invalid_encoding, InputFormat::Json).unwrap_err();
        assert_eq!(error.code, "INVALID_UTF8");
        assert_eq!(error.path, "");
        assert_eq!(error.message, "Strategy input must be UTF-8");
        assert_eq!(error.location, Location { line: 1, column });
        assert!(!serde_json::to_string(&error).unwrap().contains(marker));
    }

    #[test]
    fn byte_node_and_depth_limits_apply_to_both_formats() {
        for format in [InputFormat::Yaml, InputFormat::Json] {
            let mut at_byte_limit = vec![b' '; MAX_BYTES];
            at_byte_limit[..2].copy_from_slice(b"[]");
            parse(&at_byte_limit, format).unwrap();
            let error = parse(&vec![b' '; MAX_BYTES + 1], format).unwrap_err();
            assert_eq!(error.code, "INPUT_TOO_LARGE");

            let at_node_limit = format!("[{}0]", "0,".repeat(MAX_NODES - 2));
            parse(at_node_limit.as_bytes(), format).unwrap();
            let over_nodes = format!("[{}0]", "0,".repeat(MAX_NODES - 1));
            let error = parse(over_nodes.as_bytes(), format).unwrap_err();
            assert_eq!(error.code, "TOO_MANY_NODES");
            assert_eq!(error.path, format!("/{}", MAX_NODES - 1));

            let at_depth_limit = format!(
                "{}0{}",
                "[".repeat(MAX_DEPTH - 1),
                "]".repeat(MAX_DEPTH - 1)
            );
            parse(at_depth_limit.as_bytes(), format).unwrap();
            let over_depth = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
            let error = parse(over_depth.as_bytes(), format).unwrap_err();
            assert_eq!(error.code, "MAX_DEPTH");
            assert_eq!(error.path, "/0".repeat(MAX_DEPTH));
        }
    }

    #[test]
    fn invalid_utf8_reports_the_first_invalid_byte() {
        for format in [InputFormat::Yaml, InputFormat::Json] {
            let error = parse(b"{}\r\n \xff", format).unwrap_err();
            assert_eq!(error.code, "INVALID_UTF8");
            assert_eq!(error.location, Location { line: 2, column: 2 });
        }
    }
}
