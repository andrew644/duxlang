//! The shape of a sum type: which variants it has and what each one carries.
//!
//! `Option` and `Result` live here because the universe scope predeclares them
//! as ordinary enums (D7): they are entries in the same table a program's own
//! `enum` declarations go into, and nothing below this module knows they are
//! special. A program that declares its own `Option` overwrites the entry, just
//! as it may declare its own `make`.

use dux_ast::{EnumDecl, Field, Type, Variant, VariantBody};
use dux_source::Span;

/// What a variant carries, in the source's own spellings: a variant of a
/// generic enum is resolved once the instantiation is known.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    Empty,
    Tuple(Vec<Type>),
    Named(Vec<(String, Type)>),
}

impl Payload {
    fn from_variant(variant: &Variant) -> Payload {
        match &variant.body {
            VariantBody::Empty => Payload::Empty,
            VariantBody::Tuple(types) => Payload::Tuple(types.clone()),
            VariantBody::Named(fields) => Payload::Named(
                fields
                    .iter()
                    .map(|field: &Field| (field.name.name.clone(), field.ty.clone()))
                    .collect(),
            ),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Payload::Empty)
    }
}

#[derive(Clone, Debug)]
pub struct Shape {
    pub name: String,
    pub params: Vec<String>,
    pub variants: Vec<(String, Payload)>,
}

impl Shape {
    pub fn from_decl(decl: &EnumDecl) -> Shape {
        Shape {
            name: decl.name.name.clone(),
            params: decl
                .type_params
                .iter()
                .map(|param| param.name.name.clone())
                .collect(),
            variants: decl
                .variants
                .iter()
                .map(|variant| (variant.name.name.clone(), Payload::from_variant(variant)))
                .collect(),
        }
    }

    /// The variant of that name, with its index in declaration order.
    pub fn variant(&self, name: &str) -> Option<(usize, &Payload)> {
        self.variants
            .iter()
            .position(|(known, _)| known == name)
            .map(|index| (index, &self.variants[index].1))
    }

    pub fn has_variant(&self, name: &str) -> bool {
        self.variants.iter().any(|(known, _)| known == name)
    }

    /// Does this enum take exactly that many arguments?
    pub fn takes(&self, args: usize) -> bool {
        args == self.params.len()
    }
}

/// The universe scope's own sum types, spelled exactly as DESIGN §4.5 writes
/// them: `enum Option<T> { Some(T), None }` and `enum Result<T, E> { Ok(T), Err(E) }`.
pub fn prelude() -> Vec<Shape> {
    vec![
        Shape {
            name: "Option".to_string(),
            params: vec!["T".to_string()],
            variants: vec![
                ("Some".to_string(), Payload::Tuple(vec![named("T")])),
                ("None".to_string(), Payload::Empty),
            ],
        },
        Shape {
            name: "Result".to_string(),
            params: vec!["T".to_string(), "E".to_string()],
            variants: vec![
                ("Ok".to_string(), Payload::Tuple(vec![named("T")])),
                ("Err".to_string(), Payload::Tuple(vec![named("E")])),
            ],
        },
    ]
}

fn named(name: &str) -> Type {
    Type::Named {
        name: name.to_string(),
        args: Vec::new(),
        span: Span::default(),
    }
}
