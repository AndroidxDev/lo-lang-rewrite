pub mod token;

pub use token::{InterpPart, LexError, Span, Token, TokenKind};

pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
    col: usize,
    base_offset: usize,
    comptime_brace_stack: Vec<usize>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            line: 1,
            col: 1,
            base_offset: 0,
            comptime_brace_stack: Vec::new(),
        }
    }

    fn with_offset(src: &'a str, base_offset: usize, line: usize, col: usize) -> Self {
        Self {
            src,
            pos: 0,
            line,
            col,
            base_offset,
            comptime_brace_stack: Vec::new(),
        }
    }

    pub fn tokenize(src: &'a str) -> Result<Vec<Token>, LexError> {
        let mut lexer = Self::new(src);
        lexer.scan_all(true)
    }

    fn scan_all(&mut self, include_eof: bool) -> Result<Vec<Token>, LexError> {
        let mut tokens: Vec<Token> = Vec::new();
        let mut had_space = false;

        while let Some(ch) = self.peek() {
            // Horizontal whitespace
            if ch == ' ' || ch == '\t' || ch == '\r' {
                self.advance();
                had_space = true;
                continue;
            }

            // Newline separator (collapse consecutive newlines)
            if ch == '\n' {
                let start = self.abs_pos();
                let line = self.line;
                let col = self.col;
                self.advance();
                let end = self.abs_pos();
                if !matches!(tokens.last().map(|t| &t.kind), Some(TokenKind::Newline) | None) {
                    tokens.push(Token::new(
                        TokenKind::Newline,
                        Span::new(start, end, line, col),
                        had_space,
                    ));
                }
                had_space = true;
                continue;
            }

            // Nested block comments #[ ... ]#
            if self.starts_with("#[") {
                self.skip_block_comment()?;
                had_space = true;
                continue;
            }

            // Single-line comment `# ...` OR labeled block `#label` before `{` or `@`
            if ch == '#' {
                if let Some(label_tok) = self.try_scan_label(had_space) {
                    tokens.push(label_tok);
                    had_space = false;
                    continue;
                }
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.advance();
                }
                had_space = true;
                continue;
            }

            let tok = self.scan_token(had_space)?;
            tokens.push(tok);
            had_space = false;
        }

        if include_eof {
            let pos = self.abs_pos();
            tokens.push(Token::new(
                TokenKind::Eof,
                Span::new(pos, pos, self.line, self.col),
                had_space,
            ));
        }

        Ok(tokens)
    }

    fn abs_pos(&self) -> usize {
        self.base_offset + self.pos
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.src[self.pos..].starts_with(pat)
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn advance_n(&mut self, n: usize) {
        for _ in 0..n {
            self.advance();
        }
    }

    fn err<T>(&self, start: usize, line: usize, col: usize, msg: impl Into<String>) -> Result<T, LexError> {
        Err(LexError {
            message: msg.into(),
            span: Span::new(start, self.abs_pos(), line, col),
        })
    }

    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        let start = self.abs_pos();
        let line = self.line;
        let col = self.col;
        self.advance_n(2); // #[
        let mut depth = 1usize;

        while depth > 0 {
            if self.peek().is_none() {
                return self.err(start, line, col, "unterminated nested block comment `#[ ... ]#`");
            }
            if self.starts_with("#[") {
                self.advance_n(2);
                depth += 1;
            } else if self.starts_with("]#") {
                self.advance_n(2);
                depth -= 1;
            } else {
                self.advance();
            }
        }
        Ok(())
    }

    fn try_scan_label(&mut self, had_space: bool) -> Option<Token> {
        let next = self.peek_nth(1)?;
        if !(next.is_ascii_lowercase() || next == '_') {
            return None;
        }
        let rest = &self.src[self.pos + 1..];
        let mut ident_bytes = 0usize;
        for c in rest.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                ident_bytes += c.len_utf8();
            } else {
                break;
            }
        }
        if ident_bytes == 0 {
            return None;
        }
        let ident = &rest[..ident_bytes];
        if ident == "_" {
            return None;
        }
        let after_ident = &rest[ident_bytes..];
        let trimmed = after_ident.trim_start_matches([' ', '\t']);
        if trimmed.starts_with('{') || trimmed.starts_with('@') {
            let start = self.abs_pos();
            let line = self.line;
            let col = self.col;
            let label_name = ident.to_string();
            self.advance(); // '#'
            for _ in label_name.chars() {
                self.advance();
            }
            let end = self.abs_pos();
            return Some(Token::new(
                TokenKind::Label(label_name),
                Span::new(start, end, line, col),
                had_space,
            ));
        }
        None
    }

    fn note_open_brace(&mut self) {
        if let Some(depth) = self.comptime_brace_stack.last_mut() {
            *depth += 1;
        }
    }

    fn scan_token(&mut self, had_space: bool) -> Result<Token, LexError> {
        let start = self.abs_pos();
        let line = self.line;
        let col = self.col;
        let ch = self.peek().unwrap();

        // String literal or interpolated string
        if ch == '"' {
            return self.scan_string(had_space);
        }

        // Character literal
        if ch == '\'' {
            return self.scan_char(had_space);
        }

        // Numbers & boolean literals 1b / 0b
        if ch.is_ascii_digit() {
            return self.scan_number_or_bool(had_space);
        }

        // Identifiers & TypeIdents
        if ch.is_ascii_alphabetic() || ch == '_' {
            return Ok(self.scan_identifier(had_space));
        }

        // Special check for closing comptime `}>` vs `}`
        if ch == '}' {
            if let Some(&depth) = self.comptime_brace_stack.last() {
                if depth == 0 && self.starts_with("}>") {
                    self.comptime_brace_stack.pop();
                    self.advance_n(2);
                    return Ok(Token::new(
                        TokenKind::ComptimeClose,
                        Span::new(start, self.abs_pos(), line, col),
                        had_space,
                    ));
                } else {
                    if let Some(d) = self.comptime_brace_stack.last_mut() {
                        *d = d.saturating_sub(1);
                    }
                    self.advance();
                    return Ok(Token::new(
                        TokenKind::RBrace,
                        Span::new(start, self.abs_pos(), line, col),
                        had_space,
                    ));
                }
            } else if self.starts_with("}>") {
                self.advance_n(2);
                return Ok(Token::new(
                    TokenKind::ComptimeClose,
                    Span::new(start, self.abs_pos(), line, col),
                    had_space,
                ));
            } else {
                self.advance();
                return Ok(Token::new(
                    TokenKind::RBrace,
                    Span::new(start, self.abs_pos(), line, col),
                    had_space,
                ));
            }
        }

        // 3-character operators & block openers (longest-match first)
        let three_char = [
            ("+<-", TokenKind::PlusAssign),
            ("-<-", TokenKind::MinusAssign),
            ("*<-", TokenKind::StarAssign),
            ("/<-", TokenKind::SlashAssign),
            ("..=", TokenKind::DotDotEq),
            ("~>>", TokenKind::ParPipe),
            ("?~>", TokenKind::SafePipe),
            (".<<", TokenKind::BitShl),
            (".>>", TokenKind::BitShr),
            ("?!{", TokenKind::ErrDeferOpen),
            ("~>{", TokenKind::AsyncOpen),
        ];
        for (pat, kind) in three_char {
            if self.starts_with(pat) {
                if matches!(kind, TokenKind::ErrDeferOpen | TokenKind::AsyncOpen) {
                    self.note_open_brace();
                }
                self.advance_n(3);
                return Ok(Token::new(
                    kind,
                    Span::new(start, self.abs_pos(), line, col),
                    had_space,
                ));
            }
        }

        // 2-character operators & block openers
        if self.starts_with("<{") {
            self.comptime_brace_stack.push(0);
            self.advance_n(2);
            return Ok(Token::new(
                TokenKind::ComptimeOpen,
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ));
        }

        let two_char = [
            ("!{", TokenKind::TryOpen),
            ("~{", TokenKind::DeferOpen),
            ("~=", TokenKind::TildeEq),
            ("<-", TokenKind::LeftArrow),
            ("=>", TokenKind::FatArrow),
            ("?.", TokenKind::SafeDot),
            ("?[", TokenKind::SafeLBracket),
            ("?(", TokenKind::SafeLParen),
            ("??", TokenKind::Elvis),
            ("!!", TokenKind::BangBang),
            ("?!", TokenKind::QuestBang),
            ("?>", TokenKind::QuestGt),
            ("?^", TokenKind::QuestCaret),
            ("^@", TokenKind::CaretAt),
            ("^~", TokenKind::CaretTilde),
            ("^#", TokenKind::CaretHash),
            ("!^", TokenKind::BangCaret),
            ("!@", TokenKind::BangAt),
            ("!#", TokenKind::BangHash),
            ("~@", TokenKind::TildeAt),
            ("~#", TokenKind::TildeHash),
            ("~>", TokenKind::Pipe),
            ("<~", TokenKind::LeftPipe),
            ("..", TokenKind::DotDot),
            (".&", TokenKind::BitAnd),
            (".|", TokenKind::BitOr),
            (".^", TokenKind::BitXor),
            (".~", TokenKind::BitNot),
            ("==", TokenKind::EqEq),
            ("!=", TokenKind::NotEq),
            ("<=", TokenKind::LtEq),
            (">=", TokenKind::GtEq),
        ];
        for (pat, kind) in two_char {
            if self.starts_with(pat) {
                if matches!(kind, TokenKind::TryOpen | TokenKind::DeferOpen) {
                    self.note_open_brace();
                }
                self.advance_n(2);
                return Ok(Token::new(
                    kind,
                    Span::new(start, self.abs_pos(), line, col),
                    had_space,
                ));
            }
        }

        // Single-character operators & brackets
        let single_kind = match ch {
            '[' => Some(TokenKind::LBracket),
            ']' => Some(TokenKind::RBracket),
            '{' => {
                self.note_open_brace();
                Some(TokenKind::LBrace)
            }
            '(' => Some(TokenKind::LParen),
            ')' => Some(TokenKind::RParen),
            ',' => Some(TokenKind::Comma),
            ';' => Some(TokenKind::Semicolon),
            '=' => Some(TokenKind::Eq),
            '~' => Some(TokenKind::Tilde),
            '?' => Some(TokenKind::Question),
            '^' => Some(TokenKind::Caret),
            '.' => Some(TokenKind::Dot),
            '+' => Some(TokenKind::Plus),
            '-' => Some(TokenKind::Minus),
            '*' => Some(TokenKind::Star),
            '/' => Some(TokenKind::Slash),
            '%' => Some(TokenKind::Percent),
            '&' => Some(TokenKind::Amp),
            '|' => Some(TokenKind::Bar),
            '!' => Some(TokenKind::Bang),
            '<' => Some(TokenKind::Lt),
            '>' => Some(TokenKind::Gt),
            '@' => Some(TokenKind::At),
            '$' => Some(TokenKind::Dollar),
            _ => None,
        };

        if let Some(kind) = single_kind {
            self.advance();
            return Ok(Token::new(
                kind,
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ));
        }

        self.advance();
        self.err(
            start,
            line,
            col,
            format!("unexpected character `{}`", ch),
        )
    }

    fn scan_identifier(&mut self, had_space: bool) -> Token {
        let start = self.abs_pos();
        let rel_start = self.pos;
        let line = self.line;
        let col = self.col;

        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = &self.src[rel_start..self.pos];
        let kind = if text == "_" {
            TokenKind::Underscore
        } else if text.chars().next().unwrap().is_ascii_uppercase()
            || TokenKind::is_primitive_type(text)
        {
            TokenKind::TypeIdent(text.to_string())
        } else {
            TokenKind::Ident(text.to_string())
        };

        Token::new(kind, Span::new(start, self.abs_pos(), line, col), had_space)
    }

    fn scan_number_or_bool(&mut self, had_space: bool) -> Result<Token, LexError> {
        let start = self.abs_pos();
        let line = self.line;
        let col = self.col;

        // Check boolean literals 1b and 0b (when not followed by alphanumeric/underscore)
        if (self.starts_with("1b") || self.starts_with("0b"))
            && !self
                .peek_nth(2)
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            let val = self.starts_with("1b");
            self.advance_n(2);
            return Ok(Token::new(
                TokenKind::BoolLit(val),
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ));
        }

        // Hexadecimal integer 0x...
        if self.starts_with("0x") || self.starts_with("0X") {
            self.advance_n(2);
            let mut digits = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    digits.push(c);
                    self.advance();
                } else if c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
            if digits.is_empty() {
                return self.err(start, line, col, "expected hex digits after `0x`");
            }
            let val = u64::from_str_radix(&digits, 16)
                .map_err(|e| LexError {
                    message: format!("invalid hex integer `0x{}`: {}", digits, e),
                    span: Span::new(start, self.abs_pos(), line, col),
                })?;
            return Ok(Token::new(
                TokenKind::IntLit(val as i64),
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ));
        }

        // Binary integer 0b0101...
        if self.starts_with("0b") || self.starts_with("0B") {
            self.advance_n(2);
            let mut digits = String::new();
            while let Some(c) = self.peek() {
                if c == '0' || c == '1' {
                    digits.push(c);
                    self.advance();
                } else if c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
            if digits.is_empty() {
                return self.err(start, line, col, "expected binary digits after `0b`");
            }
            let val = u64::from_str_radix(&digits, 2)
                .map_err(|e| LexError {
                    message: format!("invalid binary integer `0b{}`: {}", digits, e),
                    span: Span::new(start, self.abs_pos(), line, col),
                })?;
            return Ok(Token::new(
                TokenKind::IntLit(val as i64),
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ));
        }

        // Decimal integer or float
        let mut raw = String::new();
        let mut is_float = false;

        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                raw.push(c);
                self.advance();
            } else if c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        // Fractional part ONLY if `.` is followed by an ASCII digit (never consume `..` or `.method`)
        if self.peek() == Some('.') && self.peek_nth(1).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            raw.push('.');
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    raw.push(c);
                    self.advance();
                } else if c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Scientific exponent part
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            raw.push(self.advance().unwrap());
            if matches!(self.peek(), Some('+' | '-')) {
                raw.push(self.advance().unwrap());
            }
            let mut exp_digits = 0usize;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    raw.push(c);
                    exp_digits += 1;
                    self.advance();
                } else if c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
            if exp_digits == 0 {
                return self.err(start, line, col, "expected exponent digits in float literal");
            }
        }

        // Reject trailing invalid suffix like `123abc`
        if let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                return self.err(
                    start,
                    line,
                    col,
                    format!("invalid suffix starting with `{}` on numeric literal", c),
                );
            }
        }

        if is_float {
            let val: f64 = raw.parse().map_err(|e| LexError {
                message: format!("invalid float literal `{}`: {}", raw, e),
                span: Span::new(start, self.abs_pos(), line, col),
            })?;
            Ok(Token::new(
                TokenKind::FloatLit(val),
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ))
        } else {
            let val: i64 = raw
                .parse::<i64>()
                .or_else(|_| raw.parse::<u64>().map(|u| u as i64))
                .map_err(|e| LexError {
                    message: format!("invalid integer literal `{}`: {}", raw, e),
                    span: Span::new(start, self.abs_pos(), line, col),
                })?;
            Ok(Token::new(
                TokenKind::IntLit(val),
                Span::new(start, self.abs_pos(), line, col),
                had_space,
            ))
        }
    }

    fn scan_char(&mut self, had_space: bool) -> Result<Token, LexError> {
        let start = self.abs_pos();
        let line = self.line;
        let col = self.col;
        self.advance(); // opening '\''

        let ch = match self.advance() {
            None | Some('\n') => {
                return self.err(start, line, col, "unterminated character literal")
            }
            Some('\\') => self.scan_escape_char(start, line, col)?,
            Some(c) => c,
        };

        if self.peek() != Some('\'') {
            return self.err(start, line, col, "expected closing `'` after character literal");
        }
        self.advance();

        Ok(Token::new(
            TokenKind::CharLit(ch),
            Span::new(start, self.abs_pos(), line, col),
            had_space,
        ))
    }

    fn scan_escape_char(&mut self, start: usize, line: usize, col: usize) -> Result<char, LexError> {
        match self.advance() {
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('\\') => Ok('\\'),
            Some('"') => Ok('"'),
            Some('\'') => Ok('\''),
            Some('0') => Ok('\0'),
            Some('{') => Ok('{'),
            Some('}') => Ok('}'),
            Some('x') => {
                let mut hex = String::new();
                for _ in 0..2 {
                    match self.advance() {
                        Some(h) if h.is_ascii_hexdigit() => hex.push(h),
                        _ => return self.err(start, line, col, "expected two hex digits after `\\x`"),
                    }
                }
                let byte = u8::from_str_radix(&hex, 16).unwrap();
                Ok(byte as char)
            }
            Some(other) => self.err(
                start,
                line,
                col,
                format!("unknown escape sequence `\\{}`", other),
            ),
            None => self.err(start, line, col, "unterminated escape sequence"),
        }
    }

    fn scan_string(&mut self, had_space: bool) -> Result<Token, LexError> {
        let start = self.abs_pos();
        let line = self.line;
        let col = self.col;
        self.advance(); // opening '"'

        let mut parts: Vec<InterpPart> = Vec::new();
        let mut current_lit = String::new();
        let mut has_interp = false;

        loop {
            match self.peek() {
                None => return self.err(start, line, col, "unterminated string literal"),
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    let esc = self.scan_escape_char(start, line, col)?;
                    current_lit.push(esc);
                }
                Some('{') => {
                    has_interp = true;
                    if !current_lit.is_empty() {
                        parts.push(InterpPart::Literal(std::mem::take(&mut current_lit)));
                    }
                    let expr_line = self.line;
                    let expr_col = self.col + 1;
                    self.advance(); // '{'
                    let expr_rel_start = self.pos;
                    let expr_abs_start = self.abs_pos();

                    // Scan balanced `{ ... }` respecting nested strings, chars, and comments
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.peek() {
                            None => {
                                return self.err(
                                    start,
                                    line,
                                    col,
                                    "unterminated `{...}` interpolation in string literal",
                                )
                            }
                            Some('"') => {
                                self.advance();
                                while let Some(sc) = self.peek() {
                                    self.advance();
                                    if sc == '\\' {
                                        let _ = self.advance();
                                    } else if sc == '"' {
                                        break;
                                    }
                                }
                            }
                            Some('\'') => {
                                self.advance();
                                while let Some(cc) = self.peek() {
                                    self.advance();
                                    if cc == '\\' {
                                        let _ = self.advance();
                                    } else if cc == '\'' {
                                        break;
                                    }
                                }
                            }
                            Some('#') if self.starts_with("#[") => {
                                self.skip_block_comment()?;
                            }
                            Some('{') => {
                                depth += 1;
                                self.advance();
                            }
                            Some('}') => {
                                depth -= 1;
                                if depth > 0 {
                                    self.advance();
                                }
                            }
                            Some(_) => {
                                self.advance();
                            }
                        }
                    }

                    let expr_rel_end = self.pos;
                    self.advance(); // closing '}'

                    let expr_slice = &self.src[expr_rel_start..expr_rel_end];
                    if expr_slice.trim().is_empty() {
                        return self.err(
                            expr_abs_start,
                            expr_line,
                            expr_col,
                            "empty `{}` interpolation inside string literal",
                        );
                    }
                    let mut sub_lexer =
                        Lexer::with_offset(expr_slice, expr_abs_start, expr_line, expr_col);
                    let mut expr_tokens = sub_lexer.scan_all(true)?;
                    // Strip leading/trailing newlines inside interpolation if any
                    if matches!(expr_tokens.first().map(|t| &t.kind), Some(TokenKind::Newline)) {
                        expr_tokens.remove(0);
                    }
                    parts.push(InterpPart::ExprTokens(expr_tokens));
                }
                Some(_) => {
                    current_lit.push(self.advance().unwrap());
                }
            }
        }

        let span = Span::new(start, self.abs_pos(), line, col);
        if !has_interp {
            Ok(Token::new(TokenKind::StringLit(current_lit), span, had_space))
        } else {
            if !current_lit.is_empty() {
                parts.push(InterpPart::Literal(current_lit));
            }
            Ok(Token::new(TokenKind::InterpString(parts), span, had_space))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nested_comments_and_booleans() {
        let src = r#"
            # Single-line comment in Russian: Привет
            #[ Outer comment #[ deeply #[ nested ]# comment ]# still outer ]#
            flag_t = 1b
            flag_f = 0b
            bin_val = 0b1010
            hex_val = 0xFF
        "#;
        let tokens = Lexer::tokenize(src).expect("lex should succeed");
        let kinds: Vec<&TokenKind> = tokens
            .iter()
            .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Eof))
            .map(|t| &t.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                &TokenKind::Ident("flag_t".into()),
                &TokenKind::Eq,
                &TokenKind::BoolLit(true),
                &TokenKind::Ident("flag_f".into()),
                &TokenKind::Eq,
                &TokenKind::BoolLit(false),
                &TokenKind::Ident("bin_val".into()),
                &TokenKind::Eq,
                &TokenKind::IntLit(10),
                &TokenKind::Ident("hex_val".into()),
                &TokenKind::Eq,
                &TokenKind::IntLit(255),
            ]
        );
    }

    #[test]
    fn test_string_interpolation_utf8() {
        let src = r#""Привет {user.name}, сумма = {a + b}!""#;
        let tokens = Lexer::tokenize(src).expect("lex should succeed");
        match &tokens[0].kind {
            TokenKind::InterpString(parts) => {
                assert_eq!(parts.len(), 5);
                assert_eq!(parts[0], InterpPart::Literal("Привет ".into()));
                assert_eq!(parts[2], InterpPart::Literal(", сумма = ".into()));
                assert_eq!(parts[4], InterpPart::Literal("!".into()));
            }
            other => panic!("expected InterpString, got {:?}", other),
        }
    }

    #[test]
    fn test_all_compound_operators_and_comptime() {
        let src = "+<- -<- *<- /<- ?. ?[ ?( ?? ?~> ~>> ~> <~ ?! ?> ?^ ^@ ^~ ^# !^ !@ !# ~@ ~# .& .| .^ .~ .<< .>> .. ..= !{ ~{ ?!{ ~>{ <{ 1 }>";
        let tokens = Lexer::tokenize(src).expect("lex should succeed");
        let expected = vec![
            TokenKind::PlusAssign,
            TokenKind::MinusAssign,
            TokenKind::StarAssign,
            TokenKind::SlashAssign,
            TokenKind::SafeDot,
            TokenKind::SafeLBracket,
            TokenKind::SafeLParen,
            TokenKind::Elvis,
            TokenKind::SafePipe,
            TokenKind::ParPipe,
            TokenKind::Pipe,
            TokenKind::LeftPipe,
            TokenKind::QuestBang,
            TokenKind::QuestGt,
            TokenKind::QuestCaret,
            TokenKind::CaretAt,
            TokenKind::CaretTilde,
            TokenKind::CaretHash,
            TokenKind::BangCaret,
            TokenKind::BangAt,
            TokenKind::BangHash,
            TokenKind::TildeAt,
            TokenKind::TildeHash,
            TokenKind::BitAnd,
            TokenKind::BitOr,
            TokenKind::BitXor,
            TokenKind::BitNot,
            TokenKind::BitShl,
            TokenKind::BitShr,
            TokenKind::DotDot,
            TokenKind::DotDotEq,
            TokenKind::TryOpen,
            TokenKind::DeferOpen,
            TokenKind::ErrDeferOpen,
            TokenKind::AsyncOpen,
            TokenKind::ComptimeOpen,
            TokenKind::IntLit(1),
            TokenKind::ComptimeClose,
            TokenKind::Eof,
        ];
        let actual: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ranges_not_confused_with_floats_and_labels() {
        let src = "0..10, 1..=100, 3.14, #search { ^#search 0 }";
        let tokens = Lexer::tokenize(src).expect("lex should succeed");
        let kinds: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::IntLit(0),
                TokenKind::DotDot,
                TokenKind::IntLit(10),
                TokenKind::Comma,
                TokenKind::IntLit(1),
                TokenKind::DotDotEq,
                TokenKind::IntLit(100),
                TokenKind::Comma,
                TokenKind::FloatLit(3.14),
                TokenKind::Comma,
                TokenKind::Label("search".into()),
                TokenKind::LBrace,
                TokenKind::CaretHash,
                TokenKind::Ident("search".into()),
                TokenKind::IntLit(0),
                TokenKind::RBrace,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_entire_lo_md_examples_tokenize() {
        let spec_order_service = r#"
Order.calc_discount = {
  code = @.coupon?.trim()?.upper() ?? ""
  ? (code == "VIP50") { 0.50 } { 0.0 }
}
"#;
        let tokens = Lexer::tokenize(spec_order_service).expect("should lex order service");
        assert!(tokens.len() > 20);
    }
}
