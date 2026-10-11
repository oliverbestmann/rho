//! `id_type!` declares a string id newtype, e.g. `SessionId`, so different
//! kinds of ids cannot be mixed up. Extensions use it for their own ids.

#[doc(hidden)]
pub use {rusqlite as __rusqlite, schemars as __schemars, serde as __serde};

/// Declares `pub struct $name` wrapping a string, with `new`, a `const`
/// `from_static`, `as_str`, `Display`, serde and JSON schema as a plain
/// string (for tool inputs), and SQLite conversions.
///
/// ```ignore
/// rho::id_type! {
///     /// A background task's id.
///     pub TaskId
/// }
/// const FIRST: TaskId = TaskId::from_static("1");
/// ```
#[macro_export]
macro_rules! id_type {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis struct $name(::std::borrow::Cow<'static, str>);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(::std::borrow::Cow::Owned(id.into()))
            }

            pub const fn from_static(id: &'static str) -> Self {
                Self(::std::borrow::Cow::Borrowed(id))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl $crate::id::__serde::Serialize for $name {
            fn serialize<S: $crate::id::__serde::Serializer>(
                &self,
                s: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> $crate::id::__serde::Deserialize<'de> for $name {
            fn deserialize<D: $crate::id::__serde::Deserializer<'de>>(
                d: D,
            ) -> ::std::result::Result<Self, D::Error> {
                <String as $crate::id::__serde::Deserialize>::deserialize(d).map(Self::new)
            }
        }

        impl $crate::id::__schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                <String as $crate::id::__schemars::JsonSchema>::schema_name()
            }

            fn json_schema(
                generator: &mut $crate::id::__schemars::SchemaGenerator,
            ) -> $crate::id::__schemars::Schema {
                <String as $crate::id::__schemars::JsonSchema>::json_schema(generator)
            }
        }

        impl $crate::id::__rusqlite::types::FromSql for $name {
            fn column_result(
                value: $crate::id::__rusqlite::types::ValueRef<'_>,
            ) -> $crate::id::__rusqlite::types::FromSqlResult<Self> {
                <String as $crate::id::__rusqlite::types::FromSql>::column_result(value)
                    .map(Self::new)
            }
        }

        impl $crate::id::__rusqlite::types::ToSql for $name {
            fn to_sql(
                &self,
            ) -> $crate::id::__rusqlite::Result<$crate::id::__rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    crate::id_type! {
        /// For the test.
        pub TestId
    }

    #[test]
    fn ids_are_plain_strings_on_the_wire() {
        const FIXED: TestId = TestId::from_static("a");
        assert_eq!(FIXED, TestId::new("a"));
        assert_eq!(serde_json::to_value(&FIXED).unwrap(), "a");
        let back: TestId = serde_json::from_value("b".into()).unwrap();
        assert_eq!((back.as_str(), back.to_string()), ("b", "b".to_string()));
        let schema = schemars::schema_for!(TestId);
        assert_eq!(schema.get("type").unwrap(), "string");
        assert_eq!(format!("{FIXED:?}"), r#"TestId("a")"#);
    }
}
