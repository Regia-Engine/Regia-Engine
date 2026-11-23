// SPDX-License-Identifier: MPL-2.0
//! A nesting bound for binary decodes.
//!
//! postcard has no recursion limit, and a type decoded through its own serde implementation
//! recurses on fields that reflection cannot see (an opaque type, or a `#[reflect(ignore)]`
//! field). [`Limited`] wraps any [`Deserializer`] and counts every value nested inside another
//! (sequence and map items, struct fields, enum payloads, `Some`, newtypes), failing with an error
//! past a fixed depth instead of overflowing the stack. The wrapper only forwards calls; it never
//! changes the decoded data.

use std::fmt;

use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor,
};

/// Deepest value nesting a binary decode accepts: the same bound RON applies to text.
pub(super) const MAX_BINARY_NESTING: u32 = 128;

/// A deserializer that allows at most `left` more levels of nesting below the value it reads.
pub(super) struct Limited<D> {
    inner: D,
    left: u32,
}

impl<D> Limited<D> {
    /// Wraps `inner` with the full [`MAX_BINARY_NESTING`] budget.
    pub(super) fn new(inner: D) -> Self {
        Self {
            inner,
            left: MAX_BINARY_NESTING,
        }
    }
}

fn too_deep<E: de::Error>() -> E {
    E::custom(format_args!(
        "nesting deeper than {MAX_BINARY_NESTING} levels in binary data"
    ))
}

/// A seed for a value one level below the current one.
struct Child<S> {
    seed: S,
    left: u32,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Child<S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<S::Value, D::Error> {
        let Some(left) = self.left.checked_sub(1) else {
            return Err(too_deep());
        };
        self.seed.deserialize(Limited {
            inner: deserializer,
            left,
        })
    }
}

/// A seed read at the current level (an enum's variant tag).
struct Same<S> {
    seed: S,
    left: u32,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Same<S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<S::Value, D::Error> {
        self.seed.deserialize(Limited {
            inner: deserializer,
            left: self.left,
        })
    }
}

/// Wraps a visitor so the accessors it receives hand out [`Child`] seeds.
struct Wrap<V> {
    visitor: V,
    left: u32,
}

macro_rules! forward_visit {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(
            fn $method<E: de::Error>(self, v: $ty) -> Result<Self::Value, E> {
                self.visitor.$method(v)
            }
        )*
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Wrap<V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.visitor.expecting(f)
    }

    forward_visit! {
        visit_bool(bool), visit_i8(i8), visit_i16(i16), visit_i32(i32), visit_i64(i64), visit_i128(i128),
        visit_u8(u8), visit_u16(u16), visit_u32(u32), visit_u64(u64), visit_u128(u128),
        visit_f32(f32), visit_f64(f64), visit_char(char), visit_str(&str), visit_string(String),
        visit_bytes(&[u8]), visit_byte_buf(Vec<u8>),
    }

    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        self.visitor.visit_borrowed_str(v)
    }

    fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
        self.visitor.visit_borrowed_bytes(v)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let Some(left) = self.left.checked_sub(1) else {
            return Err(too_deep());
        };
        self.visitor.visit_some(Limited {
            inner: deserializer,
            left,
        })
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let Some(left) = self.left.checked_sub(1) else {
            return Err(too_deep());
        };
        self.visitor.visit_newtype_struct(Limited {
            inner: deserializer,
            left,
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_seq(Wrap {
            visitor: seq,
            left: self.left,
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_map(Wrap {
            visitor: map,
            left: self.left,
        })
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_enum(Wrap {
            visitor: data,
            left: self.left,
        })
    }
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Wrap<A> {
    type Error = A::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, A::Error> {
        self.visitor.next_element_seed(Child {
            seed,
            left: self.left,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        self.visitor.size_hint()
    }
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for Wrap<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, A::Error> {
        self.visitor.next_key_seed(Child {
            seed,
            left: self.left,
        })
    }

    fn next_value_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<T::Value, A::Error> {
        self.visitor.next_value_seed(Child {
            seed,
            left: self.left,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        self.visitor.size_hint()
    }
}

impl<'de, A: EnumAccess<'de>> EnumAccess<'de> for Wrap<A> {
    type Error = A::Error;
    type Variant = Wrap<A::Variant>;

    fn variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<(T::Value, Self::Variant), A::Error> {
        let left = self.left;
        let (value, variant) = self.visitor.variant_seed(Same { seed, left })?;
        Ok((
            value,
            Wrap {
                visitor: variant,
                left,
            },
        ))
    }
}

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Wrap<A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.visitor.unit_variant()
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.visitor.newtype_variant_seed(Child {
            seed,
            left: self.left,
        })
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, A::Error> {
        self.visitor.tuple_variant(
            len,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        self.visitor.struct_variant(
            fields,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }
}

macro_rules! forward_deserialize {
    ($de:lifetime; $($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<$de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                self.inner.$method(Wrap { visitor, left: self.left })
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Limited<D> {
    type Error = D::Error;

    forward_deserialize! {
        'de;
        deserialize_any, deserialize_bool, deserialize_i8, deserialize_i16, deserialize_i32,
        deserialize_i64, deserialize_i128, deserialize_u8, deserialize_u16, deserialize_u32,
        deserialize_u64, deserialize_u128, deserialize_f32, deserialize_f64, deserialize_char,
        deserialize_str, deserialize_string, deserialize_bytes, deserialize_byte_buf,
        deserialize_option, deserialize_unit, deserialize_seq, deserialize_map,
        deserialize_identifier, deserialize_ignored_any,
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_unit_struct(
            name,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_newtype_struct(
            name,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_tuple(
            len,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_tuple_struct(
            name,
            len,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_struct(
            name,
            fields,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.inner.deserialize_enum(
            name,
            variants,
            Wrap {
                visitor,
                left: self.left,
            },
        )
    }

    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    enum Chain {
        Nil,
        Cons(Box<Chain>),
    }

    fn chain(links: usize) -> Chain {
        (0..links).fold(Chain::Nil, |inner, _| Chain::Cons(Box::new(inner)))
    }

    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
        let mut deserializer = postcard::Deserializer::from_bytes(bytes);
        T::deserialize(Limited::new(&mut deserializer))
    }

    #[test]
    fn the_bound_is_exactly_max_binary_nesting_levels() -> Result<(), postcard::Error> {
        // `links` newtype payloads nest `links` levels below the top value.
        let at_limit = postcard::to_stdvec(&chain(MAX_BINARY_NESTING as usize))?;
        assert_eq!(decode::<Chain>(&at_limit)?, chain(MAX_BINARY_NESTING as usize));
        let past = postcard::to_stdvec(&chain(MAX_BINARY_NESTING as usize + 1))?;
        assert!(decode::<Chain>(&past).is_err());
        Ok(())
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    enum Shape {
        Unit,
        New(u8),
        Tuple(i16, String),
        Struct { a: Option<u64>, b: Vec<f32> },
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Unit;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Wrapper(u32);

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Everything {
        flag: bool,
        ints: (i8, i16, i32, i64, i128),
        uints: (u8, u16, u32, u64, u128),
        floats: (f32, f64),
        c: char,
        text: String,
        bytes: Vec<u8>,
        none: Option<String>,
        some: Option<Option<u8>>,
        unit: (),
        unit_struct: Unit,
        wrapper: Wrapper,
        array: [u16; 3],
        map: BTreeMap<String, Vec<Shape>>,
        shapes: Vec<Shape>,
    }

    #[test]
    fn the_wrapper_decodes_every_serde_shape_unchanged() -> Result<(), postcard::Error> {
        let value = Everything {
            flag: true,
            ints: (-1, -300, -70_000, i64::MIN, i128::MIN),
            uints: (255, 65_535, u32::MAX, u64::MAX, u128::MAX),
            floats: (f32::NAN.copysign(-1.0), -0.0),
            c: 'ß',
            text: "door \"north\"".into(),
            bytes: vec![0, 1, 255],
            none: None,
            some: Some(None),
            unit: (),
            unit_struct: Unit,
            wrapper: Wrapper(7),
            array: [1, 2, 3],
            map: BTreeMap::from([
                ("a".to_string(), vec![Shape::Unit, Shape::New(3)]),
                ("b".to_string(), vec![]),
            ]),
            shapes: vec![
                Shape::Tuple(-5, "t".into()),
                Shape::Struct {
                    a: Some(9),
                    b: vec![1.5, -2.0],
                },
            ],
        };
        let bytes = postcard::to_stdvec(&value)?;
        let plain: Everything = postcard::from_bytes(&bytes)?;
        let limited: Everything = decode(&bytes)?;
        // Compare by re-encoding: the NaN field makes `PartialEq` false for equal data.
        assert_eq!(postcard::to_stdvec(&limited)?, bytes);
        assert_eq!(postcard::to_stdvec(&plain)?, bytes);
        assert_eq!(limited.shapes, value.shapes);
        assert_eq!(limited.map, value.map);
        Ok(())
    }
}
