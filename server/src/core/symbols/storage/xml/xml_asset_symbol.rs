use crate::core::symbols::storage::{XmlDataParent, xml::xml_field_map::XmlFieldMap};

use ruff_text_size::TextRange;

use crate::constants::OYarn;

#[derive(Debug)]
pub struct XmlAssetSymbol {
    pub xml_id: Option<OYarn>,
    pub is_external: bool,
    pub range: TextRange,

    parent: XmlDataParent,
    pub(in crate::core::symbols::storage) fields: XmlFieldMap,
}

impl XmlAssetSymbol {
    pub fn new(xml_id: Option<OYarn>, range: TextRange, parent: XmlDataParent, is_external: bool) -> Self {
        Self { xml_id, range, parent, is_external, fields: XmlFieldMap::default() }
    }

    pub fn parent(&self) -> XmlDataParent {
        self.parent
    }
}
