// SPDX-License-Identifier: MPL-2.0
//! Splits a serialized row struct into one RON text per field, in declaration order.
//!
//! A row is written through serde: [`row_fields`] runs the row's `Serialize` impl against a
//! serializer that accepts only a struct and, for each field, writes the field's value with
//! `ron::to_string`. The texts are exactly what a `.table.ron` file holds for each field, so a cell
//! read back with the RON parser gives the same value (enums keep their variant names, nested
//! structs and lists keep their shape).

use serde::Serialize;
use serde::ser::{self, Impossible};

/// Why a row could not be split into fields.
#[derive(Debug)]
pub(crate) struct CaptureError(pub(crate) String);

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CaptureError {}

impl ser::Error for CaptureError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

/// The fields of `row` as `(name, RON text)`; fields skipped by `skip_serializing_if` are absent.
pub(crate) fn row_fields<R: Serialize + ?Sized>(
    row: &R,
) -> Result<Vec<(&'static str, String)>, CaptureError> {
    row.serialize(FieldCapture)
}

struct FieldCapture;

/// Collects the fields of the one struct.
pub(crate) struct FieldList(Vec<(&'static str, String)>);

impl ser::SerializeStruct for FieldList {
    type Ok = Vec<(&'static str, String)>;
    type Error = CaptureError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), CaptureError> {
        let text = ron::to_string(value).map_err(|e| CaptureError(format!("field `{key}`: {e}")))?;
        self.0.push((key, text));
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, CaptureError> {
        Ok(self.0)
    }
}

fn not_a_struct<T>() -> Result<T, CaptureError> {
    Err(CaptureError(
        "a table row must serialize as a struct with named fields".to_owned(),
    ))
}

macro_rules! reject {
    ($($name:ident($($arg:ty),*);)*) => {
        $(fn $name(self, $(_: $arg),*) -> Result<Self::Ok, CaptureError> { not_a_struct() })*
    };
}

impl ser::Serializer for FieldCapture {
    type Ok = Vec<(&'static str, String)>;
    type Error = CaptureError;
    type SerializeSeq = Impossible<Self::Ok, CaptureError>;
    type SerializeTuple = Impossible<Self::Ok, CaptureError>;
    type SerializeTupleStruct = Impossible<Self::Ok, CaptureError>;
    type SerializeTupleVariant = Impossible<Self::Ok, CaptureError>;
    type SerializeMap = Impossible<Self::Ok, CaptureError>;
    type SerializeStruct = FieldList;
    type SerializeStructVariant = Impossible<Self::Ok, CaptureError>;

    reject! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_f32(f32);
        serialize_f64(f64);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
        serialize_none();
        serialize_unit();
        serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<Self::Ok, CaptureError> {
        not_a_struct()
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<Self::Ok, CaptureError> {
        // A transparent wrapper around a struct row still splits into the inner fields.
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<Self::Ok, CaptureError> {
        not_a_struct()
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, CaptureError> {
        not_a_struct()
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, CaptureError> {
        not_a_struct()
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, CaptureError> {
        not_a_struct()
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, CaptureError> {
        not_a_struct()
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, CaptureError> {
        not_a_struct()
    }

    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Self::SerializeStruct, CaptureError> {
        Ok(FieldList(Vec::with_capacity(len)))
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, CaptureError> {
        not_a_struct()
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    enum Kind {
        Rifle,
        Burst(u8),
    }

    #[derive(Serialize)]
    struct Inner {
        x: i32,
    }

    #[derive(Serialize)]
    struct Row {
        name: String,
        damage: u32,
        kind: Kind,
        burst: Kind,
        at: Inner,
        tags: Vec<String>,
        maybe: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        gone: Option<u8>,
    }

    #[test]
    fn fields_come_out_in_order_as_ron() {
        let row = Row {
            name: "a \"q\"".into(),
            damage: 30,
            kind: Kind::Rifle,
            burst: Kind::Burst(3),
            at: Inner { x: -2 },
            tags: vec!["t".into()],
            maybe: Some(0.5),
            gone: None,
        };
        let fields = row_fields(&row).expect("capture");
        assert_eq!(
            fields,
            vec![
                ("name", "\"a \\\"q\\\"\"".to_owned()),
                ("damage", "30".to_owned()),
                ("kind", "Rifle".to_owned()),
                ("burst", "Burst(3)".to_owned()),
                ("at", "(x:-2)".to_owned()),
                ("tags", "[\"t\"]".to_owned()),
                ("maybe", "Some(0.5)".to_owned()),
            ]
        );
    }

    #[test]
    fn non_struct_rows_are_rejected() {
        assert!(row_fields(&5u32).is_err());
        assert!(row_fields(&vec![1, 2]).is_err());
    }
}
