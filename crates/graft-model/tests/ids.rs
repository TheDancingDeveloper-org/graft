//! Text forms of ids: exactly the documented forms parse.

use graft_model::{BlobId, ChangesetId, IdError, IdPrefix, ObjectId, ObjectKind, TreeId};

const HEX: &str = "5c1eb18777436c10d0c36067dcca7b83c1d96a19e31c3e0a98d7b0c7c79df34d";

#[test]
fn hex_round_trips() {
    let id = ObjectId::from_hex(HEX).unwrap();
    assert_eq!(id.to_hex(), HEX);
}

#[test]
fn hex_rejects_wrong_lengths() {
    assert_eq!(ObjectId::from_hex(""), Err(IdError::Length(0)));
    assert_eq!(ObjectId::from_hex(&HEX[..63]), Err(IdError::Length(63)));
    assert_eq!(
        ObjectId::from_hex(&format!("{HEX}0")),
        Err(IdError::Length(65))
    );
}

#[test]
fn hex_rejects_upper_case_and_non_hex() {
    assert_eq!(
        ObjectId::from_hex(&HEX.to_uppercase()),
        Err(IdError::NotLowerHex)
    );
    let mut s = HEX.to_owned();
    s.replace_range(10..11, "g");
    assert_eq!(ObjectId::from_hex(&s), Err(IdError::NotLowerHex));
    s.replace_range(10..11, " ");
    assert_eq!(ObjectId::from_hex(&s), Err(IdError::NotLowerHex));
}

#[test]
fn hex_rejects_multibyte_characters_of_the_right_byte_length() {
    // 62 hex characters plus one two-byte character: 64 bytes.
    let s = format!("{}é", &HEX[..62]);
    assert_eq!(s.len(), 64);
    assert_eq!(ObjectId::from_hex(&s), Err(IdError::NotLowerHex));
}

#[test]
fn hex_rejects_0x_prefix() {
    assert_eq!(
        ObjectId::from_hex(&format!("0x{}", &HEX[..62])),
        Err(IdError::NotLowerHex)
    );
}

#[test]
fn typed_ids_round_trip() {
    let id = ObjectId::from_hex(HEX).unwrap();
    let b = BlobId(id);
    let t = TreeId(id);
    let c = ChangesetId(id);
    assert_eq!(b.to_string(), format!("b{HEX}"));
    assert_eq!(BlobId::parse(&b.to_string()), Ok(b));
    assert_eq!(TreeId::parse(&t.to_string()), Ok(t));
    assert_eq!(ChangesetId::parse(&c.to_string()), Ok(c));
    assert_eq!(c.to_string().parse::<ChangesetId>(), Ok(c));
}

#[test]
fn typed_ids_reject_the_wrong_kind_letter() {
    assert_eq!(
        TreeId::parse(&format!("b{HEX}")),
        Err(IdError::WrongKind {
            expected: ObjectKind::Tree
        })
    );
    assert_eq!(
        BlobId::parse(&format!("B{HEX}")),
        Err(IdError::WrongKind {
            expected: ObjectKind::Blob
        })
    );
}

#[test]
fn typed_ids_reject_bare_hex_and_wrong_lengths() {
    assert_eq!(BlobId::parse(HEX), Err(IdError::Length(64)));
    assert_eq!(BlobId::parse(""), Err(IdError::Length(0)));
    assert_eq!(BlobId::parse(&format!("b{HEX}0")), Err(IdError::Length(66)));
    assert_eq!(
        BlobId::parse(&format!("b{}", HEX.to_uppercase())),
        Err(IdError::NotLowerHex)
    );
}

#[test]
fn typed_ids_reject_a_multibyte_first_character() {
    let s = format!("é{}", &HEX[..63]);
    assert_eq!(s.len(), 65);
    assert_eq!(
        ChangesetId::parse(&s),
        Err(IdError::WrongKind {
            expected: ObjectKind::Changeset
        })
    );
}

#[test]
fn prefixes_accept_12_to_64_hex() {
    let id = ObjectId::from_hex(HEX).unwrap();
    for n in [12, 13, 40, 64] {
        let p = IdPrefix::parse(&format!("c{}", &HEX[..n])).unwrap();
        assert_eq!(p.kind(), ObjectKind::Changeset);
        assert_eq!(p.hex(), &HEX[..n]);
        assert!(p.matches(&id));
    }
    let other = ObjectId([0; 32]);
    assert!(
        !IdPrefix::parse(&format!("t{}", &HEX[..12]))
            .unwrap()
            .matches(&other)
    );
}

#[test]
fn prefixes_reject_short_long_and_malformed() {
    assert_eq!(
        IdPrefix::parse(&format!("c{}", &HEX[..11])),
        Err(IdError::Length(11))
    );
    assert_eq!(
        IdPrefix::parse(&format!("c{HEX}0")),
        Err(IdError::Length(65))
    );
    assert_eq!(IdPrefix::parse(&HEX[..13]), Err(IdError::NotLowerHex));
    assert_eq!(
        IdPrefix::parse(&format!("x{}", &HEX[..12])),
        Err(IdError::NotLowerHex)
    );
    assert_eq!(
        IdPrefix::parse(&format!("c{}", HEX[..12].to_uppercase())),
        Err(IdError::NotLowerHex)
    );
    assert_eq!(IdPrefix::parse(""), Err(IdError::NotLowerHex));
}

#[test]
fn debug_shows_a_short_form() {
    let id = ObjectId::from_hex(HEX).unwrap();
    assert_eq!(format!("{id:?}"), format!("ObjectId({})", &HEX[..12]));
}
