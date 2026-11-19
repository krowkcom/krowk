//! `billing`: the workspace's plan, bought or managed in the dashboard. On
//! Free it opens the plan chooser; on a paid plan the billing route, which
//! hands an owner or admin to Polar's customer portal. A member is told who
//! to ask. krowk never talks to Polar itself: it only opens the page.

use super::agent::new_client;
use super::auth::{headless, open_browser};
use super::Ctx;
use crate::output::{self, Format};
use krowk_api::{fail, Error};
use serde_json::json;

pub(crate) fn billing(ctx: &mut Ctx, args: &[String]) -> Result<(), Error> {
    if let Some(first) = args.first() {
        return Err(fail("unexpected_argument", format!("`krowk billing` takes no arguments, and got `{first}`")));
    }
    let client = new_client(ctx)?;
    if !client.authenticated() {
        return Err(fail("not_authenticated", "billing is a workspace's — run `krowk login` first"));
    }
    let key = client.verify_key()?;
    // Only an owner's or admin's key may: a member's names who to ask, and
    // a key with no person behind it (or a registry that does not say)
    // names nobody.
    if key.can_manage_billing != Some(true) {
        let name = if key.workspace_name.is_empty() { &key.workspace } else { &key.workspace_name };
        let fix = if key.can_manage_billing.is_none() {
            format!("Only owners and admins can manage billing for {name}, and this key does not belong to one. Log in as an owner or admin, then try again.")
        } else if key.billing_contacts.is_empty() {
            format!("Billing for {name} is managed by its owners and admins. Ask one of them.")
        } else {
            format!("Billing for {name} is managed by {}. Ask one of them.", key.billing_contacts.join(", "))
        };
        let mut e = fail("billing_not_permitted", fix);
        e.body.insert("workspace".into(), json!(key.workspace));
        e.body.insert("billing_contacts".into(), json!(key.billing_contacts));
        return Err(e);
    }
    // Free, or a registry that does not say: the chooser. A paid plan: the
    // route that redirects to a fresh portal session.
    let paid = !key.plan.is_empty() && key.plan != "free";
    let url = format!("{}/{}", app_url(&client.base_url), if paid { "billing" } else { "plans" });
    let opened = !ctx.f.no_browser && !headless(ctx) && open_browser(&url);
    let what = if paid { "manage the subscription" } else { "choose a plan" };
    if ctx.format != Format::Human {
        let data = json!({ "url": url, "opened": opened, "plan": key.plan });
        let rendered = if ctx.f.quiet { output::encode(&data) } else { output::encode(&json!({ "ok": true, "data": data, "summary": what })) };
        return ctx.emit(&rendered);
    }
    let said = if opened { format!("Opened {url} to {what}") } else { format!("Open {url} to {what}") };
    ctx.emit(&said)
}

/// The dashboard beside a registry: the API's origin, its `api.` host
/// swapped for `app.` (api.krowk.com → app.krowk.com). A registry on any
/// other host serves its dashboard itself, as the local stand-in does.
fn app_url(base_url: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("https", base_url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.strip_prefix("api.").map_or_else(|| host.to_string(), |h| format!("app.{h}"));
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod tests {
    use super::app_url;

    #[test]
    fn r_bill_6_the_dashboard_sits_beside_the_api() {
        assert_eq!(app_url("https://api.krowk.com/v1"), "https://app.krowk.com");
        assert_eq!(app_url("http://localhost:8787/v1"), "http://localhost:8787");
        assert_eq!(app_url("https://registry.example.com/v1/"), "https://registry.example.com");
    }
}
