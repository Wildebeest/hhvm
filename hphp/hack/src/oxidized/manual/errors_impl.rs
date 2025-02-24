// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use std::cmp::Ordering;

use ocamlrep::rc::RcOc;

use crate::errors::*;
use crate::message::Message;
use crate::pos::Pos;
use crate::pos_or_decl::PosOrDecl;
use crate::quickfix::Edit;
use crate::quickfix::QfPos;
use crate::quickfix::Quickfix;
use crate::user_error::UserError;

impl<PP, P> UserError<PP, P> {
    pub fn new(
        code: ErrorCode,
        claim: Message<PP>,
        reasons: Vec<Message<P>>,
        quickfixes: Vec<Quickfix<PP>>,
    ) -> Self {
        Self {
            code,
            claim,
            reasons,
            quickfixes,
            is_fixmed: false,
        }
    }

    pub fn pos(&self) -> &PP {
        let Message(pos, _msg) = &self.claim;
        pos
    }

    pub fn msg(&self) -> &bstr::BStr {
        let Message(_, msg) = &self.claim;
        msg.as_ref()
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }
}

pub trait FileOrd {
    fn cmp_file(&self, other: &Self) -> Ordering;
}

impl FileOrd for Pos {
    fn cmp_file(&self, other: &Self) -> Ordering {
        self.filename().cmp(other.filename())
    }
}

impl<PP: Ord + FileOrd, P: Ord + FileOrd> Ord for UserError<PP, P> {
    // Intended to match the implementation of `compare` in `Errors.sort` in OCaml.
    fn cmp(&self, other: &Self) -> Ordering {
        let Self {
            code: self_code,
            claim: self_claim,
            reasons: self_reasons,
            quickfixes: _,
            is_fixmed: _,
        } = self;
        let Self {
            code: other_code,
            claim: other_claim,
            reasons: other_reasons,
            quickfixes: _,
            is_fixmed: _,
        } = other;
        let Message(self_pos, self_msg) = self_claim;
        let Message(other_pos, other_msg) = other_claim;
        // The primary sort order is by file of the claim (main message).
        self_pos
            .cmp_file(other_pos)
            // If the files are the same, sort by phase.
            .then(((*self_code / 1000) as isize).cmp(&((*other_code / 1000) as isize)))
            // If the phases are the same, sort by position.
            .then(self_pos.cmp(other_pos))
            // If the positions are the same, sort by claim message text.
            .then(self_msg.cmp(other_msg))
            // If the claim message text is the same, compare the reason
            // messages (which contain further explanation for the error
            // reported in the claim message).
            .then(self_reasons.iter().cmp(other_reasons.iter()))
    }
}

impl<PP: Ord + FileOrd, P: Ord + FileOrd> PartialOrd for UserError<PP, P> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl UserError<Pos, PosOrDecl> {
    /// Return a struct with a `std::fmt::Display` implementation that displays
    /// the error in the Errors.Plain format produced by OCaml Errors.to_string.
    pub fn display_plain(&self) -> DisplayPlain<'_> {
        DisplayPlain(self)
    }
}

pub struct DisplayPlain<'a>(&'a UserError<Pos, PosOrDecl>);

impl<'a> std::fmt::Display for DisplayPlain<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let UserError {
            code,
            claim,
            reasons,
            quickfixes: _,
            is_fixmed: _,
        } = self.0;
        let Message(pos, msg) = claim;
        let code = DisplayErrorCode(*code);
        write!(f, "{}\n{} ({})", pos.string(), msg, code)?;
        for Message(pos, msg) in reasons.iter() {
            write!(f, "\n  {}\n  {}", pos.string(), msg)?;
        }
        Ok(())
    }
}

fn error_kind(error_code: ErrorCode) -> &'static str {
    match error_code / 1000 {
        1 => "Parsing",
        2 => "Naming",
        3 => "NastCheck",
        4 => "Typing",
        5 => "Lint",
        8 => "Init",
        _ => "Other",
    }
}

struct DisplayErrorCode(ErrorCode);

impl std::fmt::Display for DisplayErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{:04}]", error_kind(self.0), self.0)
    }
}

impl Naming {
    pub fn fd_name_already_bound(p: Pos) -> Error {
        UserError::new(
            Self::FdNameAlreadyBound as isize,
            Message(p, "Field name already bound".into()),
            vec![],
            vec![],
        )
    }

    pub fn bad_builtin_type(p: Pos, name: &str, correct_name: &str) -> Error {
        UserError::new(
            Self::InvalidBuiltinType as isize,
            Message(
                p.clone(),
                format!("No such type `{}`, did you mean `{}`?", name, correct_name).into(),
            ),
            vec![],
            vec![Quickfix {
                title: format!("Change to `{}`", correct_name),
                edits: vec![Edit(correct_name.into(), QfPos::Qpos(p))],
            }],
        )
    }

    pub fn method_needs_visibility(first_token_p: Pos, name_p: Pos) -> Error {
        // Create a zero width position at the start of the first token.
        let file = RcOc::clone(first_token_p.filename_rc_ref());
        let mut p_span = first_token_p.to_raw_span();
        p_span.end = p_span.start;
        let fix_pos = Pos::from_raw_span(file, p_span);

        UserError::new(
            Self::MethodNeedsVisibility as isize,
            Message(
                name_p,
                "Methods need to be marked `public`, `private`, or `protected`.".into(),
            ),
            vec![],
            vec![
                Quickfix {
                    title: "Add `private` modifier".into(),
                    edits: vec![Edit("private ".into(), QfPos::Qpos(fix_pos.clone()))],
                },
                Quickfix {
                    title: "Add `protected` modifier".into(),
                    edits: vec![Edit("protected ".into(), QfPos::Qpos(fix_pos.clone()))],
                },
                Quickfix {
                    title: "Add `public` modifier".into(),
                    edits: vec![Edit("public ".into(), QfPos::Qpos(fix_pos))],
                },
            ],
        )
    }

    pub fn unsupported_trait_use_as(p: Pos) -> Error {
        UserError::new(
            Self::UnsupportedTraitUseAs as isize,
            Message(
                p,
                "Trait use as is a PHP feature that is unsupported in Hack".into(),
            ),
            vec![],
            vec![],
        )
    }

    pub fn unsupported_instead_of(p: Pos) -> Error {
        UserError::new(
            Self::UnsupportedInsteadOf as isize,
            Message(
                p,
                "insteadof is a PHP feature that is unsupported in Hack".into(),
            ),
            vec![],
            vec![],
        )
    }
}

impl NastCheck {
    pub fn not_abstract_without_typeconst(p: Pos) -> Error {
        UserError::new(
            Self::NotAbstractWithoutTypeconst as isize,
            Message(
                p,
                "This type constant is not declared as abstract, it must have an assigned type"
                    .into(),
            ),
            vec![],
            vec![],
        )
    }

    pub fn multiple_xhp_category(p: Pos) -> Error {
        UserError::new(
            Self::MultipleXhpCategory as isize,
            Message(
                p,
                "XHP classes can only contain one category declaration".into(),
            ),
            vec![],
            vec![],
        )
    }

    pub fn partially_abstract_typeconst_definition(p: Pos, kind: &str) -> Error {
        UserError::new(
            Self::PartiallyAbstractTypeconstDefinition as isize,
            Message(
                p,
                format!(
                    "`{}` constraints are only legal on abstract type constants",
                    kind
                )
                .into(),
            ),
            vec![],
            vec![],
        )
    }
}

#[derive(Debug)]
pub struct ParseFormatError;

impl std::str::FromStr for Format {
    type Err = ParseFormatError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "context" => Ok(Self::Context),
            "raw" => Ok(Self::Raw),
            "highlighted" => Ok(Self::Highlighted),
            "plain" => Ok(Self::Plain),
            _ => Err(ParseFormatError),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context => "context".fmt(f),
            Self::Raw => "raw".fmt(f),
            Self::Highlighted => "highlighted".fmt(f),
            Self::Plain => "plain".fmt(f),
        }
    }
}

impl std::error::Error for ParseFormatError {}

impl std::fmt::Display for ParseFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "Unrecognized error format".fmt(f)
    }
}
