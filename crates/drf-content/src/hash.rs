// SPDX-License-Identifier: GPL-3.0-or-later

//! The client's own string hash.
//!
//! `String::HashString` @ `0x00592142`: djb2 seeded with `0x1505`, multiplied
//! by `0x21`, over the ASCII-case-folded bytes, and a result of zero is
//! returned as one. The client caches this value into every `DFCProperty` and
//! uses it as the on-wire class id (`getClassById` @ `0x00624570`) and as the
//! `GCNode+0x50` archetype id (`GCClassRegistry::getObjectDef` @ `0x005E2F40`),
//! so it is the id space the character graph is written in.

/// `String::HashString` over `name`, stopping at the first NUL.
///
/// `name` may be any byte string; bytes `A`..=`Z` are folded to lowercase
/// before hashing, and a hash of zero is returned as one (the client's own
/// sentinel against a legitimate empty-string hash).
pub fn dfc_hash(name: &[u8]) -> u32 {
    let mut result: u32 = 0x1505;
    for &byte in name {
        if byte == 0 {
            break;
        }
        let folded = if (0x41..=0x5A).contains(&byte) {
            byte + 0x20
        } else {
            byte
        };
        result = result.wrapping_mul(0x21).wrapping_add(folded as u32);
    }
    if result == 0 {
        1
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_client_hash_for_the_item_property_names() {
        // These ids appear verbatim in a serialized item object; they are the
        // hash of the property's short name, not of its native symbol.
        assert_eq!(dfc_hash(b"InventoryX"), 0x3997_6ECB);
        assert_eq!(dfc_hash(b"Level"), 0x0FDA_BC3D);
        assert_eq!(dfc_hash(b"Name"), 0x7C9B_0C46);
        assert_eq!(dfc_hash(b"ID"), 0x0059_7832);
    }

    #[test]
    fn case_folds_and_stops_at_nul() {
        assert_eq!(dfc_hash(b"Level"), dfc_hash(b"LEVEL"));
        assert_eq!(dfc_hash(b"Level\0garbage"), dfc_hash(b"Level"));
    }
}
