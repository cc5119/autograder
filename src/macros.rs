//! Small crate-wide declarative macros.

/// Builds a `HashMap<String, String>`, calling `.to_string()` on every key
/// and value so callers don't have to spell `.to_string()`/`.clone()`/
/// `.as_str()` themselves.
#[macro_export]
macro_rules! str_map {
    ($($key:expr => $value:expr),* $(,)?) => {
        ::std::collections::HashMap::<String, String>::from([
            $(($key.to_string(), $value.to_string())),*
        ])
    };
}
