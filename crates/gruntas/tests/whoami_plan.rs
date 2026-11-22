//! R-BILL-4: `krowk whoami` names the workspace's plan, and an owner or
//! admin of a paid plan when it renews or ends; `--json` carries the same.
//! Against the stand-in registry, whose key markers fake the plan and role.

use serde_json::Value;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

struct Gruntas {
    home: PathBuf,
    api: String,
}

impl Gruntas {
    fn whoami(&self, token: &str, json: bool) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_krowk"));
        cmd.arg("whoami")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GRUNTAS_API_URL", &self.api)
            .env("GRUNTAS_NO_UPDATE_CHECK", "1")
            .env("GRUNTAS_TOKEN", token)
            .current_dir(&self.home);
        // Piped, krowk answers in JSON unless asked for the human form.
        cmd.args(["--format", if json { "json" } else { "human" }]);
        let out = cmd.output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "krowk whoami failed:\n{text}");
        text
    }
}

#[test]
fn r_bill_4_whoami_shows_the_plan_and_dates_only_to_who_manages_billing() {
    let registry = gruntas_devregistry::start(TcpListener::bind("127.0.0.1:0").unwrap(), gruntas_devregistry::Config::default()).unwrap();
    let home = std::env::temp_dir().join(format!("krowk-whoami-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let krowk = Gruntas { home: home.clone(), api: format!("{}/v1", registry.url()) };

    let plan = |token: &str| {
        let out = krowk.whoami(token, false);
        out.lines().find(|l| l.trim_start().starts_with("Plan")).map(|l| l.trim_start()["Plan".len()..].trim().to_string()).unwrap_or_else(|| panic!("no plan line:\n{out}"))
    };
    assert_eq!(plan("krowk_sk_free_owner"), "Free");
    assert!(plan("krowk_sk_pro_owner").starts_with("Pro · renews "));
    assert!(plan("krowk_sk_pro_ending_owner").starts_with("Pro · ends "));
    assert!(plan("krowk_sk_business_owner").starts_with("Business · renews "));
    assert_eq!(plan("krowk_sk_pro_member"), "Pro");
    assert_eq!(plan("krowk_sk_business_member"), "Business");

    let json = |token: &str| -> Value { serde_json::from_str::<Value>(&krowk.whoami(token, true)).unwrap()["data"].clone() };
    let owner = json("krowk_sk_pro_owner");
    assert_eq!((owner["plan"].as_str(), owner["can_manage_billing"].as_bool()), (Some("pro"), Some(true)), "{owner}");
    assert!(owner["plan_renews_at"].is_string() && owner["plan_ends_at"].is_null(), "{owner}");
    let ending = json("krowk_sk_pro_ending_owner");
    assert!(ending["plan_ends_at"].is_string() && ending["plan_renews_at"].is_null(), "{ending}");
    let member = json("krowk_sk_pro_member");
    assert_eq!((member["plan"].as_str(), member["can_manage_billing"].as_bool()), (Some("pro"), Some(false)), "{member}");
    assert!(member["plan_renews_at"].is_null() && member["plan_ends_at"].is_null(), "{member}");
    let free = json("krowk_sk_free_owner");
    assert_eq!((free["plan"].as_str(), free["can_manage_billing"].as_bool()), (Some("free"), Some(true)), "{free}");
    assert!(free["plan_renews_at"].is_null(), "{free}");

    let _ = std::fs::remove_dir_all(&home);
}
