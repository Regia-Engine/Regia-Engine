// SPDX-License-Identifier: MPL-2.0
//! JSON shape admission before constructing arrays, objects or serde_json Values.
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

use super::{ContentBudget, ContentError};

pub(crate) fn admit(bytes: &[u8], budget: &mut ContentBudget<'_>) -> Result<(), ContentError> {
    budget.checkpoint()?;
    let mut failure = None;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let result = Node {
        budget,
        failure: &mut failure,
        root: true,
        accessor: false,
        accessor_array: false,
    }
    .deserialize(&mut deserializer);
    if let Some(error) = failure {
        return Err(error);
    }
    result.map_err(|error| ContentError::Invalid(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| ContentError::Invalid(error.to_string()))
}

struct Node<'a, 'b> {
    budget: &'a mut ContentBudget<'b>,
    failure: &'a mut Option<ContentError>,
    root: bool,
    accessor: bool,
    accessor_array: bool,
}

impl<'de> DeserializeSeed<'de> for Node<'_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let result = self.budget.work(1).and_then(|()| {
            if self.accessor {
                self.budget.accessors(1)
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            let message = error.to_string();
            *self.failure = Some(error);
            return Err(serde::de::Error::custom(message));
        }
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_, '_> {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON value")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq
            .next_element_seed(Node {
                budget: self.budget,
                failure: self.failure,
                root: false,
                accessor: self.accessor_array,
                accessor_array: false,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            map.next_value_seed(Node {
                budget: self.budget,
                failure: self.failure,
                root: false,
                accessor: false,
                accessor_array: self.root && key == "accessors",
            })?;
        }
        Ok(())
    }
}
