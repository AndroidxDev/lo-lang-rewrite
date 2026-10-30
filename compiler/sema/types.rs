use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Unit,
    Never,
    Null,
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Usz,
    F32,
    F64,
    Char,
    Str,
    Nullable(Box<Type>),
    Array(Box<Type>),
    Tuple(Vec<Type>),
    Map(Box<Type>, Box<Type>),
    Set(Box<Type>),
    Range(Box<Type>),
    Generator(Box<Type>),
    Task(Box<Type>),
    Channel(Box<Type>),
    Struct(String),
    Enum(String),
    Interface(String),
    Func {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    Module(String),
    ErrorAny,
}

impl Type {
    pub fn make_nullable(t: Type) -> Type {
        match t {
            Type::Null => Type::Null,
            Type::Nullable(_) => t,
            Type::Never => Type::Never,
            other => Type::Nullable(Box::new(other)),
        }
    }

    pub fn unwrap_nullable(&self) -> Option<&Type> {
        match self {
            Type::Nullable(inner) => Some(inner),
            _ => None,
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, Type::Null | Type::Nullable(_))
    }

    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            Type::I8
                | Type::I16
                | Type::I32
                | Type::I64
                | Type::U8
                | Type::U16
                | Type::U32
                | Type::U64
                | Type::Usz
        )
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || self.is_float()
    }

    pub fn mangle(&self) -> String {
        match self {
            Type::Unit => "unit".into(),
            Type::Never => "never".into(),
            Type::Null => "null".into(),
            Type::Bool => "bool".into(),
            Type::I8 => "i8".into(),
            Type::I16 => "i16".into(),
            Type::I32 => "i32".into(),
            Type::I64 => "i64".into(),
            Type::U8 => "u8".into(),
            Type::U16 => "u16".into(),
            Type::U32 => "u32".into(),
            Type::U64 => "u64".into(),
            Type::Usz => "usz".into(),
            Type::F32 => "f32".into(),
            Type::F64 => "f64".into(),
            Type::Char => "char".into(),
            Type::Str => "str".into(),
            Type::Nullable(t) => format!("opt_{}", t.mangle()),
            Type::Array(t) => format!("arr_{}", t.mangle()),
            Type::Tuple(ts) => {
                let parts: Vec<String> = ts.iter().map(|t| t.mangle()).collect();
                format!("tup_{}", parts.join("_"))
            }
            Type::Map(k, v) => format!("map_{}_{}", k.mangle(), v.mangle()),
            Type::Set(t) => format!("set_{}", t.mangle()),
            Type::Range(t) => format!("rng_{}", t.mangle()),
            Type::Generator(t) => format!("gen_{}", t.mangle()),
            Type::Task(t) => format!("task_{}", t.mangle()),
            Type::Channel(t) => format!("chan_{}", t.mangle()),
            Type::Struct(s) => format!("st_{}", s),
            Type::Enum(s) => format!("en_{}", s),
            Type::Interface(s) => format!("if_{}", s),
            Type::Func { params, ret } => {
                let ps: Vec<String> = params.iter().map(|p| p.mangle()).collect();
                format!("fn_{}_to_{}", ps.join("_"), ret.mangle())
            }
            Type::Module(m) => format!("mod_{}", m),
            Type::ErrorAny => "err_any".into(),
        }
    }

    /// Check if `actual` can be assigned to `expected` under strict Kotlin-style null-safety.
    pub fn is_assignable_to(&self, expected: &Type) -> bool {
        if self == expected || *self == Type::Never {
            return true;
        }
        // Empty `[]` (Unit) coerces to empty Array `[T..]` or empty Map `[K => V]`
        if *self == Type::Unit && matches!(expected, Type::Array(_) | Type::Map(_, _)) {
            return true;
        }
        // `~` (Null) is assignable ONLY to Nullable `[T?]`, NEVER to non-null `[T]`
        if *self == Type::Null {
            return matches!(expected, Type::Nullable(_) | Type::Null);
        }
        // Non-null `T` can be implicitly widened into `[T?]`
        if let Type::Nullable(inner_exp) = expected {
            if self.is_assignable_to(inner_exp) {
                return true;
            }
            if let Type::Nullable(inner_act) = self {
                return inner_act.is_assignable_to(inner_exp);
            }
        }
        // `[Null..]` (e.g. `[~]`) or `[T..]` into `[T?..]`
        if let (Type::Array(act_el), Type::Array(exp_el)) = (self, expected) {
            return act_el.is_assignable_to(exp_el);
        }
        // Widening lossless integer literals/subtypes when annotated (e.g. i64 literal into i32/u8/f64)
        if self.is_integer() && expected.is_integer() {
            return true;
        }
        if *self == Type::F64 && *expected == Type::F32 {
            return true;
        }
        false
    }

    /// Compute least upper bound (common supertype) of two branches (`if/else`, `match`, array elements).
    pub fn unify(a: &Type, b: &Type) -> Option<Type> {
        if a == b {
            return Some(a.clone());
        }
        if *a == Type::Never {
            return Some(b.clone());
        }
        if *b == Type::Never {
            return Some(a.clone());
        }
        if *a == Type::Null && *b == Type::Null {
            return Some(Type::Null);
        }
        if *a == Type::Null {
            return Some(Type::make_nullable(b.clone()));
        }
        if *b == Type::Null {
            return Some(Type::make_nullable(a.clone()));
        }
        if let Type::Nullable(ia) = a {
            let inner = if let Type::Nullable(ib) = b {
                Self::unify(ia, ib)?
            } else {
                Self::unify(ia, b)?
            };
            return Some(Type::make_nullable(inner));
        }
        if let Type::Nullable(ib) = b {
            let inner = Self::unify(a, ib)?;
            return Some(Type::make_nullable(inner));
        }
        if *a == Type::Unit && matches!(b, Type::Array(_) | Type::Map(_, _)) {
            return Some(b.clone());
        }
        if *b == Type::Unit && matches!(a, Type::Array(_) | Type::Map(_, _)) {
            return Some(a.clone());
        }
        if let (Type::Array(ea), Type::Array(eb)) = (a, b) {
            return Some(Type::Array(Box::new(Self::unify(ea, eb)?)));
        }
        if (a.is_float() && b.is_numeric()) || (b.is_float() && a.is_numeric()) {
            return Some(Type::F64);
        }
        if a.is_integer() && b.is_integer() {
            return Some(Type::I64);
        }
        None
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Unit => write!(f, "[]"),
            Type::Never => write!(f, "!"),
            Type::Null => write!(f, "~"),
            Type::Bool => write!(f, "bool"),
            Type::I8 => write!(f, "i8"),
            Type::I16 => write!(f, "i16"),
            Type::I32 => write!(f, "i32"),
            Type::I64 => write!(f, "i64"),
            Type::U8 => write!(f, "u8"),
            Type::U16 => write!(f, "u16"),
            Type::U32 => write!(f, "u32"),
            Type::U64 => write!(f, "u64"),
            Type::Usz => write!(f, "usz"),
            Type::F32 => write!(f, "f32"),
            Type::F64 => write!(f, "f64"),
            Type::Char => write!(f, "char"),
            Type::Str => write!(f, "str"),
            Type::Nullable(t) => write!(f, "[{}?]", t),
            Type::Array(t) => write!(f, "[{}..]", t),
            Type::Tuple(ts) => {
                let s: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
                write!(f, "[{}]", s.join(", "))
            }
            Type::Map(k, v) => write!(f, "[{} => {}]", k, v),
            Type::Set(t) => write!(f, "[%{}]", t),
            Type::Range(t) => write!(f, "{}..{}", t, t),
            Type::Generator(t) => write!(f, "gen[{}]", t),
            Type::Task(t) => write!(f, "[~{}]", t),
            Type::Channel(t) => write!(f, "chan[~{}]", t),
            Type::Struct(s) | Type::Enum(s) | Type::Interface(s) => write!(f, "[{}]", s),
            Type::Func { params, ret } => {
                let ps: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                write!(f, "({}) [{}]", ps.join(", "), ret)
            }
            Type::Module(m) => write!(f, "<module:{}>", m),
            Type::ErrorAny => write!(f, "[Err]"),
        }
    }
}
