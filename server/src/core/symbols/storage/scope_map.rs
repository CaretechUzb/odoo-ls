//! Flat map of the symbols declared in the body of a file, package, module, class or function.

use std::cmp::Ordering;

use crate::{constants::OYarn, core::symbols::symbol_keys::SymbolKey, oyarn};

/// One declaration of `name` in the flow section `section` of a scope.
#[derive(Debug, Clone)]
pub struct ScopeRow {
    name: OYarn,
    section: u32,
    key: SymbolKey,
}

impl ScopeRow {
    pub fn name(&self) -> &OYarn {
        &self.name
    }

    /// Index of the flow section (if/else branch, loop body...) the declaration belongs to,
    /// see `SymbolMgr::get_section_for`.
    pub fn section(&self) -> u32 {
        self.section
    }

    pub fn key(&self) -> &SymbolKey {
        &self.key
    }

    fn cmp_to(&self, name: &str, section: u32) -> Ordering {
        self.name.as_str().cmp(name).then(self.section.cmp(&section))
    }
}

/// The symbols declared in a scope, by name, then by section.
///
/// A sorted `Vec` of `(name, section, key)` rows: most names have a single declaration, so one
/// 32-byte row per declaration is much smaller than a map of maps of vectors.
/// Rows are sorted by `(name, section)`; the rows of one `(name, section)` keep their insertion
/// order, which is the source order of the declarations (`get_loc_symbol` relies on it).
#[derive(Debug, Default)]
pub struct ScopeMap {
    rows: Vec<ScopeRow>,
}

impl ScopeMap {
    /// Add `key` as the last declaration of `name` in `section`.
    pub fn insert(&mut self, name: &str, section: u32, key: SymbolKey) {
        let index = self.rows.partition_point(|row| row.cmp_to(name, section) != Ordering::Greater);
        // Rows of the same name are on one side or the other of `index`: reuse their yarn.
        let same_name = |row: &&ScopeRow| row.name.as_str() == name;
        let name = match index.checked_sub(1).and_then(|i| self.rows.get(i)).filter(same_name)
            .or_else(|| self.rows.get(index).filter(same_name)) {
            Some(row) => row.name.clone(),
            None => oyarn!("{}", name),
        };
        self.rows.insert(index, ScopeRow { name, section, key });
    }

    /// All the declarations of `name`, if any.
    pub fn get(&self, name: &str) -> Option<NameEntry<'_>> {
        let start = self.rows.partition_point(|row| row.name.as_str() < name);
        let tail = &self.rows[start..];
        let len = tail.partition_point(|row| row.name.as_str() == name);
        (len > 0).then(|| NameEntry { rows: &tail[..len] })
    }

    /// The declared names, in name order.
    pub fn names(&self) -> impl Iterator<Item = NameEntry<'_>> {
        Self::group(&self.rows)
    }

    /// The declared names that start with `prefix`, in name order.
    pub fn names_with_prefix<'a>(&'a self, prefix: &str) -> impl Iterator<Item = NameEntry<'a>> + use<'a> {
        let start = self.rows.partition_point(|row| row.name.as_str() < prefix);
        let tail = &self.rows[start..];
        let len = tail.partition_point(|row| row.name.starts_with(prefix));
        Self::group(&tail[..len])
    }

    /// Every declared symbol, by name then section.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &SymbolKey> {
        self.rows.iter().map(|row| &row.key)
    }

    /// The declarations of the section `section`, by name.
    pub fn rows_in_section(&self, section: u32) -> impl Iterator<Item = &ScopeRow> {
        self.rows.iter().filter(move |row| row.section == section)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn shrink_to_fit(&mut self) {
        self.rows.shrink_to_fit();
    }

    fn group(rows: &[ScopeRow]) -> impl Iterator<Item = NameEntry<'_>> {
        rows.chunk_by(|a, b| a.name == b.name).map(|rows| NameEntry { rows })
    }
}

/// The declarations of one name in a `ScopeMap`, by section then source order. Never empty.
#[derive(Debug, Clone, Copy)]
pub struct NameEntry<'a> {
    rows: &'a [ScopeRow],
}

impl<'a> NameEntry<'a> {
    pub fn name(&self) -> &'a OYarn {
        &self.rows[0].name
    }

    pub fn rows(&self) -> &'a [ScopeRow] {
        self.rows
    }

    /// The declarations in the section `section`, in source order.
    pub fn section(&self, section: u32) -> &'a [ScopeRow] {
        let start = self.rows.partition_point(|row| row.section < section);
        let tail = &self.rows[start..];
        &tail[..tail.partition_point(|row| row.section == section)]
    }

    /// The sections that declare the name, in section order, each with its declarations.
    pub fn sections(&self) -> impl Iterator<Item = (u32, &'a [ScopeRow])> + use<'a> {
        self.rows.chunk_by(|a, b| a.section == b.section).map(|rows| (rows[0].section, rows))
    }

    /// All the declarations, by section then source order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &'a SymbolKey> + use<'a> {
        self.rows.iter().map(|row| &row.key)
    }
}

#[cfg(test)]
mod tests {
    use slotmap::KeyData;

    use super::*;
    use crate::core::symbols::symbol_keys::VariableKey;

    fn key(n: u64) -> SymbolKey {
        SymbolKey::Variable(VariableKey::from(KeyData::from_ffi(n | (1 << 32))))
    }

    fn keys(rows: &[ScopeRow]) -> Vec<SymbolKey> {
        rows.iter().map(|row| *row.key()).collect()
    }

    #[test]
    fn rows_sorted_by_name_and_section_keep_insertion_order() {
        let mut map = ScopeMap::default();
        map.insert("b", 1, key(1));
        map.insert("a", 2, key(2));
        map.insert("b", 0, key(3));
        map.insert("b", 1, key(4));
        map.insert("a", 0, key(5));
        map.insert("ab", 0, key(6));
        map.insert("b", 1, key(7));

        let names: Vec<_> = map.names().map(|entry| entry.name().to_string()).collect();
        assert_eq!(names, ["a", "ab", "b"]);
        let b = map.get("b").unwrap();
        assert_eq!(keys(b.section(1)), [key(1), key(4), key(7)]);
        assert_eq!(keys(b.section(0)), [key(3)]);
        assert!(b.section(2).is_empty());
        assert_eq!(b.sections().map(|(s, rows)| (s, rows.len())).collect::<Vec<_>>(), [(0, 1), (1, 3)]);
        assert_eq!(b.keys().copied().collect::<Vec<_>>(), [key(3), key(1), key(4), key(7)]);
        assert_eq!(map.keys().count(), 7);
        assert_eq!(map.rows_in_section(0).map(|row| *row.key()).collect::<Vec<_>>(), [key(5), key(6), key(3)]);
        assert!(map.get("c").is_none());
        assert!(map.get("").is_none());
        assert_eq!(map.get("a").unwrap().rows().len(), 2);
    }

    #[test]
    fn prefix_scan() {
        let mut map = ScopeMap::default();
        for (i, name) in ["self", "se", "sel", "s", "other", "selfish", "t", "sf"].iter().enumerate() {
            map.insert(name, 0, key(i as u64));
        }
        let with = |p: &str| map.names_with_prefix(p).map(|e| e.name().to_string()).collect::<Vec<_>>();
        assert_eq!(with("sel"), ["sel", "self", "selfish"]);
        assert_eq!(with("s"), ["s", "se", "sel", "self", "selfish", "sf"]);
        assert_eq!(with(""), ["other", "s", "se", "sel", "self", "selfish", "sf", "t"]);
        assert!(with("z").is_empty());
    }
}
