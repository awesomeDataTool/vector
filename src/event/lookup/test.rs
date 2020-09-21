use super::*;

const SUFFICIENTLY_COMPLEX: &str =
    r#"regular."quoted"."quoted but spaces"."quoted.but.periods".lookup[0].nested_lookup[0][0]"#;
const SUFFICIENTLY_DECOMPOSED: [Segment; 9] = [
    Segment::field(r#"regular"#),
    Segment::field(r#"quoted"#),
    Segment::field(r#"quoted but spaces"#),
    Segment::field(r#"quoted.but.periods"#),
    Segment::field(r#"lookup"#),
    Segment::index(0),
    Segment::field(r#"nested_lookup"#),
    Segment::index(0),
    Segment::index(0),
];

#[test]
fn impl_index_ranges() {
    crate::test_util::trace_init();
    let lookup = Lookup::try_from(SUFFICIENTLY_COMPLEX).unwrap();

    // This test is primarily to ensure certain interfaces exist and weren't disturbed.
    assert_eq!(lookup[..], SUFFICIENTLY_DECOMPOSED[..]);
    assert_eq!(lookup[..4], SUFFICIENTLY_DECOMPOSED[..4]);
    assert_eq!(lookup[..=4], SUFFICIENTLY_DECOMPOSED[..=4]);
    assert_eq!(lookup[2..], SUFFICIENTLY_DECOMPOSED[2..]);
}

#[test]
fn impl_index_usize() {
    crate::test_util::trace_init();
    let lookup = Lookup::try_from(SUFFICIENTLY_COMPLEX).unwrap();

    for i in 0..SUFFICIENTLY_DECOMPOSED.len() {
        assert_eq!(lookup[i], SUFFICIENTLY_DECOMPOSED[i])
    }
}

#[test]
fn impl_index_mut_index_mut() {
    crate::test_util::trace_init();
    let mut lookup = Lookup::try_from(SUFFICIENTLY_COMPLEX).unwrap();

    for i in 0..SUFFICIENTLY_DECOMPOSED.len() {
        let x = &mut lookup[i]; // Make sure we force a mutable borrow!
        assert_eq!(x, &mut SUFFICIENTLY_DECOMPOSED[i])
    }
}

#[test]
fn iter() {
    crate::test_util::trace_init();
    let lookup = Lookup::try_from(SUFFICIENTLY_COMPLEX).unwrap();

    let mut iter = lookup.iter();
    for (index, expected) in SUFFICIENTLY_DECOMPOSED.iter().enumerate() {
        let parsed = iter.next().expect(&format!(
            "Expected at index {}: {:?}, got None.",
            index, expected
        ));
        assert_eq!(expected, parsed, "Failed at {}", index);
    }
}

#[test]
fn into_iter() {
    crate::test_util::trace_init();
    let lookup = Lookup::try_from(SUFFICIENTLY_COMPLEX).unwrap();
    let mut iter = lookup.into_iter();
    for (index, expected) in SUFFICIENTLY_DECOMPOSED.iter().cloned().enumerate() {
        let parsed = iter.next().expect(&format!(
            "Expected at index {}: {:?}, got None.",
            index, expected
        ));
        assert_eq!(expected, parsed, "Failed at {}", index);
    }
}
