//! Length-capped wire containers. Deserialization rejects a length above the cap
//! before allocating anything proportional to it.

use core::fmt;
use core::marker::PhantomData;
use core::ops::Deref;

use serde::de::{self, Deserialize, Deserializer, SeqAccess, Visitor};
use serde::ser::{Serialize, Serializer};

/// A value is longer than its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("length {len} exceeds the cap of {max}")]
pub struct BoundError {
    /// Actual length.
    pub len: usize,
    /// Allowed maximum.
    pub max: usize,
}

fn check(len: usize, max: usize) -> Result<(), BoundError> {
    if len > max {
        Err(BoundError { len, max })
    } else {
        Ok(())
    }
}

/// Byte string of at most `N` bytes.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedBytes<const N: usize>(Vec<u8>);

impl<const N: usize> BoundedBytes<N> {
    /// Empty value.
    pub fn new() -> Self {
        Self(Vec::new())
    }
    /// Borrows the bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
    /// Returns the owned bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedBytes<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BoundedBytes({} bytes)", self.0.len())
    }
}

impl<const N: usize> Deref for BoundedBytes<N> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> AsRef<[u8]> for BoundedBytes<N> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> TryFrom<Vec<u8>> for BoundedBytes<N> {
    type Error = BoundError;
    fn try_from(v: Vec<u8>) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v))
    }
}

impl<const N: usize> TryFrom<&[u8]> for BoundedBytes<N> {
    type Error = BoundError;
    fn try_from(v: &[u8]) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v.to_vec()))
    }
}

impl<const N: usize> Serialize for BoundedBytes<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

struct BytesVisitor<const N: usize>;

impl<'de, const N: usize> Visitor<'de> for BytesVisitor<N> {
    type Value = BoundedBytes<N>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at most {N} bytes")
    }
    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        BoundedBytes::try_from(v).map_err(E::custom)
    }
    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        BoundedBytes::try_from(v).map_err(E::custom)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        if let Some(hint) = seq.size_hint() {
            check(hint, N).map_err(de::Error::custom)?;
        }
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
        while let Some(b) = seq.next_element::<u8>()? {
            check(out.len() + 1, N).map_err(de::Error::custom)?;
            out.push(b);
        }
        Ok(BoundedBytes(out))
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedBytes<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_bytes(BytesVisitor::<N>)
    }
}

/// UTF-8 string of at most `N` bytes.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedString<const N: usize>(String);

impl<const N: usize> BoundedString<N> {
    /// Empty value.
    pub fn new() -> Self {
        Self(String::new())
    }
    /// Borrows the string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Returns the owned string.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<const N: usize> fmt::Debug for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<const N: usize> fmt::Display for BoundedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<const N: usize> Deref for BoundedString<N> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> AsRef<str> for BoundedString<N> {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<const N: usize> TryFrom<String> for BoundedString<N> {
    type Error = BoundError;
    fn try_from(v: String) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v))
    }
}

impl<const N: usize> TryFrom<&str> for BoundedString<N> {
    type Error = BoundError;
    fn try_from(v: &str) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v.to_owned()))
    }
}

impl<const N: usize> Serialize for BoundedString<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

struct StrVisitor<const N: usize>;

impl<const N: usize> Visitor<'_> for StrVisitor<N> {
    type Value = BoundedString<N>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a string of at most {N} bytes")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        BoundedString::try_from(v).map_err(E::custom)
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        BoundedString::try_from(v).map_err(E::custom)
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedString<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_str(StrVisitor::<N>)
    }
}

/// Sequence of at most `N` elements.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T, const N: usize> BoundedVec<T, N> {
    /// Empty value.
    pub fn new() -> Self {
        Self(Vec::new())
    }
    /// Borrows the elements.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
    /// Borrows the elements mutably (the length cannot change through it).
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.0
    }
    /// Returns the owned elements.
    pub fn into_inner(self) -> Vec<T> {
        self.0
    }
    /// Appends an element unless the cap is reached.
    pub fn push(&mut self, v: T) -> Result<(), BoundError> {
        check(self.0.len() + 1, N)?;
        self.0.push(v);
        Ok(())
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for BoundedVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<T, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T, const N: usize> TryFrom<Vec<T>> for BoundedVec<T, N> {
    type Error = BoundError;
    fn try_from(v: Vec<T>) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v))
    }
}

impl<T: Clone, const N: usize> TryFrom<&[T]> for BoundedVec<T, N> {
    type Error = BoundError;
    fn try_from(v: &[T]) -> Result<Self, BoundError> {
        check(v.len(), N)?;
        Ok(Self(v.to_vec()))
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a BoundedVec<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Serialize, const N: usize> Serialize for BoundedVec<T, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

struct SeqVisitor<T, const N: usize>(PhantomData<T>);

impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for SeqVisitor<T, N> {
    type Value = BoundedVec<T, N>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a sequence of at most {N} elements")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        if let Some(hint) = seq.size_hint() {
            check(hint, N).map_err(de::Error::custom)?;
        }
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
        while let Some(v) = seq.next_element::<T>()? {
            check(out.len() + 1, N).map_err(de::Error::custom)?;
            out.push(v);
        }
        Ok(BoundedVec(out))
    }
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedVec<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(SeqVisitor::<T, N>(PhantomData))
    }
}
