use ruff_text_size::TextRange;

/// Where the docstring of a class, function or variable comes from.
///
/// Docstrings read from source are not kept in memory: the symbol stores the range of the
/// string literal, and hover reads its value again from the symbol's file (see
/// `FeaturesUtils::doc_string_text`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocString {
    /// Range of the docstring's string literal expression in the symbol's file.
    Source(TextRange),
    /// Text given by a hook.
    Static(&'static str),
}
