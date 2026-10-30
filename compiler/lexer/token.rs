use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn new(start: usize, end: usize, line: usize, col: usize) -> Self {
        Self {
            start,
            end,
            line,
            col,
        }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            line: self.line,
            col: self.col,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpPart {
    Literal(String),
    ExprTokens(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Identifiers & Types
    Ident(String),
    TypeIdent(String),
    Label(String), // #label before { or @

    // Literals
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool), // 1b, 0b
    CharLit(char),
    StringLit(String),
    InterpString(Vec<InterpPart>),

    // Four Bracket Families & Block Openers
    LBracket,      // [
    RBracket,      // ]
    LBrace,        // {
    RBrace,        // }
    LParen,        // (
    RParen,        // )
    ComptimeOpen,  // <{
    ComptimeClose, // }>
    TryOpen,       // !{
    DeferOpen,     // ~{
    ErrDeferOpen,  // ?!{
    AsyncOpen,     // ~>{

    // Separators
    Comma,     // ,
    Semicolon, // ;
    Newline,   // \n

    // Bindings & Mutations
    Eq,          // =
    TildeEq,     // ~=
    LeftArrow,   // <-
    PlusAssign,  // +<-
    MinusAssign, // -<-
    StarAssign,  // *<-
    SlashAssign, // /<-
    FatArrow,    // =>

    // Null-safety, Conditionals, Error Operators
    Tilde,        // ~
    Question,     // ?
    SafeDot,      // ?.
    SafeLBracket, // ?[
    SafeLParen,   // ?(
    Elvis,        // ??
    SafePipe,     // ?~>
    BangBang,     // !!
    QuestBang,    // ?!
    QuestGt,      // ?>
    QuestCaret,   // ?^

    // Returns, Yield, Throw, Loop Control
    Caret,      // ^
    CaretAt,    // ^@
    CaretTilde, // ^~
    CaretHash,  // ^#
    BangCaret,  // !^
    BangAt,     // !@
    BangHash,   // !#
    TildeAt,    // ~@
    TildeHash,  // ~#

    // Pipelines & Concurrency
    Pipe,     // ~>
    ParPipe,  // ~>>
    LeftPipe, // <~

    // Ranges & Member Access
    Dot,      // .
    DotDot,   // ..
    DotDotEq, // ..=

    // Bitwise (dot-prefixed)
    BitAnd, // .&
    BitOr,  // .|
    BitXor, // .^
    BitNot, // .~
    BitShl, // .<<
    BitShr, // .>>

    // Arithmetic, Comparison, Logical
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %
    Amp,     // &
    Bar,     // |
    Bang,    // !
    EqEq,    // ==
    NotEq,   // !=
    Lt,      // <
    Gt,      // >
    LtEq,    // <=
    GtEq,    // >=

    // Special Contextual Symbols
    At,         // @
    Dollar,     // $
    Underscore, // _

    Eof,
}

impl TokenKind {
    pub fn is_primitive_type(name: &str) -> bool {
        matches!(
            name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "usz"
                | "f32"
                | "f64"
                | "bool"
                | "str"
                | "char"
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub had_space_before: bool,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span, had_space_before: bool) -> Self {
        Self {
            kind,
            span,
            had_space_before,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LexError at {}:{}: {}",
            self.span.line, self.span.col, self.message
        )
    }
}

impl std::error::Error for LexError {}
