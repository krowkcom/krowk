//! R-BILL-6: `gruntas billing` opens the plan chooser on Free and the billing
//! route on a paid plan, and refuses a member with who to ask. Against the
//! stand-in registry, whose key markers fake the plan and role; with
//! `--no-browser`, so the page is printed rather than opened, whatever
//! display the machine has (macOS always has one).

use serde_json::Value;
use std::net::TcpListener;
use std::process::{Command, Output};

fn billing(home: &std::path::Path, api: &str, token: &str, format: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gruntas"))
        .args(["billing", "--no-browser", "--format", format])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GRUNTAS_API_URL", api)
        .env("GRUNTAS_NO_UPDATE_CHECK", "1")
        .env("GRUNTAS_TOKEN", token)
        .current_dir(home)
        .output()
        .unwrap()
}

#[test]
fn r_bill_6_billing_opens_the_page_for_the_plan_or_names_who_to_ask() {
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
    let home = std::env::temp_dir().join(format!("gruntas-billing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let api = format!("{}/v1", registry.url());
    let site = registry.url().trim_end_matches('/').to_string();
    let text = |o: &Output| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));

    // Owners and admins: the chooser on Free, the billing route when paid;
    // with no browser to open, the page is printed.
    for (token, page) in [("krowk_sk_free_owner", "plans"), ("krowk_sk_pro_owner", "billing"), ("krowk_sk_business_owner", "billing")] {
        let out = billing(&home, &api, token, "human");
        assert!(out.status.success(), "{token}: {}", text(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("Open {site}/{page} to {}", if page == "plans" { "choose a plan" } else { "manage the subscription" }));
        let out = billing(&home, &api, token, "json");
        let data = &serde_json::from_slice::<Value>(&out.stdout).unwrap()["data"];
        assert_eq!((data["url"].as_str(), data["opened"].as_bool()), (Some(format!("{site}/{page}").as_str()), Some(false)), "{data}");
    }

    // A member, on any plan: refused, exit 4, told who to ask.
    for token in ["krowk_sk_pro_member", "krowk_sk_free_member"] {
        let out = billing(&home, &api, token, "human");
        assert_eq!(out.status.code(), Some(4), "{}", text(&out));
        let said = text(&out);
        assert!(said.contains("Billing for Local workspace is managed by owner@example.test. Ask one of them."), "{said}");
        assert!(!said.contains("Billing contacts"), "the contacts are said once: {said}");
        let out = billing(&home, &api, token, "json");
        assert_eq!(out.status.code(), Some(4));
        let body: Value = serde_json::from_slice(&out.stderr).unwrap();
        let error = &body["error"];
        assert_eq!((body["ok"].as_bool(), error["error"].as_str()), (Some(false), Some("billing_not_permitted")), "{body}");
        assert_eq!(error["billing_contacts"], serde_json::json!(["owner@example.test"]), "{body}");
    }

    let _ = std::fs::remove_dir_all(&home);
}
