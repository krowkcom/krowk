//! D9, R-REN-10: the names that kept krowk when the product became gruntas.
//!
//! Historical, and never renamed: sealed data, signatures, the registry and
//! other devices depend on each of these byte for byte. Every one is the
//! constant the code uses, spelled out here once more, so a rename of any
//! of them fails this test instead of breaking what was written before it.
//! A change here is a protocol change, not a rename. (`krowk://pair`, the
//! phone's pairing invite, left with the phone code; nothing here builds it.)
//! scripts/name_sweep.allow lets the same names through the krowk sweep.

#![cfg(all(feature = "harness", unix))]

use gruntas::runctx::{self, Link, Metadata};
use gruntas_api::client as registry;
use gruntas_client::{device_chain, e2e, pairing, recovery, relay_link, relay_ticket, session_record, user_key};
use gruntas_harness::connect;
use gruntas_harness::sync::{handoff, mdns};

#[test]
fn d9_the_domain_separation_labels_are_unchanged() {
    let labels: &[(&[u8], &[u8])] = &[
        (pairing::LABEL, b"krowk/pair/v1"),
        (pairing::CONFIRM_KEY, b"krowk/pair-confirm/v1"),
        (pairing::SEAL_KEY, b"krowk/pair-seal/v1"),
        (pairing::MAC_B_CONFIRM, b"krowk/pair/v1 b-confirm"),
        (pairing::MAC_A_REPLY, b"krowk/pair/v1 a-reply"),
        (pairing::MAC_B_ACK, b"krowk/pair/v1 b-ack"),
        (pairing::REPLY_AD, b"krowk/pair/v1 reply"),
        (registry::REGISTRY_V1_LABEL.as_bytes(), b"krowk/registry/v1"),
        (registry::REGISTRY_V2_LABEL.as_bytes(), b"krowk/registry/v2"),
        (user_key::WRAP_INFO, b"krowk/user-key/v1"),
        (user_key::ID_LABEL, b"krowk/user-key-id/v1"),
        (user_key::CHAIN_AAD, b"krowk/user-key-chain/v1"),
        (e2e::SESSION_KEY_AAD, b"krowk/session-key/v2"),
        (e2e::SESSION_KEYS_AAD, b"krowk/session-keys/v3"),
        (e2e::LABEL_DEVICE_ID_V1, b"krowk/device-id/v1"),
        (e2e::LABEL_ACCOUNT_KEY_ID_V1, b"krowk/account-key-id/v1"),
        (e2e::LABEL_ACCOUNT_KEY_V1, b"krowk/account-key/v1"),
        (e2e::LABEL_SESSION_INDEX_V1, b"krowk/session-index/v1"),
        (e2e::LABEL_VINTAGE_V2, b"krowk/vintage/v2"),
        (e2e::LABEL_VINTAGE_KEY_V2, b"krowk/vintage-key/v2"),
        (e2e::LABEL_PRESENCE_V1, b"krowk/presence/v1"),
        (e2e::LABEL_PRESENCE_V1_KEY, b"krowk/presence/v1-key"),
        (e2e::LABEL_HOST_COMMAND_V1, b"krowk/host-command/v1"),
        (e2e::LABEL_HOST_COMMAND_V1_KEY, b"krowk/host-command/v1-key"),
        (e2e::LABEL_HOST_ANSWER_V1, b"krowk/host-answer/v1"),
        (e2e::LABEL_HOST_ANSWER_V1_KEY, b"krowk/host-answer/v1-key"),
        (e2e::LABEL_TRANSPORT_V1, b"krowk/transport/v1"),
        (e2e::LABEL_IMAGE_NAME_V1, b"krowk/image-name/v1"),
        (e2e::LABEL_IMAGE_V1, b"krowk/image/v1"),
        (e2e::LABEL_CHUNK_EPOCH_V1, b"krowk/chunk-epoch/v1"),
        (e2e::LABEL_CHUNK_V1, b"krowk/chunk/v1"),
        (e2e::LABEL_CHUNK_V2, b"krowk/chunk/v2"),
        (e2e::LABEL_FRAME_V1, b"krowk/frame/v1"),
        (e2e::RELAY_JOIN_LABEL, b"krowk/relay/v1"),
        (relay_link::HELLO_LABEL, b"krowk/relay-hello/v1"),
        (relay_ticket::LABEL, b"krowk/relay-ticket/v1"),
        (recovery::INFO, b"krowk/recovery-device/v1"),
        (session_record::LABEL, b"krowk/session-record/v1"),
        (device_chain::SIGNATURE_LABEL, b"krowk/device-list/v1"),
        (mdns::TAG_LABEL, b"krowk mdns v1"),
    ];
    for (got, want) in labels {
        assert_eq!(got, want, "{} changed", String::from_utf8_lossy(want));
    }
}

#[test]
fn d9_the_headers_prefixes_and_discovery_name_are_unchanged() {
    let names = [
        (registry::DEVICE_HEADER, "X-Krowk-Device"),
        (registry::TIMESTAMP_HEADER, "X-Krowk-Timestamp"),
        (registry::SIGNATURE_HEADER, "X-Krowk-Signature"),
        (registry::SIGNATURE_VERSION_HEADER, "X-Krowk-Signature-Version"),
        (relay_ticket::HEADER, "x-krowk-ticket"),
        (gruntas_api::API_KEY_PREFIX, "krowk_sk_"),
        (gruntas_api::CLAIM_TOKEN_PREFIX, "krowk_claim_"),
        (mdns::SERVICE, "_krowk._tcp.local."),
        (connect::PROVIDER_PREFIX, "krowk-"),
        (handoff::JOB_REFS, "refs/krowk/jobs/"),
    ];
    for (got, want) in names {
        assert_eq!(got, want, "{want} changed");
    }
}

#[test]
fn d9_the_registry_metadata_fields_are_unchanged() {
    let mut m = Metadata::default();
    m.dirty = Some(true);
    m.harness = "h".into();
    m.client = "c".into();
    m.change_url = "https://example.com/pr/1".into();
    m.session = "s".into();
    m.links = vec![Link { url: "https://example.com".into(), ..Link::default() }];
    m.references = vec!["r".into()];
    let sent = serde_json::to_value(&m).unwrap();
    let mut fields: Vec<&str> = sent.as_object().unwrap().keys().map(String::as_str).filter(|k| k.starts_with("krowk.")).collect();
    fields.extend([runctx::CAPTION_KEY, runctx::ENGINE_KEY]);
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["krowk.caption", "krowk.change.url", "krowk.client", "krowk.engine", "krowk.harness", "krowk.links", "krowk.references", "krowk.session", "krowk.vcs.dirty"]
    );
}
