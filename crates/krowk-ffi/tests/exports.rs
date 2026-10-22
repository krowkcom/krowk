//! R-FFI-1, R-PROTO-2: the exports answer what krowk-client does, not a
//! copy of it — the protocol version is krowk-client's constant, and a
//! pairing code is parsed by krowk-client's parser.

use krowk_ffi::{KrowkError, PairingInvite, parse_pairing_code, parse_pairing_invite, protocol_version};

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

/// Ticket 68's own sample, as `krowk devices add` draws it.
const INVITE: &str = "krowk://pair?v=1&code=K7QF9M3X&user=0192f5a0-7e1c-7cc3-9d1a-5b6f00000001&device=0123456789abcdef0123456789abcdef";

#[test]
fn r_e2e_3_r_client_1_a_scanned_invite_is_read_by_krowk_clients_parser() {
    let invite = parse_pairing_invite(INVITE.into()).expect("the sample is an invite");
    assert_eq!(
        invite,
        PairingInvite { code: "K7QF-9M3X".into(), user_id: "0192f5a0-7e1c-7cc3-9d1a-5b6f00000001".into(), device: "0123456789abcdef0123456789abcdef".into() }
    );
    // The code it carries is one the join takes.
    assert_eq!(parse_pairing_code(invite.code).unwrap(), "K7QF-9M3X");
}

#[test]
fn r_e2e_3_anything_but_a_krowk_invite_is_none() {
    let not = [
        INVITE.replacen("krowk://", "https://", 1),
        INVITE.replacen("v=1", "v=2", 1),
        INVITE.replacen("&user=0192f5a0-7e1c-7cc3-9d1a-5b6f00000001", "", 1),
        INVITE.replacen("&device=", "&code=K7QF9M3X&device=", 1),
        format!("{INVITE}&device=0123456789abcdef0123456789abcdef"),
        INVITE.replacen("0123456789abcdef0123456789abcdef", "0123456789abcdef0123456789abcde", 1),
        "https://krowk.com/pair?v=1&code=K7QF9M3X".into(),
        "K7QF-9M3X".into(),
        String::new(),
    ];
    for uri in not {
        assert_eq!(parse_pairing_invite(uri.clone()), None, "{uri}");
    }
}
