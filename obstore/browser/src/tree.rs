use std::collections::BTreeMap;

use obstore::catalog::CatalogEntry;

const MISSING: &str = "<none>";

/// Claim names from a slash-separated path such as `/object-type/board/`.
pub fn parse_group_path(text: &str) -> Vec<String> {
    text.split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupBucket {
    Missing,
    Value(String),
}

impl GroupBucket {
    pub fn label(&self) -> &str {
        match self {
            Self::Missing => MISSING,
            Self::Value(value) => value,
        }
    }

    fn sort_key(&self) -> (&str, &str) {
        match self {
            Self::Missing => ("", ""),
            Self::Value(value) => ("value", value.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeGroup {
    pub claim: String,
    pub bucket: GroupBucket,
    pub groups: Vec<TreeGroup>,
    pub objects: Vec<CatalogEntry>,
}

impl TreeGroup {
    pub fn label(&self) -> &str {
        self.bucket.label()
    }

    pub fn is_missing(&self) -> bool {
        matches!(self.bucket, GroupBucket::Missing)
    }
}

pub fn group_entries(entries: &[CatalogEntry], claims: &[String]) -> Vec<TreeGroup> {
    if claims.is_empty() {
        return Vec::new();
    }
    let mut buckets: BTreeMap<(String, String), Vec<CatalogEntry>> = BTreeMap::new();
    for entry in entries {
        let bucket = bucket_for(entry, &claims[0]);
        buckets
            .entry((bucket.sort_key().0.to_string(), bucket.label().to_string()))
            .or_default()
            .push(entry.clone());
    }
    buckets
        .into_iter()
        .map(|((kind, label), mut members)| {
            members.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
            let bucket = if kind.is_empty() {
                GroupBucket::Missing
            } else {
                GroupBucket::Value(label)
            };
            if claims.len() == 1 {
                TreeGroup {
                    claim: claims[0].clone(),
                    bucket,
                    groups: Vec::new(),
                    objects: members,
                }
            } else {
                TreeGroup {
                    claim: claims[0].clone(),
                    bucket,
                    groups: group_entries(&members, &claims[1..]),
                    objects: Vec::new(),
                }
            }
        })
        .collect()
}

pub fn group_key(prefix: &str, group: &TreeGroup) -> String {
    let marker = if group.is_missing() {
        "missing"
    } else {
        "value"
    };
    let piece = format!("{}:{marker}:{}", group.claim, group.label());
    if prefix.is_empty() {
        piece
    } else {
        format!("{prefix}/{piece}")
    }
}

fn bucket_for(entry: &CatalogEntry, claim: &str) -> GroupBucket {
    match entry
        .claims
        .iter()
        .find(|(name, _)| name == claim)
        .map(|(_, value)| value.as_str())
    {
        Some(value) if !value.is_empty() => GroupBucket::Value(value.to_string()),
        _ => GroupBucket::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::{group_entries, parse_group_path, GroupBucket};
    use obstore::catalog::{CatalogEntry, ObjectPresence};
    use obstore::store::ObjectId;

    fn entry(id_byte: u8, claims: &[(&str, &str)]) -> CatalogEntry {
        let id = format!("{:02x}", id_byte).repeat(32);
        CatalogEntry {
            id: ObjectId::parse(&id).expect("id"),
            presence: ObjectPresence::Complete,
            manifest_length: Some(1),
            data_length: Some(1),
            claims: claims
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            has_loa_envelope: true,
            has_transfer_manifest: false,
            has_transfer_envelope: false,
            piece_count: 0,
        }
    }

    fn ids(group: &super::TreeGroup) -> Vec<&str> {
        group
            .objects
            .iter()
            .map(|entry| entry.id.as_str())
            .collect()
    }

    #[test]
    fn parse_group_path_keeps_the_claim_names_between_slashes() {
        assert_eq!(
            parse_group_path("/object-type/board/"),
            ["object-type", "board"]
        );
        assert_eq!(
            parse_group_path("object-type/board"),
            ["object-type", "board"]
        );
        assert!(parse_group_path("  /  / ").is_empty());
        assert!(parse_group_path("").is_empty());
    }

    #[test]
    fn groups_by_object_type_then_board_with_missing_values_under_none() {
        let firmware = entry(0x11, &[("object-type", "firmware"), ("board", "heltec-v4")]);
        let firmware_no_board = entry(0x22, &[("object-type", "firmware")]);
        let board_only = entry(0x33, &[("board", "heltec-v4")]);
        let bare = entry(0x44, &[]);
        let note = entry(0x55, &[("object-type", "note"), ("board", "t-deck")]);
        let claims = parse_group_path("/object-type/board/");
        let tree = group_entries(
            &[
                note.clone(),
                bare.clone(),
                firmware_no_board.clone(),
                board_only.clone(),
                firmware.clone(),
            ],
            &claims,
        );

        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].label(), "<none>");
        assert!(tree[0].is_missing());
        assert_eq!(tree[0].claim, "object-type");
        assert_eq!(tree[0].groups.len(), 2);
        assert_eq!(tree[0].groups[0].label(), "<none>");
        assert_eq!(tree[0].groups[0].claim, "board");
        assert_eq!(ids(&tree[0].groups[0]), [bare.id.as_str()]);
        assert_eq!(tree[0].groups[1].label(), "heltec-v4");
        assert_eq!(ids(&tree[0].groups[1]), [board_only.id.as_str()]);

        assert_eq!(tree[1].label(), "firmware");
        assert_eq!(tree[1].groups[0].label(), "<none>");
        assert_eq!(ids(&tree[1].groups[0]), [firmware_no_board.id.as_str()]);
        assert_eq!(tree[1].groups[1].label(), "heltec-v4");
        assert_eq!(ids(&tree[1].groups[1]), [firmware.id.as_str()]);

        assert_eq!(tree[2].label(), "note");
        assert_eq!(tree[2].groups.len(), 1);
        assert_eq!(tree[2].groups[0].label(), "t-deck");
        assert_eq!(ids(&tree[2].groups[0]), [note.id.as_str()]);
        assert!(matches!(tree[2].bucket, GroupBucket::Value(_)));
    }

    #[test]
    fn a_single_claim_lists_object_ids_under_each_value() {
        let firmware = entry(0x11, &[("object-type", "firmware")]);
        let bare = entry(0x44, &[]);
        let tree = group_entries(
            &[firmware.clone(), bare.clone()],
            &parse_group_path("/object-type/"),
        );
        assert_eq!(tree[0].label(), "<none>");
        assert!(tree[0].groups.is_empty());
        assert_eq!(ids(&tree[0]), [bare.id.as_str()]);
        assert_eq!(tree[1].label(), "firmware");
        assert_eq!(ids(&tree[1]), [firmware.id.as_str()]);
    }
}
