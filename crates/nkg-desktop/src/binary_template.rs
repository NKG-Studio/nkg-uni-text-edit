use nkg_text_engine::TextDocument;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

const READ_CACHE_BYTES: usize = 64 * 1024;
pub const MAX_TEMPLATE_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_TEMPLATE_TOKENS: usize = 500_000;
const MAX_RESULT_NODES: usize = 200_000;
const MAX_ARRAY_ELEMENTS: u64 = 100_000;
const MAX_TYPE_DEPTH: usize = 64;
const MAX_VALUE_PREVIEW_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

impl Endianness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Little => "小端",
            Self::Big => "大端",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BinaryNode {
    pub name: String,
    pub type_name: String,
    pub byte_start: u64,
    pub byte_size: u64,
    pub value: Option<String>,
    pub children: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct BinaryParseResult {
    pub nodes: Vec<BinaryNode>,
    pub roots: Vec<usize>,
    pub endianness: Endianness,
    pub consumed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    Number(u64),
    Symbol(char),
    StringLiteral,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    line: usize,
}

#[derive(Debug, Clone, Copy)]
enum Primitive {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Char,
    Bool,
}

impl Primitive {
    fn size(self) -> u64 {
        match self {
            Self::U8 | Self::I8 | Self::Char | Self::Bool => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    fn is_character(self) -> bool {
        matches!(self, Self::Char)
    }
}

#[derive(Debug, Clone)]
enum TypeRef {
    Primitive(Primitive, String),
    Named(String),
}

impl TypeRef {
    fn display_name(&self) -> String {
        match self {
            Self::Primitive(_, name) | Self::Named(name) => name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
enum TypeDefinition {
    Struct(Vec<FieldDefinition>),
    Enum {
        underlying: TypeRef,
        variants: HashMap<u64, String>,
    },
    Alias(TypeRef),
}

#[derive(Debug, Clone)]
enum ArrayLength {
    Fixed(u64),
    Identifier(String),
}

#[derive(Debug, Clone)]
struct FieldDefinition {
    ty: TypeRef,
    name: String,
    array: Option<ArrayLength>,
}

#[derive(Debug, Clone)]
struct RootDefinition {
    field: FieldDefinition,
    offset: Option<u64>,
}

struct ParsedTemplate {
    endianness: Endianness,
    types: HashMap<String, TypeDefinition>,
    constants: HashMap<String, u64>,
    roots: Vec<RootDefinition>,
}

pub fn parse_binary_template(
    document: &TextDocument,
    source: &str,
    cancel: &AtomicBool,
) -> Result<BinaryParseResult, String> {
    if source.len() > MAX_TEMPLATE_SOURCE_BYTES {
        return Err(format!(
            "模板文件不能超过 {} 字节",
            MAX_TEMPLATE_SOURCE_BYTES
        ));
    }
    let (directive_endianness, constants) = scan_directives(source)?;
    let tokens = lex(source)?;
    if tokens.len() > MAX_TEMPLATE_TOKENS {
        return Err(format!(
            "模板过于复杂：最多允许 {MAX_TEMPLATE_TOKENS} 个词法单元"
        ));
    }
    let mut template = TemplateParser::new(tokens, directive_endianness, constants).parse()?;
    if template.roots.is_empty() {
        return Err("模板没有顶层实例；请在结构定义后添加类似 `Header header;` 的声明".into());
    }
    BinaryParser::new(document, &mut template, cancel).parse()
}

fn scan_directives(source: &str) -> Result<(Endianness, HashMap<String, u64>), String> {
    let mut endianness = Endianness::Little;
    let mut constants = HashMap::new();
    for (line_index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#pragma") {
            let words = rest.split_whitespace().collect::<Vec<_>>();
            if words.first().is_some_and(|word| *word == "endian") {
                endianness = match words.get(1).copied() {
                    Some("little") => Endianness::Little,
                    Some("big") => Endianness::Big,
                    _ => {
                        return Err(format!(
                            "模板第 {} 行：`#pragma endian` 只支持 little 或 big",
                            line_index + 1
                        ));
                    }
                };
            }
        } else if let Some(rest) = trimmed.strip_prefix("#define") {
            let mut words = rest.split_whitespace();
            if let (Some(name), Some(value)) = (words.next(), words.next()) {
                let value = parse_number_text(value).map_err(|message| {
                    format!("模板第 {} 行：#define {name} {message}", line_index + 1)
                })?;
                constants.insert(name.to_owned(), value);
            }
        }
    }
    Ok((endianness, constants))
}

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' => index += 1,
            b'\n' => {
                line += 1;
                index += 1;
            }
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let start_line = line;
                let mut closed = false;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'\n' {
                        line += 1;
                    }
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        index += 2;
                        closed = true;
                        break;
                    }
                    index += 1;
                }
                if !closed {
                    return Err(format!("模板第 {start_line} 行：块注释没有结束"));
                }
            }
            b'"' | b'\'' => {
                let quote = bytes[index];
                let start_line = line;
                index += 1;
                let mut escaped = false;
                let mut closed = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if byte == b'\n' {
                        line += 1;
                    }
                    if !escaped && byte == quote {
                        index += 1;
                        closed = true;
                        break;
                    }
                    escaped = !escaped && byte == b'\\';
                    if byte != b'\\' {
                        escaped = false;
                    }
                    index += 1;
                }
                if !closed {
                    return Err(format!("模板第 {start_line} 行：字符串没有结束"));
                }
                tokens.push(Token {
                    kind: TokenKind::StringLiteral,
                    line: start_line,
                });
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
                {
                    index += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Ident(source[start..index].to_owned()),
                    line,
                });
            }
            byte if byte.is_ascii_digit() => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_hexdigit()
                        || matches!(
                            bytes[index],
                            b'x' | b'X' | b'b' | b'B' | b'u' | b'U' | b'l' | b'L'
                        ))
                {
                    index += 1;
                }
                let raw = &source[start..index];
                let value = parse_number_text(raw)
                    .map_err(|message| format!("模板第 {line} 行：{raw} {message}"))?;
                tokens.push(Token {
                    kind: TokenKind::Number(value),
                    line,
                });
            }
            byte if b"{}[]();,:=@*<>+-".contains(&byte) => {
                tokens.push(Token {
                    kind: TokenKind::Symbol(byte as char),
                    line,
                });
                index += 1;
            }
            byte => {
                return Err(format!("模板第 {line} 行：不支持的字符 `{}`", byte as char));
            }
        }
    }
    Ok(tokens)
}

fn parse_number_text(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim_end_matches(['u', 'U', 'l', 'L']);
    let (radix, digits) = if let Some(value) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        (16, value)
    } else if let Some(value) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        (2, value)
    } else {
        (10, trimmed)
    };
    u64::from_str_radix(digits, radix).map_err(|_| "不是有效的无符号整数".into())
}

struct TemplateParser {
    tokens: Vec<Token>,
    position: usize,
    endianness: Endianness,
    types: HashMap<String, TypeDefinition>,
    constants: HashMap<String, u64>,
    roots: Vec<RootDefinition>,
}

impl TemplateParser {
    fn new(tokens: Vec<Token>, endianness: Endianness, constants: HashMap<String, u64>) -> Self {
        Self {
            tokens,
            position: 0,
            endianness,
            types: HashMap::new(),
            constants,
            roots: Vec::new(),
        }
    }

    fn parse(mut self) -> Result<ParsedTemplate, String> {
        while !self.at_end() {
            if self.consume_ident("LittleEndian") {
                self.expect_symbol('(')?;
                self.expect_symbol(')')?;
                self.expect_symbol(';')?;
                self.endianness = Endianness::Little;
            } else if self.consume_ident("BigEndian") {
                self.expect_symbol('(')?;
                self.expect_symbol(')')?;
                self.expect_symbol(';')?;
                self.endianness = Endianness::Big;
            } else if self.consume_ident("typedef") {
                self.parse_typedef()?;
            } else if self.consume_ident("using") {
                self.parse_using()?;
            } else if self.check_ident("struct") && self.looks_like_definition(1) {
                self.parse_struct_definition(false)?;
            } else if self.check_ident("enum") && self.looks_like_enum_definition() {
                self.parse_enum_definition(false)?;
            } else if self.consume_ident("const") || self.consume_ident("constexpr") {
                self.parse_constant()?;
            } else {
                let field = self.parse_field_definition()?;
                let offset = if self.consume_symbol('@') {
                    Some(self.expect_number("顶层实例偏移必须是整数")?)
                } else {
                    None
                };
                self.skip_attributes()?;
                self.expect_symbol(';')?;
                self.roots.push(RootDefinition { field, offset });
            }
        }
        Ok(ParsedTemplate {
            endianness: self.endianness,
            types: self.types,
            constants: self.constants,
            roots: self.roots,
        })
    }

    fn parse_typedef(&mut self) -> Result<(), String> {
        if self.check_ident("struct") && self.looks_like_definition(1) {
            return self.parse_struct_definition(true);
        }
        if self.check_ident("enum") && self.looks_like_enum_definition() {
            return self.parse_enum_definition(true);
        }
        let ty = self.parse_type()?;
        let alias = self.expect_ident("typedef 缺少别名")?;
        self.expect_symbol(';')?;
        self.types.insert(alias, TypeDefinition::Alias(ty));
        Ok(())
    }

    fn parse_using(&mut self) -> Result<(), String> {
        let alias = self.expect_ident("using 缺少别名")?;
        self.expect_symbol('=')?;
        let ty = self.parse_type()?;
        self.expect_symbol(';')?;
        self.types.insert(alias, TypeDefinition::Alias(ty));
        Ok(())
    }

    fn parse_constant(&mut self) -> Result<(), String> {
        let _ty = self.parse_type()?;
        let name = self.expect_ident("常量缺少名称")?;
        self.expect_symbol('=')?;
        let value = self.expect_number("常量值必须是无符号整数")?;
        self.expect_symbol(';')?;
        self.constants.insert(name, value);
        Ok(())
    }

    fn parse_struct_definition(&mut self, is_typedef: bool) -> Result<(), String> {
        self.expect_ident_value("struct")?;
        let tag = self.take_ident();
        self.expect_symbol('{')?;
        let mut fields = Vec::new();
        while !self.consume_symbol('}') {
            if self.at_end() {
                return self.error("结构定义没有结束");
            }
            fields.push(self.parse_field_definition()?);
            self.skip_attributes()?;
            self.expect_symbol(';')?;
        }
        let trailing = self.take_ident();
        self.skip_attributes()?;
        self.expect_symbol(';')?;

        if is_typedef {
            let type_name = trailing
                .clone()
                .or_else(|| tag.clone())
                .ok_or_else(|| self.message("匿名 typedef struct 缺少类型名"))?;
            self.types
                .insert(type_name.clone(), TypeDefinition::Struct(fields));
            if let Some(tag) = tag
                && tag != type_name
            {
                self.types.insert(
                    tag,
                    TypeDefinition::Alias(TypeRef::Named(type_name.clone())),
                );
            }
        } else {
            let type_name = tag.ok_or_else(|| self.message("struct 定义缺少类型名"))?;
            self.types
                .insert(type_name.clone(), TypeDefinition::Struct(fields));
            if let Some(instance) = trailing {
                self.roots.push(RootDefinition {
                    field: FieldDefinition {
                        ty: TypeRef::Named(type_name),
                        name: instance,
                        array: None,
                    },
                    offset: None,
                });
            }
        }
        Ok(())
    }

    fn parse_enum_definition(&mut self, is_typedef: bool) -> Result<(), String> {
        self.expect_ident_value("enum")?;
        self.consume_ident("class");
        let tag = self.take_ident();
        let underlying = if self.consume_symbol(':') {
            self.parse_type()?
        } else {
            TypeRef::Primitive(Primitive::U32, "uint32_t".into())
        };
        self.expect_symbol('{')?;
        let mut variants = HashMap::new();
        let mut next_value = 0_u64;
        while !self.consume_symbol('}') {
            let name = self.expect_ident("枚举项缺少名称")?;
            let value = if self.consume_symbol('=') {
                self.expect_number("枚举值必须是无符号整数")?
            } else {
                next_value
            };
            variants.insert(value, name);
            next_value = value.saturating_add(1);
            if !self.consume_symbol(',') && !self.check_symbol('}') {
                return self.error("枚举项后应为 `,` 或 `}`");
            }
        }
        let trailing = self.take_ident();
        self.expect_symbol(';')?;
        let type_name = if is_typedef {
            trailing
                .clone()
                .or_else(|| tag.clone())
                .ok_or_else(|| self.message("匿名 typedef enum 缺少类型名"))?
        } else {
            tag.ok_or_else(|| self.message("enum 定义缺少类型名"))?
        };
        self.types.insert(
            type_name.clone(),
            TypeDefinition::Enum {
                underlying,
                variants,
            },
        );
        if !is_typedef && let Some(instance) = trailing {
            self.roots.push(RootDefinition {
                field: FieldDefinition {
                    ty: TypeRef::Named(type_name),
                    name: instance,
                    array: None,
                },
                offset: None,
            });
        }
        Ok(())
    }

    fn parse_field_definition(&mut self) -> Result<FieldDefinition, String> {
        let ty = self.parse_type()?;
        if self.consume_symbol('*') {
            return self.error("暂不支持指针字段；请改为显式整数偏移");
        }
        let name = self.expect_ident("字段缺少名称")?;
        let mut array = None;
        let mut total = 1_u64;
        let mut dynamic = None;
        let mut saw_array = false;
        while self.check_symbol('[') && !self.check_double_symbol('[', '[') {
            saw_array = true;
            self.expect_symbol('[')?;
            let dimension = if let Some(value) = self.take_number() {
                ArrayLength::Fixed(value)
            } else {
                ArrayLength::Identifier(self.expect_ident("数组长度必须是整数或标识符")?)
            };
            self.expect_symbol(']')?;
            match dimension {
                ArrayLength::Fixed(value) => {
                    total = total
                        .checked_mul(value)
                        .ok_or_else(|| self.message("数组长度溢出"))?;
                }
                ArrayLength::Identifier(name) => {
                    if dynamic.is_some() || total != 1 {
                        return self.error("动态数组长度不能与其他维度组合");
                    }
                    dynamic = Some(name);
                }
            }
        }
        if self.consume_symbol(':') {
            return self.error("暂不支持 C/C++ 位字段");
        }
        if self.consume_symbol('=') {
            while !self.at_end() && !self.check_symbol(';') {
                self.position += 1;
            }
        }
        if let Some(name) = dynamic {
            array = Some(ArrayLength::Identifier(name));
        } else if saw_array {
            array = Some(ArrayLength::Fixed(total));
        }
        Ok(FieldDefinition { ty, name, array })
    }

    fn parse_type(&mut self) -> Result<TypeRef, String> {
        while self.consume_ident("const")
            || self.consume_ident("volatile")
            || self.consume_ident("packed")
        {}
        if self.consume_ident("struct") || self.consume_ident("enum") {
            return Ok(TypeRef::Named(
                self.expect_ident("struct/enum 后缺少类型名")?,
            ));
        }
        let first = self.expect_ident("缺少字段类型")?;
        let (primitive, display) = match first.as_str() {
            "unsigned" => {
                let second = self.take_ident().unwrap_or_else(|| "int".into());
                match second.as_str() {
                    "char" => (Primitive::U8, "unsigned char".into()),
                    "short" => {
                        self.consume_ident("int");
                        (Primitive::U16, "unsigned short".into())
                    }
                    "int" => (Primitive::U32, "unsigned int".into()),
                    "long" if self.consume_ident("long") => {
                        self.consume_ident("int");
                        (Primitive::U64, "unsigned long long".into())
                    }
                    "long" => {
                        self.consume_ident("int");
                        (Primitive::U32, "unsigned long".into())
                    }
                    _ => return self.error("unsigned 后的类型不受支持"),
                }
            }
            "signed" => {
                let second = self.take_ident().unwrap_or_else(|| "int".into());
                match second.as_str() {
                    "char" => (Primitive::I8, "signed char".into()),
                    "short" => {
                        self.consume_ident("int");
                        (Primitive::I16, "signed short".into())
                    }
                    "int" => (Primitive::I32, "signed int".into()),
                    "long" if self.consume_ident("long") => {
                        self.consume_ident("int");
                        (Primitive::I64, "signed long long".into())
                    }
                    "long" => {
                        self.consume_ident("int");
                        (Primitive::I32, "signed long".into())
                    }
                    _ => return self.error("signed 后的类型不受支持"),
                }
            }
            "u8" | "uint8" | "uint8_t" | "byte" => (Primitive::U8, first.clone()),
            "i8" | "int8" | "int8_t" => (Primitive::I8, first.clone()),
            "u16" | "uint16" | "uint16_t" | "ushort" => (Primitive::U16, first.clone()),
            "i16" | "int16" | "int16_t" | "short" => (Primitive::I16, first.clone()),
            "u32" | "uint32" | "uint32_t" | "uint" => (Primitive::U32, first.clone()),
            "i32" | "int32" | "int32_t" | "int" | "long" => (Primitive::I32, first.clone()),
            "u64" | "uint64" | "uint64_t" | "ulong" => (Primitive::U64, first.clone()),
            "i64" | "int64" | "int64_t" => (Primitive::I64, first.clone()),
            "float" | "f32" => (Primitive::F32, first.clone()),
            "double" | "f64" => (Primitive::F64, first.clone()),
            "char" => (Primitive::Char, first.clone()),
            "bool" => (Primitive::Bool, first.clone()),
            _ => return Ok(TypeRef::Named(first)),
        };
        Ok(TypeRef::Primitive(primitive, display))
    }

    fn skip_attributes(&mut self) -> Result<(), String> {
        while self.check_double_symbol('[', '[') {
            self.position += 2;
            let mut depth = 1_usize;
            while depth > 0 {
                if self.at_end() {
                    return self.error("属性 `[[...]]` 没有结束");
                }
                if self.check_double_symbol('[', '[') {
                    depth += 1;
                    self.position += 2;
                } else if self.check_double_symbol(']', ']') {
                    depth -= 1;
                    self.position += 2;
                } else {
                    self.position += 1;
                }
            }
        }
        Ok(())
    }

    fn looks_like_definition(&self, after_keyword: usize) -> bool {
        matches!(
            self.tokens
                .get(self.position + after_keyword)
                .map(|token| &token.kind),
            Some(TokenKind::Symbol('{'))
        ) || matches!(
            (
                self.tokens
                    .get(self.position + after_keyword)
                    .map(|token| &token.kind),
                self.tokens
                    .get(self.position + after_keyword + 1)
                    .map(|token| &token.kind)
            ),
            (Some(TokenKind::Ident(_)), Some(TokenKind::Symbol('{')))
        )
    }

    fn looks_like_enum_definition(&self) -> bool {
        let mut position = self.position + 1;
        if self
            .tokens
            .get(position)
            .is_some_and(|token| matches!(&token.kind, TokenKind::Ident(value) if value == "class"))
        {
            position += 1;
        }
        if matches!(
            self.tokens.get(position).map(|token| &token.kind),
            Some(TokenKind::Ident(_))
        ) {
            position += 1;
        }
        while position < self.tokens.len() {
            match self.tokens[position].kind {
                TokenKind::Symbol('{') => return true,
                TokenKind::Symbol(';') => return false,
                _ => position += 1,
            }
        }
        false
    }

    fn check_double_symbol(&self, first: char, second: char) -> bool {
        self.check_symbol(first)
            && matches!(
                self.tokens.get(self.position + 1).map(|token| &token.kind),
                Some(TokenKind::Symbol(value)) if *value == second
            )
    }

    fn check_ident(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.position).map(|token| &token.kind),
            Some(TokenKind::Ident(value)) if value == expected
        )
    }

    fn consume_ident(&mut self, expected: &str) -> bool {
        if self.check_ident(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident_value(&mut self, expected: &str) -> Result<(), String> {
        if self.consume_ident(expected) {
            Ok(())
        } else {
            self.error(&format!("应为 `{expected}`"))
        }
    }

    fn take_ident(&mut self) -> Option<String> {
        let value = match self.tokens.get(self.position).map(|token| &token.kind) {
            Some(TokenKind::Ident(value)) => value.clone(),
            _ => return None,
        };
        self.position += 1;
        Some(value)
    }

    fn expect_ident(&mut self, message: &str) -> Result<String, String> {
        self.take_ident().ok_or_else(|| self.message(message))
    }

    fn take_number(&mut self) -> Option<u64> {
        let value = match self.tokens.get(self.position).map(|token| &token.kind) {
            Some(TokenKind::Number(value)) => *value,
            _ => return None,
        };
        self.position += 1;
        Some(value)
    }

    fn expect_number(&mut self, message: &str) -> Result<u64, String> {
        self.take_number().ok_or_else(|| self.message(message))
    }

    fn check_symbol(&self, expected: char) -> bool {
        matches!(
            self.tokens.get(self.position).map(|token| &token.kind),
            Some(TokenKind::Symbol(value)) if *value == expected
        )
    }

    fn consume_symbol(&mut self, expected: char) -> bool {
        if self.check_symbol(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, expected: char) -> Result<(), String> {
        if self.consume_symbol(expected) {
            Ok(())
        } else {
            self.error(&format!("应为 `{expected}`"))
        }
    }

    fn at_end(&self) -> bool {
        self.position >= self.tokens.len()
    }

    fn message(&self, message: &str) -> String {
        let line = self
            .tokens
            .get(self.position)
            .or_else(|| self.tokens.last())
            .map_or(1, |token| token.line);
        format!("模板第 {line} 行：{message}")
    }

    fn error<T>(&self, message: &str) -> Result<T, String> {
        Err(self.message(message))
    }
}

struct CachedReader<'a> {
    document: &'a TextDocument,
    cache: Vec<u8>,
    cache_start: u64,
    cache_len: usize,
}

impl<'a> CachedReader<'a> {
    fn new(document: &'a TextDocument) -> Self {
        Self {
            document,
            cache: vec![0; READ_CACHE_BYTES],
            cache_start: 0,
            cache_len: 0,
        }
    }

    fn read(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| "读取偏移溢出".to_owned())?;
        if end > self.document.len() {
            return Err(format!(
                "二进制数据不足：偏移 0x{offset:X} 需要 {length} 字节，文件仅有 {} 字节",
                self.document.len()
            ));
        }
        if length > self.cache.len() {
            let mut output = vec![0; length];
            self.document
                .source()
                .read_at(offset, &mut output)
                .map_err(|error| error.to_string())?;
            return Ok(output);
        }
        let cache_end = self.cache_start.saturating_add(self.cache_len as u64);
        if offset < self.cache_start || end > cache_end {
            self.cache_start = offset;
            self.cache_len = self
                .document
                .source()
                .read_at(offset, &mut self.cache)
                .map_err(|error| error.to_string())?;
        }
        let start = usize::try_from(offset - self.cache_start)
            .map_err(|_| "缓存偏移无法表示".to_owned())?;
        Ok(self.cache[start..start + length].to_vec())
    }
}

struct BinaryParser<'a> {
    reader: CachedReader<'a>,
    template: &'a ParsedTemplate,
    cancel: &'a AtomicBool,
    nodes: Vec<BinaryNode>,
    roots: Vec<usize>,
    values: HashMap<String, u64>,
    maximum_end: u64,
}

impl<'a> BinaryParser<'a> {
    fn new(
        document: &'a TextDocument,
        template: &'a mut ParsedTemplate,
        cancel: &'a AtomicBool,
    ) -> Self {
        Self {
            reader: CachedReader::new(document),
            template,
            cancel,
            nodes: Vec::new(),
            roots: Vec::new(),
            values: HashMap::new(),
            maximum_end: 0,
        }
    }

    fn parse(mut self) -> Result<BinaryParseResult, String> {
        let mut cursor = 0_u64;
        for root in &self.template.roots {
            if self.cancel.load(Ordering::Acquire) {
                return Err("二进制模板解析已取消".into());
            }
            if let Some(offset) = root.offset {
                cursor = offset;
            }
            let (node, size, _) = self.parse_field(&root.field, cursor, 0)?;
            self.roots.push(node);
            cursor = cursor
                .checked_add(size)
                .ok_or_else(|| "顶层实例偏移溢出".to_owned())?;
        }
        Ok(BinaryParseResult {
            nodes: self.nodes,
            roots: self.roots,
            endianness: self.template.endianness,
            consumed_bytes: self.maximum_end,
        })
    }

    fn parse_field(
        &mut self,
        field: &FieldDefinition,
        offset: u64,
        depth: usize,
    ) -> Result<(usize, u64, Option<u64>), String> {
        if depth > MAX_TYPE_DEPTH {
            return Err(format!("类型嵌套超过 {MAX_TYPE_DEPTH} 层"));
        }
        let Some(array) = &field.array else {
            return self.parse_single(&field.name, &field.ty, offset, depth);
        };
        let count = match array {
            ArrayLength::Fixed(value) => *value,
            ArrayLength::Identifier(name) => self
                .values
                .get(name)
                .or_else(|| self.template.constants.get(name))
                .copied()
                .ok_or_else(|| format!("数组 `{}` 的长度标识符 `{name}` 尚无数值", field.name))?,
        };
        if count > MAX_ARRAY_ELEMENTS {
            return Err(format!(
                "数组 `{}` 有 {count} 项，超过单数组上限 {MAX_ARRAY_ELEMENTS}",
                field.name
            ));
        }
        let type_name = format!("{}[{count}]", field.ty.display_name());
        let parent = self.push_node(BinaryNode {
            name: field.name.clone(),
            type_name,
            byte_start: offset,
            byte_size: 0,
            value: None,
            children: Vec::new(),
        })?;

        if let Some(primitive) = self.resolve_primitive(&field.ty, 0)?
            && primitive.is_character()
        {
            let length =
                usize::try_from(count).map_err(|_| format!("字符数组 `{}` 过大", field.name))?;
            let bytes = self.reader.read(offset, length)?;
            self.nodes[parent].byte_size = count;
            self.nodes[parent].value = Some(format_character_array(&bytes));
            self.note_end(offset, count)?;
            return Ok((parent, count, None));
        }

        let mut cursor = offset;
        for index in 0..count {
            let (child, size, _) =
                self.parse_single(&format!("[{}]", index), &field.ty, cursor, depth + 1)?;
            self.nodes[parent].children.push(child);
            cursor = cursor
                .checked_add(size)
                .ok_or_else(|| format!("数组 `{}` 的偏移溢出", field.name))?;
        }
        let size = cursor - offset;
        self.nodes[parent].byte_size = size;
        self.note_end(offset, size)?;
        Ok((parent, size, None))
    }

    fn parse_single(
        &mut self,
        name: &str,
        ty: &TypeRef,
        offset: u64,
        depth: usize,
    ) -> Result<(usize, u64, Option<u64>), String> {
        if self.cancel.load(Ordering::Acquire) {
            return Err("二进制模板解析已取消".into());
        }
        match ty {
            TypeRef::Primitive(primitive, display) => {
                self.parse_primitive(name, display, *primitive, offset)
            }
            TypeRef::Named(type_name) => {
                let definition = self
                    .template
                    .types
                    .get(type_name)
                    .cloned()
                    .ok_or_else(|| format!("未知类型 `{type_name}`"))?;
                match definition {
                    TypeDefinition::Alias(target) => {
                        let (node, size, numeric) =
                            self.parse_single(name, &target, offset, depth + 1)?;
                        self.nodes[node].type_name = type_name.clone();
                        Ok((node, size, numeric))
                    }
                    TypeDefinition::Enum {
                        underlying,
                        variants,
                    } => {
                        let (node, size, numeric) =
                            self.parse_single(name, &underlying, offset, depth + 1)?;
                        self.nodes[node].type_name = type_name.clone();
                        if let Some(value) = numeric
                            && let Some(variant) = variants.get(&value)
                        {
                            self.nodes[node].value =
                                Some(format!("{variant} ({value} / 0x{value:X})"));
                        }
                        Ok((node, size, numeric))
                    }
                    TypeDefinition::Struct(fields) => {
                        let parent = self.push_node(BinaryNode {
                            name: name.to_owned(),
                            type_name: type_name.clone(),
                            byte_start: offset,
                            byte_size: 0,
                            value: None,
                            children: Vec::new(),
                        })?;
                        let mut cursor = offset;
                        for field in fields {
                            let (child, size, numeric) =
                                self.parse_field(&field, cursor, depth + 1)?;
                            self.nodes[parent].children.push(child);
                            if let Some(value) = numeric {
                                self.values.insert(field.name.clone(), value);
                            }
                            cursor = cursor
                                .checked_add(size)
                                .ok_or_else(|| format!("结构 `{type_name}` 的偏移溢出"))?;
                        }
                        let size = cursor - offset;
                        self.nodes[parent].byte_size = size;
                        self.note_end(offset, size)?;
                        Ok((parent, size, None))
                    }
                }
            }
        }
    }

    fn parse_primitive(
        &mut self,
        name: &str,
        type_name: &str,
        primitive: Primitive,
        offset: u64,
    ) -> Result<(usize, u64, Option<u64>), String> {
        let size = primitive.size();
        let bytes = self.reader.read(offset, size as usize)?;
        let unsigned = read_unsigned(&bytes, self.template.endianness);
        let value = match primitive {
            Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => {
                format!("{unsigned} / 0x{unsigned:X}")
            }
            Primitive::I8 => format_signed(unsigned, 8),
            Primitive::I16 => format_signed(unsigned, 16),
            Primitive::I32 => format_signed(unsigned, 32),
            Primitive::I64 => format_signed(unsigned, 64),
            Primitive::F32 => format!("{}", f32::from_bits(unsigned as u32)),
            Primitive::F64 => format!("{}", f64::from_bits(unsigned)),
            Primitive::Char => format_character(unsigned as u8),
            Primitive::Bool => match unsigned {
                0 => "false (0)".into(),
                1 => "true (1)".into(),
                value => format!("true ({value})"),
            },
        };
        let numeric = (!matches!(primitive, Primitive::F32 | Primitive::F64)).then_some(unsigned);
        let node = self.push_node(BinaryNode {
            name: name.to_owned(),
            type_name: type_name.to_owned(),
            byte_start: offset,
            byte_size: size,
            value: Some(value),
            children: Vec::new(),
        })?;
        self.note_end(offset, size)?;
        Ok((node, size, numeric))
    }

    fn resolve_primitive(&self, ty: &TypeRef, depth: usize) -> Result<Option<Primitive>, String> {
        if depth > MAX_TYPE_DEPTH {
            return Err(format!("类型别名超过 {MAX_TYPE_DEPTH} 层"));
        }
        match ty {
            TypeRef::Primitive(primitive, _) => Ok(Some(*primitive)),
            TypeRef::Named(name) => match self.template.types.get(name) {
                Some(TypeDefinition::Alias(target)) => self.resolve_primitive(target, depth + 1),
                Some(TypeDefinition::Enum { underlying, .. }) => {
                    self.resolve_primitive(underlying, depth + 1)
                }
                Some(TypeDefinition::Struct(_)) => Ok(None),
                None => Err(format!("未知类型 `{name}`")),
            },
        }
    }

    fn push_node(&mut self, node: BinaryNode) -> Result<usize, String> {
        if self.nodes.len() >= MAX_RESULT_NODES {
            return Err(format!("解析结果超过 {MAX_RESULT_NODES} 个节点的安全上限"));
        }
        let index = self.nodes.len();
        self.nodes.push(node);
        Ok(index)
    }

    fn note_end(&mut self, offset: u64, size: u64) -> Result<(), String> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| "解析结果偏移溢出".to_owned())?;
        self.maximum_end = self.maximum_end.max(end);
        Ok(())
    }
}

fn read_unsigned(bytes: &[u8], endianness: Endianness) -> u64 {
    match endianness {
        Endianness::Little => bytes
            .iter()
            .enumerate()
            .fold(0_u64, |value, (index, byte)| {
                value | (u64::from(*byte) << (index * 8))
            }),
        Endianness::Big => bytes
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)),
    }
}

fn format_signed(unsigned: u64, bits: u32) -> String {
    let signed = if bits == 64 {
        unsigned as i64
    } else {
        let shift = 64 - bits;
        ((unsigned << shift) as i64) >> shift
    };
    format!("{signed} / 0x{unsigned:X}")
}

fn format_character(byte: u8) -> String {
    match byte {
        b' '..=b'~' => format!("'{}' / 0x{byte:02X}", byte as char),
        b'\n' => format!("'\\n' / 0x{byte:02X}"),
        b'\r' => format!("'\\r' / 0x{byte:02X}"),
        b'\t' => format!("'\\t' / 0x{byte:02X}"),
        _ => format!("0x{byte:02X}"),
    }
}

fn format_character_array(bytes: &[u8]) -> String {
    let logical_end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let shown = logical_end.min(MAX_VALUE_PREVIEW_BYTES);
    let mut text = String::new();
    for byte in &bytes[..shown] {
        match byte {
            b' '..=b'~' => text.push(*byte as char),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b'\t' => text.push_str("\\t"),
            _ => text.push('.'),
        }
    }
    if logical_end > shown {
        text.push('…');
    }
    format!("\"{text}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::atomic::AtomicBool};

    fn parse(source: &str, bytes: &[u8]) -> BinaryParseResult {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.bin");
        fs::write(&path, bytes).unwrap();
        let document = TextDocument::open(path).unwrap();
        parse_binary_template(&document, source, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn parses_cpp_style_structs_arrays_and_dynamic_lengths() {
        let result = parse(
            r#"
                #pragma endian little
                struct Header {
                    uint16_t magic;
                    uint8_t count;
                    char name[4];
                    uint16_t values[count];
                };
                Header header;
            "#,
            &[0x34, 0x12, 2, b'N', b'K', b'G', 0, 1, 0, 2, 0],
        );
        let root = &result.nodes[result.roots[0]];
        assert_eq!(root.type_name, "Header");
        assert_eq!(root.byte_size, 11);
        assert_eq!(
            result.nodes[root.children[0]].value.as_deref(),
            Some("4660 / 0x1234")
        );
        assert_eq!(
            result.nodes[root.children[2]].value.as_deref(),
            Some("\"NKG\"")
        );
        let values = &result.nodes[root.children[3]];
        assert_eq!(values.children.len(), 2);
        assert_eq!(
            result.nodes[values.children[1]].value.as_deref(),
            Some("2 / 0x2")
        );
    }

    #[test]
    fn supports_big_endian_enums_typedef_and_absolute_root_offsets() {
        let result = parse(
            r#"
                BigEndian();
                typedef enum : uint16_t { Idle = 1, Ready = 2 } State;
                typedef struct { State state; uint16_t value; } Record;
                Record record @ 2;
            "#,
            &[0, 0, 0, 2, 0x12, 0x34],
        );
        let root = &result.nodes[result.roots[0]];
        assert_eq!(root.byte_start, 2);
        assert_eq!(
            result.nodes[root.children[0]].value.as_deref(),
            Some("Ready (2 / 0x2)")
        );
        assert_eq!(
            result.nodes[root.children[1]].value.as_deref(),
            Some("4660 / 0x1234")
        );
        assert_eq!(result.endianness, Endianness::Big);
    }

    #[test]
    fn reports_truncated_binary_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("short.bin");
        fs::write(&path, [1, 2]).unwrap();
        let document = TextDocument::open(path).unwrap();
        let error = parse_binary_template(
            &document,
            "struct Header { uint32_t value; }; Header header;",
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("二进制数据不足"));
    }
}
