// Representative SQL-expression subset via rust-peg (compile-time codegen, no runtime dep).
#[derive(Debug)]
pub enum Ast {
    Num(f64),
    Str(String),
    Ident(String),
    Call(String, Vec<Ast>),
    Unary(&'static str, Box<Ast>),
    Binary(&'static str, Box<Ast>, Box<Ast>),
}

peg::parser! {
    grammar sqlexpr() for str {
        rule _ = quiet!{[' ' | '\t' | '\n']*}
        rule num() -> Ast = n:$(['0'..='9']+ ("." ['0'..='9']+)?) { Ast::Num(n.parse().unwrap()) }
        rule string() -> Ast = "'" s:$([^'\'']*) "'" { Ast::Str(s.into()) }
        rule ident() -> String
            = s:$(['a'..='z' | 'A'..='Z' | '_']['a'..='z' | 'A'..='Z' | '0'..='9' | '_']*) { s.into() }
        rule call() -> Ast = f:ident() _ "(" _ args:(expression() ** (_ "," _)) _ ")" { Ast::Call(f, args) }
        rule atom() -> Ast
            = num() / string() / call() / i:ident() { Ast::Ident(i) } / "(" _ e:expression() _ ")" { e }
        pub rule expression() -> Ast = precedence! {
            x:(@) _ "or" _ y:@ { Ast::Binary("or", Box::new(x), Box::new(y)) }
            --
            x:(@) _ "and" _ y:@ { Ast::Binary("and", Box::new(x), Box::new(y)) }
            --
            "not" _ x:@ { Ast::Unary("not", Box::new(x)) }
            --
            x:(@) _ "=" _ y:@ { Ast::Binary("=", Box::new(x), Box::new(y)) }
            x:(@) _ "!=" _ y:@ { Ast::Binary("!=", Box::new(x), Box::new(y)) }
            x:(@) _ "<=" _ y:@ { Ast::Binary("<=", Box::new(x), Box::new(y)) }
            x:(@) _ ">=" _ y:@ { Ast::Binary(">=", Box::new(x), Box::new(y)) }
            x:(@) _ "<" _ y:@ { Ast::Binary("<", Box::new(x), Box::new(y)) }
            x:(@) _ ">" _ y:@ { Ast::Binary(">", Box::new(x), Box::new(y)) }
            --
            x:(@) _ "+" _ y:@ { Ast::Binary("+", Box::new(x), Box::new(y)) }
            x:(@) _ "-" _ y:@ { Ast::Binary("-", Box::new(x), Box::new(y)) }
            --
            x:(@) _ "*" _ y:@ { Ast::Binary("*", Box::new(x), Box::new(y)) }
            x:(@) _ "/" _ y:@ { Ast::Binary("/", Box::new(x), Box::new(y)) }
            --
            "-" _ x:@ { Ast::Unary("-", Box::new(x)) }
            --
            _ a:atom() _ { a }
        }
    }
}

static mut LAST_ERR: Option<String> = None;

#[no_mangle]
pub extern "C" fn parse(ptr: *const u8, len: usize) -> i32 {
    let s = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len)) };
    match sqlexpr::expression(s) {
        Ok(ast) => {
            core::mem::forget(ast);
            1
        }
        Err(e) => {
            // include error formatting machinery in the measurement
            unsafe { LAST_ERR = Some(format!("parse error at {}: expected {}", e.location, e.expected)) };
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v: Vec<u8> = Vec::with_capacity(n);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}
