//! Admission-aware JSON tree materialization for the H18 transport boundary.
//!
//! The callback retains reservations until its response is dropped. Unknown-length growth
//! allocates a distinct replacement while the old buffer is live, admits its entire capacity,
//! then moves elements; old reservations remain conservatively held. No final parser copy is
//! involved. The caller still validates encoded size by encoding this admitted JSON value.

use crate::h18_storage::btree_storage_bytes;
use erp_sdk::{Error, ErrorKind, Result};
use serde::{Serialize, ser};
use serde_json::{Map, Number, Value};
use std::{fmt, io, mem::size_of};

const MAX_DEPTH: usize = 128;
#[derive(Debug)]
struct SerError;
impl fmt::Display for SerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("H18 transport serialization refused")
    }
}
impl std::error::Error for SerError {}
impl ser::Error for SerError {
    // Serialize implementations may supply an unbounded Display; do not materialize it in an error.
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self
    }
}
type SResult<T> = std::result::Result<T, SerError>;

struct State<'a, F> {
    admit: &'a mut F,
    failure: Option<Error>,
}
impl<F: FnMut(usize) -> Result<()>> State<'_, F> {
    fn check(&self) -> SResult<()> {
        if self.failure.is_some() {
            Err(SerError)
        } else {
            Ok(())
        }
    }
    fn fail<T>(&mut self, message: &'static str) -> SResult<T> {
        if self.failure.is_none() {
            self.failure = Some(Error::validation(message));
        }
        Err(SerError)
    }
    fn absorb<T>(&mut self, result: Result<T>) -> SResult<T> {
        self.check()?;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.failure = Some(error);
                Err(SerError)
            }
        }
    }
    fn charge(&mut self, bytes: usize) -> SResult<()> {
        self.check()?;
        if bytes == 0 {
            return Ok(());
        }
        let result = (self.admit)(bytes);
        self.absorb(result)
    }
    fn allocation<T>(&mut self) -> SResult<T> {
        if self.failure.is_none() {
            self.failure = Some(Error::new(
                ErrorKind::ResourceExhausted,
                "H18 transport allocation failed",
            ));
        }
        Err(SerError)
    }
    fn multiply(&mut self, n: usize, size: usize) -> SResult<usize> {
        match n.checked_mul(size) {
            Some(bytes) => Ok(bytes),
            None => self.fail("H18 transport storage size overflow"),
        }
    }
    fn depth(&mut self, current: usize, extra: usize) -> SResult<usize> {
        match current.checked_add(extra) {
            Some(depth) if depth <= MAX_DEPTH => Ok(depth),
            _ => self.fail("H18 transport nesting exceeds 128 levels"),
        }
    }
    fn next(&mut self, n: usize) -> SResult<usize> {
        match n.checked_add(1) {
            Some(n) => Ok(n),
            None => self.fail("H18 transport storage size overflow"),
        }
    }
    fn vector(&mut self, n: usize) -> SResult<Vec<Value>> {
        let bytes = self.multiply(n, size_of::<Value>())?;
        self.charge(bytes)?;
        let mut values = Vec::new();
        if values.try_reserve_exact(n).is_err() {
            return self.allocation();
        }
        let spare = values.capacity() - n;
        let bytes = self.multiply(spare, size_of::<Value>())?;
        self.charge(bytes)?;
        Ok(values)
    }
    fn ensure_vector(&mut self, values: &mut Vec<Value>) -> SResult<()> {
        self.check()?;
        if values.len() < values.capacity() {
            return Ok(());
        }
        let required = self.next(values.len())?;
        let wanted = if values.capacity() == 0 {
            required
        } else {
            match values.capacity().checked_mul(2) {
                Some(n) => n.max(required),
                None => return self.fail("H18 transport storage size overflow"),
            }
        };
        let mut replacement = self.vector(wanted)?;
        // A new allocation while `values` remains alive forces moving growth and its overlap.
        replacement.append(values);
        *values = replacement;
        Ok(())
    }
    fn text(&mut self, text: &str) -> SResult<String> {
        self.charge(text.len())?;
        let mut out = String::new();
        if out.try_reserve_exact(text.len()).is_err() {
            return self.allocation();
        }
        self.charge(out.capacity() - text.len())?;
        out.push_str(text);
        Ok(out)
    }
    fn append_text(&mut self, out: &mut String, text: &str) -> SResult<()> {
        self.check()?;
        let required = match out.len().checked_add(text.len()) {
            Some(n) => n,
            None => return self.fail("H18 transport storage size overflow"),
        };
        if required > out.capacity() {
            let wanted = match out.capacity().checked_mul(2) {
                Some(n) => n.max(required),
                None => return self.fail("H18 transport storage size overflow"),
            };
            self.charge(wanted)?;
            let mut replacement = String::new();
            if replacement.try_reserve_exact(wanted).is_err() {
                return self.allocation();
            }
            self.charge(replacement.capacity() - wanted)?;
            replacement.push_str(out);
            *out = replacement;
        }
        out.push_str(text);
        Ok(())
    }
    fn display<T: fmt::Display + ?Sized>(&mut self, value: &T) -> SResult<String> {
        struct Writer<'s, 'a, F> {
            state: &'s mut State<'a, F>,
            out: String,
        }
        impl<F: FnMut(usize) -> Result<()>> fmt::Write for Writer<'_, '_, F> {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                self.state
                    .append_text(&mut self.out, text)
                    .map_err(|_| fmt::Error)
            }
        }
        let mut writer = Writer {
            state: self,
            out: String::new(),
        };
        if fmt::write(&mut writer, format_args!("{value}")).is_err() {
            return writer.state.fail("H18 transport display formatting failed");
        }
        writer.state.check()?;
        Ok(writer.out)
    }
    fn float<T: Serialize>(&mut self, value: &T) -> SResult<Value> {
        let mut bytes = ScalarBytes::new();
        if serde_json::to_writer(&mut bytes, value).is_err() {
            return self.fail("H18 transport float formatting failed");
        }
        // Match the prior bounded primitive JSON roundtrip, including its numeric parsing
        // semantics. Only this fixed-size scalar is parsed; no DTO/tree is reparsed.
        let number = match serde_json::from_slice::<f64>(&bytes.bytes[..bytes.len]) {
            Ok(number) => number,
            Err(_) => return self.fail("H18 transport float conversion failed"),
        };
        match Number::from_f64(number) {
            Some(number) => Ok(Value::Number(number)),
            None => self.fail("H18 transport rejects non-finite numbers"),
        }
    }
    fn scalar_key<T: Serialize>(&mut self, value: &T) -> SResult<String> {
        let mut bytes = ScalarBytes::new();
        if serde_json::to_writer(&mut bytes, value).is_err() {
            return self.fail("H18 transport scalar key formatting failed");
        }
        let text = match std::str::from_utf8(&bytes.bytes[..bytes.len]) {
            Ok(text) => text,
            Err(_) => return self.fail("H18 transport scalar key is not UTF-8"),
        };
        self.text(text)
    }
    fn map_bound(&mut self, n: usize) -> SResult<usize> {
        self.absorb(btree_storage_bytes::<String, Value>(n))
    }
}

// Numeric scalar JSON occupies at most 40 decimal digits/sign bytes (i128/u128), or
// the finite IEEE float representation; this fixed stack writer refuses any unexpected excess.
struct ScalarBytes {
    bytes: [u8; 128],
    len: usize,
}
impl ScalarBytes {
    fn new() -> Self {
        Self {
            bytes: [0; 128],
            len: 0,
        }
    }
}
impl io::Write for ScalarBytes {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let end = self
            .len
            .checked_add(input.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(io::ErrorKind::WriteZero)?;
        self.bytes[self.len..end].copy_from_slice(input);
        self.len = end;
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Materialize once, admitting serializer-owned heap storage before allocating/growing it.
/// Callback refusals are sticky even if a Serialize implementation catches an element error.
/// Nonfinite numbers, non-scalar object keys, excessive depth/size and malformed map protocols
/// fail explicitly. Error diagnostics do not materialize arbitrary user-provided Display text.
pub(crate) fn materialize<T: Serialize + ?Sized, F: FnMut(usize) -> Result<()>>(
    value: &T,
    admit: &mut F,
) -> Result<Value> {
    let mut state = State {
        admit,
        failure: None,
    };
    let result = value.serialize(
        Serializer::new(&mut state, 0)
            .map_err(|_| Error::validation("H18 transport initialization failed"))?,
    );
    if let Some(error) = state.failure {
        return Err(error);
    }
    result.map_err(|_| Error::validation("H18 transport serialization failed"))
}
struct Serializer<'s, 'a, F> {
    state: &'s mut State<'a, F>,
    depth: usize,
}
impl<'s, 'a, F: FnMut(usize) -> Result<()>> Serializer<'s, 'a, F> {
    fn new(state: &'s mut State<'a, F>, depth: usize) -> SResult<Self> {
        state.check()?;
        if depth > MAX_DEPTH {
            return state.fail("H18 transport nesting exceeds 128 levels");
        }
        Ok(Self { state, depth })
    }
    fn child<T: Serialize + ?Sized>(self, value: &T) -> SResult<Value> {
        let depth = self.state.depth(self.depth, 1)?;
        value.serialize(Self::new(self.state, depth)?)
    }
    fn seq(self, len: Option<usize>, variant: Option<&str>) -> SResult<Sequence<'s, 'a, F>> {
        let depth = self
            .state
            .depth(self.depth, if variant.is_some() { 2 } else { 1 })?;
        let variant = match variant {
            Some(name) => {
                let bytes = self.state.map_bound(1)?;
                self.state.charge(bytes)?;
                Some(self.state.text(name)?)
            }
            None => None,
        };
        let values = match len {
            Some(n) => self.state.vector(n)?,
            None => Vec::new(),
        };
        Ok(Sequence {
            state: self.state,
            depth,
            values,
            variant,
        })
    }
    fn map(self, len: Option<usize>, variant: Option<&str>) -> SResult<Object<'s, 'a, F>> {
        let depth = self
            .state
            .depth(self.depth, if variant.is_some() { 2 } else { 1 })?;
        let variant = match variant {
            Some(name) => {
                let bytes = self.state.map_bound(1)?;
                self.state.charge(bytes)?;
                Some(self.state.text(name)?)
            }
            None => None,
        };
        // A map of unknown length reserves nothing up front; growth is admitted per entry.
        let entries = len.unwrap_or_default();
        let reserved = self.state.map_bound(entries)?;
        self.state.charge(reserved)?;
        Ok(Object {
            state: self.state,
            depth,
            values: Map::new(),
            key: None,
            attempts: 0,
            reserved,
            variant,
        })
    }
}

impl<'s, 'a, F: FnMut(usize) -> Result<()>> serde::Serializer for Serializer<'s, 'a, F> {
    type Ok = Value;
    type Error = SerError;
    type SerializeSeq = Sequence<'s, 'a, F>;
    type SerializeTuple = Sequence<'s, 'a, F>;
    type SerializeTupleStruct = Sequence<'s, 'a, F>;
    type SerializeTupleVariant = Sequence<'s, 'a, F>;
    type SerializeMap = Object<'s, 'a, F>;
    type SerializeStruct = Object<'s, 'a, F>;
    type SerializeStructVariant = Object<'s, 'a, F>;
    fn serialize_bool(self, v: bool) -> SResult<Value> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> SResult<Value> {
        Ok(Value::Number((v as i64).into()))
    }
    fn serialize_i16(self, v: i16) -> SResult<Value> {
        Ok(Value::Number((v as i64).into()))
    }
    fn serialize_i32(self, v: i32) -> SResult<Value> {
        Ok(Value::Number((v as i64).into()))
    }
    fn serialize_i64(self, v: i64) -> SResult<Value> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_u8(self, v: u8) -> SResult<Value> {
        Ok(Value::Number((v as u64).into()))
    }
    fn serialize_u16(self, v: u16) -> SResult<Value> {
        Ok(Value::Number((v as u64).into()))
    }
    fn serialize_u32(self, v: u32) -> SResult<Value> {
        Ok(Value::Number((v as u64).into()))
    }
    fn serialize_u64(self, v: u64) -> SResult<Value> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_i128(self, v: i128) -> SResult<Value> {
        match Number::from_i128(v) {
            Some(n) => Ok(Value::Number(n)),
            None => self
                .state
                .fail("H18 transport integer is outside JSON Value range"),
        }
    }
    fn serialize_u128(self, v: u128) -> SResult<Value> {
        match Number::from_u128(v) {
            Some(n) => Ok(Value::Number(n)),
            None => self
                .state
                .fail("H18 transport integer is outside JSON Value range"),
        }
    }
    fn serialize_f64(self, v: f64) -> SResult<Value> {
        if !v.is_finite() {
            return self.state.fail("H18 transport rejects non-finite numbers");
        }
        self.state.float(&v)
    }
    fn serialize_f32(self, v: f32) -> SResult<Value> {
        if !v.is_finite() {
            return self.state.fail("H18 transport rejects non-finite numbers");
        }
        self.state.float(&v)
    }
    fn serialize_char(self, v: char) -> SResult<Value> {
        let mut bytes = [0; 4];
        self.serialize_str(v.encode_utf8(&mut bytes))
    }
    fn serialize_str(self, v: &str) -> SResult<Value> {
        self.state.text(v).map(Value::String)
    }
    fn serialize_bytes(self, v: &[u8]) -> SResult<Value> {
        let mut seq = self.seq(Some(v.len()), None)?;
        for byte in v {
            seq.field(byte)?;
        }
        seq.finish()
    }
    fn serialize_unit(self) -> SResult<Value> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> SResult<Value> {
        self.serialize_unit()
    }
    fn serialize_none(self) -> SResult<Value> {
        self.serialize_unit()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> SResult<Value> {
        self.child(v)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> SResult<Value> {
        self.child(v)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, name: &'static str) -> SResult<Value> {
        self.serialize_str(name)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        name: &'static str,
        value: &T,
    ) -> SResult<Value> {
        let mut map = self.map(Some(1), None)?;
        map.entry(name, value)?;
        map.finish()
    }
    fn serialize_seq(self, len: Option<usize>) -> SResult<Self::SerializeSeq> {
        self.seq(len, None)
    }
    fn serialize_tuple(self, len: usize) -> SResult<Self::SerializeTuple> {
        self.seq(Some(len), None)
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        len: usize,
    ) -> SResult<Self::SerializeTupleStruct> {
        self.seq(Some(len), None)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        name: &'static str,
        len: usize,
    ) -> SResult<Self::SerializeTupleVariant> {
        self.seq(Some(len), Some(name))
    }
    fn serialize_map(self, len: Option<usize>) -> SResult<Self::SerializeMap> {
        self.map(len, None)
    }
    fn serialize_struct(self, name: &'static str, len: usize) -> SResult<Self::SerializeStruct> {
        if name == "$serde_json::private::RawValue" || name == "$serde_json::private::Number" {
            return self.state.fail(
                "H18 transport raw JSON and arbitrary-precision number tokens are not supported",
            );
        }
        self.map(Some(len), None)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        name: &'static str,
        len: usize,
    ) -> SResult<Self::SerializeStructVariant> {
        self.map(Some(len), Some(name))
    }
    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> SResult<Value> {
        self.state.display(value).map(Value::String)
    }
}

struct Sequence<'s, 'a, F> {
    state: &'s mut State<'a, F>,
    depth: usize,
    values: Vec<Value>,
    variant: Option<String>,
}
impl<F: FnMut(usize) -> Result<()>> Sequence<'_, '_, F> {
    fn field<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.state.ensure_vector(&mut self.values)?;
        let value = value.serialize(Serializer::new(self.state, self.depth)?)?;
        self.state.check()?;
        self.values.push(value);
        Ok(())
    }
    fn finish(self) -> SResult<Value> {
        self.state.check()?;
        let value = Value::Array(self.values);
        match self.variant {
            None => Ok(value),
            Some(name) => {
                let mut map = Map::new();
                map.insert(name, value);
                Ok(Value::Object(map))
            }
        }
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeSeq for Sequence<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.field(value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeTuple for Sequence<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.field(value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeTupleStruct for Sequence<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.field(value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeTupleVariant for Sequence<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.field(value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}

struct Object<'s, 'a, F> {
    state: &'s mut State<'a, F>,
    depth: usize,
    values: Map<String, Value>,
    key: Option<String>,
    attempts: usize,
    reserved: usize,
    variant: Option<String>,
}
impl<F: FnMut(usize) -> Result<()>> Object<'_, '_, F> {
    fn key<T: Serialize + ?Sized>(&mut self, key: &T) -> SResult<()> {
        self.state.check()?;
        if self.key.is_some() {
            return self.state.fail("H18 transport map key has no value");
        }
        let key = key.serialize(KeySerializer(Serializer::new(self.state, self.depth)?))?;
        self.state.check()?;
        self.key = Some(key);
        Ok(())
    }
    fn value<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.state.check()?;
        let key = match self.key.take() {
            Some(key) => key,
            None => return self.state.fail("H18 transport map value has no key"),
        };
        let attempts = self.state.next(self.attempts)?;
        let bound = self.state.map_bound(attempts)?;
        if bound > self.reserved {
            self.state.charge(bound - self.reserved)?;
            self.reserved = bound;
        }
        self.attempts = attempts;
        let value = value.serialize(Serializer::new(self.state, self.depth)?)?;
        self.state.check()?;
        self.values.insert(key, value);
        Ok(())
    }
    fn entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> SResult<()> {
        self.key(key)?;
        self.value(value)
    }
    fn finish(self) -> SResult<Value> {
        self.state.check()?;
        if self.key.is_some() {
            return self.state.fail("H18 transport map key has no value");
        }
        let value = Value::Object(self.values);
        match self.variant {
            None => Ok(value),
            Some(name) => {
                let mut map = Map::new();
                map.insert(name, value);
                Ok(Value::Object(map))
            }
        }
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeMap for Object<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> SResult<()> {
        self.key(key)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.value(value)
    }
    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> SResult<()> {
        self.entry(key, value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeStruct for Object<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> SResult<()> {
        self.entry(key, value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}
impl<F: FnMut(usize) -> Result<()>> ser::SerializeStructVariant for Object<'_, '_, F> {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> SResult<()> {
        self.entry(key, value)
    }
    fn end(self) -> SResult<Value> {
        self.finish()
    }
}

struct KeySerializer<'s, 'a, F>(Serializer<'s, 'a, F>);
impl<'s, 'a, F: FnMut(usize) -> Result<()>> serde::Serializer for KeySerializer<'s, 'a, F> {
    type Ok = String;
    type Error = SerError;
    type SerializeSeq = ser::Impossible<String, SerError>;
    type SerializeTuple = ser::Impossible<String, SerError>;
    type SerializeTupleStruct = ser::Impossible<String, SerError>;
    type SerializeTupleVariant = ser::Impossible<String, SerError>;
    type SerializeMap = ser::Impossible<String, SerError>;
    type SerializeStruct = ser::Impossible<String, SerError>;
    type SerializeStructVariant = ser::Impossible<String, SerError>;
    fn serialize_str(self, v: &str) -> SResult<String> {
        self.0.state.text(v)
    }
    fn serialize_char(self, v: char) -> SResult<String> {
        let mut bytes = [0; 4];
        self.serialize_str(v.encode_utf8(&mut bytes))
    }
    fn serialize_bool(self, v: bool) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_i8(self, v: i8) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_i16(self, v: i16) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_i32(self, v: i32) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_i64(self, v: i64) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_i128(self, v: i128) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_u8(self, v: u8) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_u16(self, v: u16) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_u32(self, v: u32) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_u64(self, v: u64) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_u128(self, v: u128) -> SResult<String> {
        self.0.state.scalar_key(&v)
    }
    fn serialize_f32(self, v: f32) -> SResult<String> {
        if !v.is_finite() {
            return self
                .0
                .state
                .fail("H18 transport rejects non-finite numbers");
        }
        self.0.state.scalar_key(&v)
    }
    fn serialize_f64(self, v: f64) -> SResult<String> {
        if !v.is_finite() {
            return self
                .0
                .state
                .fail("H18 transport rejects non-finite numbers");
        }
        self.0.state.scalar_key(&v)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, v: &'static str) -> SResult<String> {
        self.serialize_str(v)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> SResult<String> {
        let depth = self.0.state.depth(self.0.depth, 1)?;
        v.serialize(KeySerializer(Serializer::new(self.0.state, depth)?))
    }
    fn collect_str<T: fmt::Display + ?Sized>(self, v: &T) -> SResult<String> {
        self.0.state.display(v)
    }
    fn serialize_bytes(self, _v: &[u8]) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_unit(self) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_unit_struct(self, _name: &'static str) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_none(self) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_seq(self, _len: Option<usize>) -> SResult<Self::SerializeSeq> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_tuple(self, _len: usize) -> SResult<Self::SerializeTuple> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> SResult<Self::SerializeTupleStruct> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> SResult<Self::SerializeTupleVariant> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_map(self, _len: Option<usize>) -> SResult<Self::SerializeMap> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> SResult<Self::SerializeStruct> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> SResult<Self::SerializeStructVariant> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _v: &T) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _v: &T,
    ) -> SResult<String> {
        self.0
            .state
            .fail("H18 transport map key must be a string or JSON scalar")
    }
}
