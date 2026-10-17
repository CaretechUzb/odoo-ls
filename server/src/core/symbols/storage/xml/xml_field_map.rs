//! Flat map of the fields of an xml record or asset.

use crate::{constants::OYarn, core::symbols::symbol_keys::XmlFieldKey};

/// The fields of an xml record or asset, by name.
///
/// A `Vec` of `(name, key)` rows sorted by name: a record has a few fields, so 24 bytes per field
/// is much smaller than a hash map. Fields are only added while the record is built, and the
/// `Vec` grows one row at a time to keep no spare capacity.
#[derive(Debug, Default, Clone)]
pub struct XmlFieldMap {
    rows: Vec<(OYarn, XmlFieldKey)>,
}

impl XmlFieldMap {
    /// Add the field `name`, or replace its key and return the previous one.
    pub fn insert(&mut self, name: OYarn, key: XmlFieldKey) -> Option<XmlFieldKey> {
        match self.rows.binary_search_by(|(row_name, _)| row_name.as_str().cmp(name.as_str())) {
            Ok(index) => Some(std::mem::replace(&mut self.rows[index].1, key)),
            Err(index) => {
                self.rows.reserve_exact(1);
                self.rows.insert(index, (name, key));
                None
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&XmlFieldKey> {
        self.rows.binary_search_by(|(row_name, _)| row_name.as_str().cmp(name)).ok()
            .map(|index| &self.rows[index].1)
    }

    /// The fields, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&OYarn, &XmlFieldKey)> {
        self.rows.iter().map(|(name, key)| (name, key))
    }

    pub fn values(&self) -> impl Iterator<Item = &XmlFieldKey> {
        self.rows.iter().map(|(_, key)| key)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<'a> IntoIterator for &'a XmlFieldMap {
    type Item = (&'a OYarn, &'a XmlFieldKey);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (OYarn, XmlFieldKey)>, fn(&'a (OYarn, XmlFieldKey)) -> (&'a OYarn, &'a XmlFieldKey)>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter().map(|(name, key)| (name, key))
    }
}

#[cfg(test)]
mod tests {
    use slotmap::SlotMap;

    use super::*;
    use crate::oyarn;

    #[test]
    fn insert_keeps_names_sorted_and_replaces_a_name() {
        let mut keys: SlotMap<XmlFieldKey, ()> = SlotMap::with_key();
        let (a, b, c, d) = (keys.insert(()), keys.insert(()), keys.insert(()), keys.insert(()));
        let mut map = XmlFieldMap::default();
        assert_eq!(map.insert(oyarn!("name"), a), None);
        assert_eq!(map.insert(oyarn!("model_id"), b), None);
        assert_eq!(map.insert(oyarn!("ttype"), c), None);
        assert_eq!(map.insert(oyarn!("model_id"), d), Some(b));
        let names: Vec<&str> = map.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["model_id", "name", "ttype"]);
        assert_eq!(map.get("model_id"), Some(&d));
        assert_eq!(map.get("name"), Some(&a));
        assert_eq!(map.get("model"), None);
        assert_eq!(map.values().copied().collect::<Vec<_>>(), [d, a, c]);
        assert_eq!(map.len(), 3);
    }
}
