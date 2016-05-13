// Copyright 2016 Kyle Mayes
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ops;
use std::collections::{HashSet};

use syntax::parse::token;
use syntax::ast::*;
use syntax::ext::base::{DummyResult, ExtCtxt, MacEager, MacResult};
use syntax::ext::build::{AstBuilder};
use syntax::codemap::{DUMMY_SP, Span};
use syntax::parse::token::{BinOpToken, DelimToken, Token};
use syntax::ptr::{P};

use super::{PluginResult};
use super::utility::{self, ToError, ToExpr, TtsIterator};

//================================================
// Macros
//================================================

// spec! ________________________________________

/// Constructs a `Specification`.
#[macro_export]
macro_rules! spec {
    ($($specifier:expr), *) => (Specification(vec![$($specifier), *]));
    ($($specifier:expr), *,) => (Specification(vec![$($specifier), *]));
}

//================================================
// Enums
//================================================

// Amount ________________________________________

/// Indicates how many times a sequence is allowed to occur.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Amount {
    /// `+`
    OneOrMore,
    /// `*`
    ZeroOrMore,
    /// `?`
    ZeroOrOne,
}

impl ToExpr for Amount {
    fn to_expr(&self, context: &mut ExtCtxt, span: Span) -> P<Expr> {
        let path = utility::mk_path(context, &["easy_plugin", "Amount", &format!("{:?}", self)]);
        context.expr_path(context.path_global(span, path))
    }
}

// Specifier _____________________________________

/// A piece of a plugin argument specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Specifier {
    /// An attribute (e.g., `#[cfg(target_os = "windows")]`).
    Attr(String),
    /// A binary operator (e.g., `+`, `*`).
    BinOp(String),
    /// A brace-delimited sequence of statements (e.g., `{ log(error, "hi"); return 12; }`).
    Block(String),
    /// A delimited sequence of token trees (e.g., `()`, `[foo - "bar"]`).
    Delim(String),
    /// An expression (e.g., `2 + 2`, `if true { 1 } else { 2 }`, `f(42)`).
    Expr(String),
    /// An identifier (e.g., `x`, `foo`).
    Ident(String),
    /// An item (e.g., `fn foo() { }`, `struct Bar;`).
    Item(String),
    /// A lifetime (e.g., `'a`).
    Lftm(String),
    /// A literal (e.g., `322`, `'a'`, `"foo"`).
    Lit(String),
    /// A "meta" item, as found in attributes (e.g., `cfg(target_os = "windows")`).
    Meta(String),
    /// A pattern (e.g., `Some(t)`, `(17, 'a')`, `_`).
    Pat(String),
    /// A qualified name (e.g., `T::SpecialA`).
    Path(String),
    /// A single statement (e.g., `let x = 3`).
    Stmt(String),
    /// A type (e.g., `i32`, `Vec<(char, String)>`, `&T`).
    Ty(String),
    /// A single token.
    Tok(String),
    /// A single token tree.
    Tt(String),
    /// A non-variable piece.
    Specific(Token),
    /// A delimited piece.
    Delimited(DelimToken, Specification),
    /// A sequence piece.
    Sequence(Amount, Option<Token>, Specification),
    /// A named sequence piece.
    NamedSequence(String, Amount, Option<Token>, Specification),
}

impl Specifier {
    //- Constructors -----------------------------

    /// Returns a new `Specifier` for the given identifier.
    pub fn specific_ident(ident: &str) -> Specifier {
        let ident = Ident::with_empty_ctxt(token::intern(ident));
        Specifier::Specific(Token::Ident(ident))
    }

    /// Returns a new `Specifier` for the given lifetime.
    pub fn specific_lftm(lftm: &str) -> Specifier {
        let lftm = Ident::with_empty_ctxt(token::intern(lftm));
        Specifier::Specific(Token::Lifetime(lftm))
    }

    //- Accessors --------------------------------

    /// Returns the name of this specifier, if applicable.
    pub fn get_name(&self) -> Option<&String> {
        match *self {
            Specifier::Attr(ref name) |
            Specifier::BinOp(ref name) |
            Specifier::Block(ref name) |
            Specifier::Delim(ref name) |
            Specifier::Expr(ref name) |
            Specifier::Ident(ref name) |
            Specifier::Item(ref name) |
            Specifier::Lftm(ref name) |
            Specifier::Lit(ref name) |
            Specifier::Meta(ref name) |
            Specifier::Pat(ref name) |
            Specifier::Path(ref name) |
            Specifier::Stmt(ref name) |
            Specifier::Ty(ref name) |
            Specifier::Tok(ref name) |
            Specifier::Tt(ref name) |
            Specifier::NamedSequence(ref name, _, _, _) => Some(name),
            _ => None,
        }
    }

    fn to_fields_(&self, context: &mut ExtCtxt, span: Span, stack: &[Amount]) -> Vec<Field> {
        let name = match self.get_name() {
            Some(name) => name,
            None => return vec![],
        };

        let mut expr = quote_expr!(context, _m.get($name).unwrap());
        if stack.is_empty() {
            expr =  quote_expr!(context, $expr.into());
            vec![context.field_imm(span, context.ident_of(name), expr)]
        } else {
            let f = stack.iter().skip(1).fold(quote_expr!(context, |s| s.into()), |f, a| {
                if *a == Amount::ZeroOrOne {
                    quote_expr!(context, |s| s.as_sequence().iter().map($f).next())
                } else {
                    quote_expr!(context, |s| s.as_sequence().iter().map($f).collect())
                }
            });
            if stack[0] == Amount::ZeroOrOne {
                expr = quote_expr!(context, $expr.as_sequence().iter().map($f).next());
            } else {
                expr = quote_expr!(context, $expr.as_sequence().iter().map($f).collect());
            }
            vec![context.field_imm(span, context.ident_of(name), expr)]
        }
    }

    /// Returns `Field`s that would initialize values matched by this specifier.
    pub fn to_fields(&self, context: &mut ExtCtxt, span: Span, stack: &[Amount]) -> Vec<Field> {
        match *self {
            Specifier::Delimited(_, ref subspecification) =>
                subspecification.to_fields(context, span, stack),
            Specifier::Sequence(amount, _, ref subspecification) => {
                let mut stack = stack.to_vec();
                stack.push(amount);
                subspecification.to_fields(context, span, &stack)
            },
            _ => self.to_fields_(context, span, stack),
        }
    }

    /// Returns `StructField`s that could contain values matched by this specifier.
    pub fn to_struct_fields(&self, context: &mut ExtCtxt, span: Span) -> Vec<StructField> {
        macro_rules! field {
            ($name:expr, $($variant:tt)*) => ({
                let field = StructField {
                    span: span,
                    ident: Some(context.ident_of($name)),
                    vis: Visibility::Public,
                    id: DUMMY_NODE_ID,
                    ty: quote_ty!(context, $($variant)*),
                    attrs: vec![],
                };
                vec![field]
            });
        }

        macro_rules! field_spanned {
            ($name:expr, $($variant:tt)*) => ({
                field!($name, ::syntax::codemap::Spanned<$($variant)*>)
            });
        }

        match *self {
            Specifier::Attr(ref name) => field!(name, ::syntax::ast::Attribute),
            Specifier::BinOp(ref name) => field_spanned!(name, ::syntax::parse::token::BinOpToken),
            Specifier::Block(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::Block>),
            Specifier::Delim(ref name) =>
                field_spanned!(name, ::std::rc::Rc<::syntax::ast::Delimited>),
            Specifier::Expr(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::Expr>),
            Specifier::Ident(ref name) => field_spanned!(name, ::syntax::ast::Ident),
            Specifier::Item(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::Item>),
            Specifier::Lftm(ref name) => field_spanned!(name, ::syntax::ast::Name),
            Specifier::Lit(ref name) => field!(name, ::syntax::ast::Lit),
            Specifier::Meta(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::MetaItem>),
            Specifier::Pat(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::Pat>),
            Specifier::Path(ref name) => field!(name, ::syntax::ast::Path),
            Specifier::Stmt(ref name) => field!(name, ::syntax::ast::Stmt),
            Specifier::Ty(ref name) => field!(name, ::syntax::ptr::P<::syntax::ast::Ty>),
            Specifier::Tok(ref name) => field_spanned!(name, ::syntax::parse::token::Token),
            Specifier::Tt(ref name) => field!(name, ::syntax::ast::TokenTree),
            Specifier::Delimited(_, ref subspecification) =>
                subspecification.to_struct_fields(context, span),
            Specifier::Sequence(amount, _, ref subspecification) => {
                let mut subfields = subspecification.to_struct_fields(context, span);
                for subfield in &mut subfields {
                    let ty = subfield.ty.clone();
                    if amount == Amount::ZeroOrOne {
                        subfield.ty = quote_ty!(context, ::std::option::Option<$ty>);
                    } else {
                        subfield.ty = quote_ty!(context, ::std::vec::Vec<$ty>);
                    }
                }
                subfields
            },
            Specifier::NamedSequence(ref name, amount, _, _) => if amount == Amount::ZeroOrOne {
                field_spanned!(name, bool)
            } else {
                field_spanned!(name, usize)
            },
            _ => vec![],
        }
    }
}

impl ToExpr for Specifier {
    fn to_expr(&self, context: &mut ExtCtxt, span: Span) -> P<Expr> {
        macro_rules! expr {
            ($variant:expr, $($argument:expr), *) => ({
                let identifiers = &["easy_plugin", "Specifier", $variant];
                let arguments = vec![$($argument.to_expr(context, span)), *];
                utility::mk_expr_call(context, span, identifiers, arguments)
            });
        }

        match *self {
            Specifier::Attr(ref name) => expr!("Attr", name),
            Specifier::BinOp(ref name) => expr!("BinOp", name),
            Specifier::Block(ref name) => expr!("Block", name),
            Specifier::Delim(ref name) => expr!("Delim", name),
            Specifier::Expr(ref name) => expr!("Expr", name),
            Specifier::Ident(ref name) => expr!("Ident", name),
            Specifier::Item(ref name) => expr!("Item", name),
            Specifier::Lftm(ref name) => expr!("Lftm", name),
            Specifier::Lit(ref name) => expr!("Lit", name),
            Specifier::Meta(ref name) => expr!("Meta", name),
            Specifier::Pat(ref name) => expr!("Pat", name),
            Specifier::Path(ref name) => expr!("Path", name),
            Specifier::Stmt(ref name) => expr!("Stmt", name),
            Specifier::Ty(ref name) => expr!("Ty", name),
            Specifier::Tok(ref name) => expr!("Tok", name),
            Specifier::Tt(ref name) => expr!("Tt", name),
            Specifier::Specific(ref token) => expr!("Specific", token),
            Specifier::Delimited(delimiter, ref subspecification) =>
                expr!("Delimited", delimiter, subspecification),
            Specifier::Sequence(amount, ref separator, ref subspecification) =>
                expr!("Sequence", amount, separator, subspecification),
            Specifier::NamedSequence(ref name, amount, ref separator, ref subspecification) =>
                expr!("NamedSequence", name, amount, separator, subspecification),
        }
    }
}

//================================================
// Structs
//================================================

// Specification _________________________________

/// A sequence of specifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Specification(pub Vec<Specifier>);

impl Specification {
    //- Accessors --------------------------------

    /// Returns `Field`s that would initialize values matched by this specification.
    pub fn to_fields(&self, context: &mut ExtCtxt, span: Span, stack: &[Amount]) -> Vec<Field> {
        self.iter().flat_map(|s| s.to_fields(context, span, stack).into_iter()).collect()
    }

    /// Returns `StructField`s that could contain values matched by this specification.
    pub fn to_struct_fields(&self, context: &mut ExtCtxt, span: Span) -> Vec<StructField> {
        self.iter().flat_map(|s| s.to_struct_fields(context, span).into_iter()).collect()
    }
}

impl ToExpr for Specification {
    fn to_expr(&self, context: &mut ExtCtxt, span: Span) -> P<Expr> {
        let identifiers = &["easy_plugin", "Specification"];
        let arguments = vec![self.0.to_expr(context, span)];
        utility::mk_expr_call(context, span, identifiers, arguments)
    }
}

impl ops::Deref for Specification {
    type Target = [Specifier];

    fn deref(&self) -> &[Specifier] {
        &self.0[..]
    }
}

//================================================
// Functions
//================================================

/// Parses a named specifier or a sequence (e.g., `$a:expr` or `$($b:expr), *`).
fn parse_dollar<'i, I>(
    span: Span, tts: &mut TtsIterator<'i, I>, names: &mut HashSet<String>
) -> PluginResult<Specifier> where I: Iterator<Item=&'i TokenTree> {
    match try!(tts.expect()) {
        &TokenTree::Token(subspan, Token::Ident(ref ident)) => {
            let name = ident.name.as_str().to_string();
            if names.insert(name.clone()) {
                parse_named_specifier(tts, name)
            } else {
                subspan.to_error("duplicate named specifier")
            }
        },
        &TokenTree::Delimited(_, ref delimited) => parse_sequence(span, tts, &delimited.tts, names),
        invalid => invalid.to_error("expected named specifier or sequence"),
    }
}

/// Parses a named specifier (e.g., `$a:expr`).
fn parse_named_specifier<'i, I>(
    tts: &mut TtsIterator<'i, I>, name: String
) -> PluginResult<Specifier> where I: Iterator<Item=&'i TokenTree> {
    try!(tts.expect_specific_token(Token::Colon));
    match try!(tts.expect()) {
        &TokenTree::Delimited(subspan, ref delimited) => {
            let mut names = HashSet::new();
            let subspecification = try!(parse_specification_(subspan, &delimited.tts, &mut names));
            if !names.is_empty() {
                return subspan.to_error("named specifiers not allowed in named sequences");
            }
            let (amount, separator) = try!(parse_sequence_suffix(tts));
            Ok(Specifier::NamedSequence(name, amount, separator, subspecification))
        },
        &TokenTree::Token(subspan, Token::Ident(ref ident)) => match &*ident.name.as_str() {
            "attr" => Ok(Specifier::Attr(name)),
            "binop" => Ok(Specifier::BinOp(name)),
            "block" => Ok(Specifier::Block(name)),
            "delim" => Ok(Specifier::Delim(name)),
            "expr" => Ok(Specifier::Expr(name)),
            "ident" => Ok(Specifier::Ident(name)),
            "item" => Ok(Specifier::Item(name)),
            "lftm" => Ok(Specifier::Lftm(name)),
            "lit" => Ok(Specifier::Lit(name)),
            "meta" => Ok(Specifier::Meta(name)),
            "pat" => Ok(Specifier::Pat(name)),
            "path" => Ok(Specifier::Path(name)),
            "stmt" => Ok(Specifier::Stmt(name)),
            "ty" => Ok(Specifier::Ty(name)),
            "tok" => Ok(Specifier::Tok(name)),
            "tt" => Ok(Specifier::Tt(name)),
            _ => subspan.to_error("invalid named specifier type"),
        },
        invalid => invalid.to_error("expected named specifier type or sequence"),
    }
}

/// Parses the suffix of a sequence (e.g., the `, *` in `$($b:expr), *`).
fn parse_sequence_suffix<'i, I>(
    tts: &mut TtsIterator<'i, I>
) -> PluginResult<(Amount, Option<Token>)> where I: Iterator<Item=&'i TokenTree> {
    match try!(tts.expect_token("expected separator, `*`, or `+`")) {
        (_, Token::BinOp(BinOpToken::Plus)) => Ok((Amount::OneOrMore, None)),
        (_, Token::BinOp(BinOpToken::Star)) => Ok((Amount::ZeroOrMore, None)),
        (_, Token::Question) => Ok((Amount::ZeroOrOne, None)),
        (subspan, separator) => match try!(tts.expect_token("expected `*` or `+`")) {
            (_, Token::BinOp(BinOpToken::Plus)) => Ok((Amount::OneOrMore, Some(separator))),
            (_, Token::BinOp(BinOpToken::Star)) => Ok((Amount::ZeroOrMore, Some(separator))),
            _ => subspan.to_error("expected `*` or `+`"),
        },
    }
}

/// Parses a sequence (e.g., `$($b:expr), *`).
fn parse_sequence<'i, I>(
    span: Span, tts: &mut TtsIterator<'i, I>, subtts: &[TokenTree], names: &mut HashSet<String>
) -> PluginResult<Specifier> where I: Iterator<Item=&'i TokenTree> {
    let subspecification = try!(parse_specification_(span, subtts, names));
    let (amount, separator) = try!(parse_sequence_suffix(tts));
    Ok(Specifier::Sequence(amount, separator, subspecification))
}

/// Actually parses the supplied specification.
fn parse_specification_(
    span: Span, tts: &[TokenTree], names: &mut HashSet<String>
) -> PluginResult<Specification> {
    let mut tts = TtsIterator::new(tts.iter(), span, "unexpected end of specification");
    let mut specification = vec![];
    while let Some(tt) = tts.next() {
        match *tt {
            TokenTree::Token(_, Token::Dollar) =>
                specification.push(try!(parse_dollar(span, &mut tts, names))),
            TokenTree::Token(_, ref token) =>
                specification.push(Specifier::Specific(token.clone())),
            TokenTree::Delimited(subspan, ref delimited) => {
                let subspecification = try!(parse_specification_(subspan, &delimited.tts, names));
                specification.push(Specifier::Delimited(delimited.delim, subspecification));
            },
            _ => unreachable!(),
        }
    }
    Ok(Specification(specification))
}

/// Parses the supplied specification.
pub fn parse_specification(tts: &[TokenTree]) -> PluginResult<Specification> {
    let start = tts.iter().nth(0).map_or(DUMMY_SP, |s| s.get_span());
    let end = tts.iter().last().map_or(DUMMY_SP, |s| s.get_span());
    let span = Span { lo: start.lo, hi: end.hi, expn_id: start.expn_id };
    parse_specification_(span, tts, &mut HashSet::new())
}

#[doc(hidden)]
pub fn expand_parse_specification(
    context: &mut ExtCtxt, span: Span, arguments: &[TokenTree]
) -> Box<MacResult> {
    match parse_specification(arguments) {
        Ok(specification) => MacEager::expr(specification.to_expr(context, span)),
        Err((span, message)) => {
            context.span_err(span, &message);
            DummyResult::any(span)
        },
    }
}

//================================================
// Tests
//================================================

#[cfg(test)]
mod tests {
    use super::*;

    use syntax::parse;
    use syntax::ast::{TokenTree};
    use syntax::parse::{ParseSess};
    use syntax::parse::token::{DelimToken, Token};

    fn with_tts<F>(source: &str, f: F) where F: Fn(Vec<TokenTree>) {
        let session = ParseSess::new();
        let source = source.into();
        let mut parser = parse::new_parser_from_source_str(&session, vec![], "".into(), source);
        f(parser.parse_all_token_trees().unwrap());
    }

    #[test]
    fn test_parse_specification() {
        with_tts("", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![]);
        });

        with_tts("$a:attr $b:tt", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![
                Specifier::Attr("a".into()),
                Specifier::Tt("b".into()),
            ]);
        });

        with_tts("$($a:ident $($b:ident)*), + $($c:ident)?", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![
                Specifier::Sequence(Amount::OneOrMore, Some(Token::Comma), spec![
                    Specifier::Ident("a".into()),
                    Specifier::Sequence(Amount::ZeroOrMore, None, spec![
                        Specifier::Ident("b".into()),
                    ]),
                ]),
                Specifier::Sequence(Amount::ZeroOrOne, None, spec![
                    Specifier::Ident("c".into()),
                ]),
            ]);
        });

        with_tts("$a:(A)* $b:(B), + $c:(C)?", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![
                Specifier::NamedSequence("a".into(), Amount::ZeroOrMore, None, spec![
                    Specifier::specific_ident("A"),
                ]),
                Specifier::NamedSequence("b".into(), Amount::OneOrMore, Some(Token::Comma), spec![
                    Specifier::specific_ident("B"),
                ]),
                Specifier::NamedSequence("c".into(), Amount::ZeroOrOne, None, spec![
                    Specifier::specific_ident("C"),
                ]),
            ]);
        });

        with_tts("() [$a:ident] {$b:ident $c:ident}", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![
                Specifier::Delimited(DelimToken::Paren, spec![]),
                Specifier::Delimited(DelimToken::Bracket, spec![
                    Specifier::Ident("a".into()),
                ]),
                Specifier::Delimited(DelimToken::Brace, spec![
                    Specifier::Ident("b".into()),
                    Specifier::Ident("c".into()),
                ]),
            ]);
        });

        with_tts("~ foo 'bar", |tts| {
            assert_eq!(parse_specification(&tts).unwrap(), spec![
                Specifier::Specific(Token::Tilde),
                Specifier::specific_ident("foo"),
                Specifier::specific_lftm("'bar"),
            ]);
        });
    }
}
