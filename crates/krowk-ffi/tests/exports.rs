//! R-FFI-1, R-PROTO-2: the exports answer what krowk-client does, not a
//! copy of it — the protocol version is krowk-client's constant, and a
//! pairing code is parsed by krowk-client's parser.

use krowk_ffi::{KrowkError, parse_pairing_code, protocol_version};

#[test]
fn r_proto_2_protocol_version_is_krowk_clients() {
    assert_eq!(protocol_version(), krowk_client::protocol::PROTOCOL_VERSION);
}

#[test]
fn r_ffi_1_a_pairing_code_is_given_back_canonical() {
    assert_eq!(parse_pairing_code("abcd efgh".into()).unwrap(), "ABCD-EFGH");
    assert_eq!(parse_pairing_code("ABCD-EFGH".into()).unwrap(), "ABCD-EFGH");
}

#[test]
fn r_ffi_1_a_bad_pairing_code_is_invalid_input() {
    // Too short, and a `0`, which the alphabet leaves out
    // (23456789ABCDEFGHJKMNPQRSTVWXYZ: no I, L, O, U, 0 or 1).
    for text in ["abc", "ABCD-EFG0"] {
        match parse_pairing_code(text.into()) {
            Err(KrowkError::InvalidInput { detail }) => {
                assert!(detail.contains("not a pairing code"), "{text}: {detail}")
            }
            other => panic!("{text}: {other:?}"),
        }
    }
}
