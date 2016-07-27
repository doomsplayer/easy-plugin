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

use std::cell::{RefCell};
use std::marker::{PhantomData};

use syntax::ext::tt::transcribe;
use syntax::ast::*;
use syntax::codemap::{DUMMY_SP, Span, Spanned};
use syntax::errors::{DiagnosticBuilder, FatalError, Level};
use syntax::errors::emitter::{Emitter};
use syntax::ext::base::{ExtCtxt};
use syntax::ext::build::{AstBuilder};
use syntax::parse::{ParseSess, PResult};
use syntax::parse::lexer::{Reader, TokenAndSpan};
use syntax::parse::parser::{Parser, PathStyle};
use syntax::parse::token::{Token};
use syntax::ptr::{P};
use syntax::tokenstream::{TokenTree};

use super::{PluginResult};

//================================================
// Macros
//================================================

// parse! _______________________________________

/// Defines a parsing method for `TransactionParser` that parses a particular AST entity.
macro_rules! parse {
    ($name:ident($($argument:expr), *)$(.$method:ident())*, $description:expr, $ty:ty) => {
        pub fn $name(&mut self, name: &str) -> PluginResult<$ty> {
            self.parse_expected($description, name, |p| p.$name($($argument), *))
        }
    };

    (OPTION: $name:ident($($argument:expr), *)$(.$method:ident())*, $description:expr, $ty:ty) => {
        pub fn $name(&mut self, name: &str) -> PluginResult<$ty> {
            let span = self.get_span();
            match self.apply(|p| p.$name($($argument), *)) {
                Ok(Some(value)) => return Ok(value),
                Err(mut db) => db.cancel(),
                _ => { },
            }
            span.to_error(format!("expected {}: '{}'", $description, name))
        }
    };
}

// to_error! _____________________________________

/// Defines a `ToError` implementation for the supplied type.
macro_rules! to_error {
    ($ty:ty) => (
        impl<T, S: Into<String>> ToError<T, S> for $ty {
            fn to_error(&self, message: S) -> PluginResult<T> {
                Err((self.span, message.into()))
            }
        }
    );
}

//================================================
// Traits
//================================================

// PluginResultExt _______________________________

/// Extends `PluginResult<T>`.
pub trait PluginResultExt<T> {
    /// Returns this `PluginResult<T>` with a different span if it is an `Err`.
    fn map_err_span(self, span: Span) -> PluginResult<T>;

    /// Returns this `PluginResult<T>` with a different message if it is an `Err`.
    fn map_err_message<S: Into<String>>(self, message: S) -> PluginResult<T>;
}

impl<T> PluginResultExt<T> for PluginResult<T> {
    fn map_err_span(self, span: Span) -> PluginResult<T> {
        self.map_err(|(_, m)| (span, m))
    }

    fn map_err_message<S: Into<String>>(self, message: S) -> PluginResult<T> {
        self.map_err(|(s, _)| (s, message.into()))
    }
}

// ToError _______________________________________

/// A type that can be extended into a `PluginResult<T>`.
pub trait ToError<T, S> where S: Into<String> {
    /// Returns an `Err` value with the span of this value and the supplied message.
    fn to_error(&self, message: S) -> PluginResult<T>;
}

impl<T, S: Into<String>> ToError<T, S> for Span {
    fn to_error(&self, message: S) -> PluginResult<T> {
        Err((*self, message.into()))
    }
}

impl<T, S: Into<String>> ToError<T, S> for TokenTree {
    fn to_error(&self, message: S) -> PluginResult<T> {
        Err((self.get_span(), message.into()))
    }
}

impl<T, U, S: Into<String>> ToError<T, S> for Spanned<U> {
    fn to_error(&self, message: S) -> PluginResult<T> {
        Err((self.span, message.into()))
    }
}

to_error!(Block);
to_error!(Expr);
to_error!(Item);
to_error!(Pat);
to_error!(Path);
to_error!(Stmt);
to_error!(Ty);

//================================================
// Structs
//================================================

// SaveEmitter ___________________________________

/// The most recent fatal error, if any.
thread_local! { static ERROR: RefCell<Option<(Span, String)>> = RefCell::default() }

/// A diagnostic emitter that saves fatal errors to a thread local variable.
pub struct SaveEmitter;

impl Emitter for SaveEmitter {
    fn emit(&mut self, builder: &DiagnosticBuilder) {
        if builder.level == Level::Fatal {
            let span = builder.span.primary_span().unwrap_or(DUMMY_SP);
            ERROR.with(|e| *e.borrow_mut() = Some((span, builder.message.clone())));
        }
    }
}

// TokenReader ___________________________________

/// A token reader which wraps a `Vec<TokenAndSpan>`.
#[derive(Clone)]
struct TokenReader<'s> {
    session: &'s ParseSess,
    tokens: Vec<TokenAndSpan>,
    index: usize,
}

impl<'s> TokenReader<'s> {
    //- Constructors -----------------------------

    fn new(session: &'s ParseSess, tokens: Vec<TokenAndSpan>) -> TokenReader<'s> {
        TokenReader { session: session, tokens: tokens, index: 0 }
    }
}

impl<'s> Reader for TokenReader<'s> {
    fn is_eof(&self) -> bool {
        self.index + 1 == self.tokens.len()
    }

    fn try_next_token(&mut self) -> Result<TokenAndSpan, ()> {
        Ok(self.next_token())
    }

    fn fatal(&self, message: &str) -> FatalError {
        self.session.span_diagnostic.span_fatal(self.peek().sp, message)
    }

    fn err(&self, message: &str) {
        self.session.span_diagnostic.span_err(self.peek().sp, message);
    }

    fn emit_fatal_errors(&mut self) { }

    fn peek(&self) -> TokenAndSpan {
        self.tokens[self.index].clone()
    }

    fn next_token(&mut self) -> TokenAndSpan {
        let next = self.tokens[self.index].clone();
        if !self.is_eof() {
            self.index += 1;
        }
        next
    }
}

// TransactionParser _____________________________

/// A wrapper around a `Parser` which allows for rolling back parsing actions.
pub struct TransactionParser<'s> {
    session: &'s ParseSess,
    tokens: Vec<TokenAndSpan>,
    start: usize,
    position: usize,
}

impl<'s> TransactionParser<'s> {
    //- Constructors -----------------------------

    pub fn new(session: &'s ParseSess, tts: &[TokenTree]) -> TransactionParser<'s> {
        let mut parser = TransactionParser {
            session: session, tokens: vec![], start: 0, position: 0
        };

        // Generate `TokenAndSpan`s from the supplied `TokenTree`s.
        let handler = &session.span_diagnostic;
        let mut reader = transcribe::new_tt_reader(handler, None, None, tts.into());
        while !reader.is_eof() {
            parser.tokens.push(reader.next_token());
        }
        parser.tokens.push(reader.next_token());

        parser
    }

    //- Accessors --------------------------------

    /// Returns the span of current token.
    pub fn get_span(&self) -> Span {
        if self.position == self.tokens.len() {
            self.tokens.get(self.tokens.len().saturating_sub(1)).expect("expected span").sp
        } else {
            self.tokens.get(self.position).expect("expected span").sp
        }
    }

    /// Returns the span of the last token processed.
    pub fn get_last_span(&self) -> Span {
        if self.position == self.tokens.len() {
            self.tokens.get(self.tokens.len().saturating_sub(1)).expect("expected span").sp
        } else {
            self.tokens.get(self.position.saturating_sub(1)).expect("expected span").sp
        }
    }

    /// Returns whether this parser has successfully processed all of its tokens.
    pub fn is_empty(&self) -> bool {
        self.position == self.tokens.len() - 1
    }

    //- Mutators ---------------------------------

    /// Sets the saved position to the current position.
    pub fn save(&mut self) {
        self.start = self.position;
    }

    /// Sets the current position to the saved position.
    pub fn rollback(&mut self) {
        self.position = self.start;
    }

    /// Applies an action to this parser, returning the result of the action.
    fn apply<T, F: FnOnce(&mut Parser<'s>) -> T>(&mut self, f: F) -> T {
        // Construct a temporary `Parser` that reads from the unprocessed `TokenAndSpan`s.
        let reader = Box::new(TokenReader::new(self.session, self.tokens[self.position..].into()));
        let mut parser = Parser::new(self.session, vec![], reader);

        // Apply the action, incrementing the position by how many `TokenAndSpan`s were read.
        let result = f(&mut parser);
        self.position += parser.tokens_consumed;
        result
    }

    pub fn bump_and_get(&mut self) -> Token {
        self.apply(|p| p.bump_and_get())
    }

    pub fn eat(&mut self, token: &Token) -> bool {
        self.apply(|p| p.eat(token))
    }

    /// Applies a parsing action to this parser, returning the result of the action.
    ///
    /// If the parsing action fails, the reported error is the last fatal parsing error.
    pub fn parse<T, F: FnOnce(&mut Parser<'s>) -> PResult<'s, T>>(
        &mut self, f: F
    ) -> PluginResult<T> {
        self.apply(f).map_err(|mut db| {
            db.cancel();
            ERROR.with(|e| e.borrow().clone().unwrap_or_else(|| (DUMMY_SP, "no error".into())))
        })
    }

    /// Applies a parsing action to this parser, returning the result of the action.
    ///
    /// If the parsing action fails, the reported error describes what kind of AST entity was
    /// expected.
    fn parse_expected<T, F: FnOnce(&mut Parser<'s>) -> PResult<'s, T>>(
        &mut self, description: &str, name: &str, f: F
    ) -> PluginResult<T> {
        let span = self.get_span();
        self.apply(f).map_err(|mut db| {
            db.cancel();
            (span, format!("expected {}: '{}'", description, name))
        })
    }

    parse!(parse_attribute(true), "attribute", Attribute);
    parse!(parse_block(), "block", P<Block>);
    parse!(parse_expr(), "expression", P<Expr>);
    parse!(parse_ident(), "identifier", Ident);
    parse!(OPTION: parse_item(), "item", P<Item>);
    parse!(parse_lifetime(), "lifetime", Lifetime);
    parse!(parse_lit(), "literal", Lit);
    parse!(parse_meta_item(), "meta item", P<MetaItem>);
    parse!(parse_pat(), "pattern", P<Pat>);
    parse!(parse_path(PathStyle::Type), "path", Path);
    parse!(OPTION: parse_stmt(), "statement", Stmt);
    parse!(parse_ty(), "type", P<Ty>);
    parse!(parse_token_tree(), "token tree", TokenTree);
}

// TtsIterator ___________________________________

/// A token tree iterator which returns an error when the output does not match expectations.
pub struct TtsIterator<'i, I> where I: Iterator<Item=&'i TokenTree> {
    pub error: (Span, String),
    pub iterator: I,
    _marker: PhantomData<&'i ()>,
}

impl<'i, I> TtsIterator<'i, I> where I: Iterator<Item=&'i TokenTree> {
    //- Constructors -----------------------------

    pub fn new(iterator: I, span: Span, message: &str) -> TtsIterator<'i, I> {
        TtsIterator { error: (span, message.into()), iterator: iterator, _marker: PhantomData }
    }

    //- Mutators ---------------------------------

    pub fn expect(&mut self) -> PluginResult<&'i TokenTree> {
        self.iterator.next().ok_or_else(|| self.error.clone())
    }

    pub fn expect_token(&mut self, description: &str) -> PluginResult<(Span, Token)> {
        self.expect().and_then(|tt| {
            match *tt {
                TokenTree::Token(span, ref token) => Ok((span, token.clone())),
                _ => tt.to_error(format!("expected {}", description)),
            }
        })
    }

    pub fn expect_specific_token(&mut self, token: Token) -> PluginResult<()> {
        let description = Parser::token_to_string(&token);
        self.expect_token(&description).and_then(|(s, t)| {
            if mtwt_eq(&t, &token) {
                Ok(())
            } else {
                s.to_error(format!("expected {}", description))
            }
        })
    }
}

impl<'i, I> Iterator for TtsIterator<'i, I> where I: Iterator<Item=&'i TokenTree> {
    type Item = &'i TokenTree;

    fn next(&mut self) -> Option<&'i TokenTree> {
        self.iterator.next()
    }
}

//================================================
// Functions
//================================================

pub fn mk_expr_call(context: &ExtCtxt, span: Span, idents: &[&str], args: Vec<P<Expr>>) -> P<Expr> {
    context.expr_call_global(span, mk_path(context, idents), args)
}

pub fn mk_path(context: &ExtCtxt, idents: &[&str]) -> Vec<Ident> {
    idents.iter().map(|i| context.ident_of(i)).collect()
}

pub fn mtwt_eq(left: &Token, right: &Token) -> bool {
    match (left, right) {
        (&Token::Ident(left), &Token::Ident(right)) |
        (&Token::Lifetime(left), &Token::Lifetime(right)) =>
            left.name.as_str() == right.name.as_str(),
        _ => left == right,
    }
}
