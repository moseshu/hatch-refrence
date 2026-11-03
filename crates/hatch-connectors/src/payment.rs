//! Payment — explicitly deferred, by scope decision (2026-11-03).
//!
//! There is no wallet, no checkout and no pay/submit action anywhere in
//! this codebase. The only payment behaviour that exists is refusal:
//!
//! * the policy layer already classifies spending as
//!   `ActionClass::Payment` with an autonomy ceiling of `Suggest`, and
//!   the gatekeeper turns `Suggest` into `Decision::Deny` ("payment is
//!   only a suggestion, never executed" — the assistant may suggest, the
//!   user must execute it themselves). Browser checkout clicks are
//!   mapped to the same class (`ClickKind::Payment`), so a checkout
//!   proposed through the browser, an MCP tool or a plan step is refused
//!   by the same gate — there is no bypass.
//! * this module is the **extension point** for a future provider: it
//!   holds the request shape and a `request_payment` that always denies.
//!   Nothing here touches the network, the vault, or any config.
//!
//! Plugging in a real payment provider later (do not do this until the
//! user asks for it):
//!
//! 1. Write a provider connector: a subprocess CLI speaking the runner
//!    protocol (`framework::runner`), exposing the provider's charge /
//!    refund ops. No wallet logic in the agent loop.
//! 2. Add its manifest `$HATCH_HOME/connectors/payment.toml` — `name =
//!    "payment"`, `token_ref` naming the vault token; the runner injects
//!    the value via the vault helper, exactly like CalDAV does.
//! 3. Register the connector name (`CONNECTOR_NAME`) with the registry;
//!    `hatch doctor` reports its status check like any other connector.
//! 4. Only as a deliberate product decision, raise the `Payment`
//!    autonomy ceiling in `hatch-policy` — until then the gatekeeper
//!    keeps denying every payment, including one from a connected
//!    provider. Steps 1–3 alone cannot execute a payment.
//!
//! The agent loop and the gatekeeper need no changes for any of this.

use hatch_core::{Error, Result};

/// The name a future payment-provider connector registers under.
pub const CONNECTOR_NAME: &str = "payment";

/// Why every payment request is refused. Keep the English sentence
/// first: it is the scope decision verbatim.
pub const DENIAL_REASON: &str = "payment is only a suggestion, never executed — 支付只能由助手提出建议，必须由你本人执行；本机未接入任何支付服务。";

/// A payment request, as data only. There is no execution path for it:
/// [`request_payment`] always denies, whatever is in here.
#[derive(Clone, Debug)]
pub struct PaymentRequest {
    /// Amount in the currency's minor unit (e.g. cents), as a string so
    /// no float rounding is ever involved.
    pub amount_minor: String,
    /// ISO 4217 code, e.g. `"USD"`.
    pub currency: String,
    /// Who would be paid.
    pub payee: String,
    /// Human-readable description, shown in the suggestion.
    pub description: String,
}

/// Always denies. This is the whole implementation until a provider is
/// plugged in (see the module docs): no network, no vault, no config,
/// no side effect of any kind.
pub fn request_payment(_req: &PaymentRequest) -> Result<()> {
    Err(Error::Denied(DENIAL_REASON.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> PaymentRequest {
        PaymentRequest {
            amount_minor: "45000".into(),
            currency: "USD".into(),
            payee: "example merchant".into(),
            description: "test charge".into(),
        }
    }

    #[test]
    fn payment_is_only_a_suggestion_never_executed() {
        let err = request_payment(&req()).unwrap_err();
        assert!(err.to_string().contains(DENIAL_REASON), "{err}");
        assert!(err.to_string().contains("only a suggestion"), "{err}");
    }

    #[test]
    fn denial_reason_matches_the_gatekeeper_wording() {
        // The gatekeeper denies ActionClass::Payment with the same
        // "suggest only, the user executes it" rule; the stub must not
        // drift into softer wording.
        assert!(DENIAL_REASON.contains("只能由助手提出建议，必须由你本人执行"));
        assert_eq!(CONNECTOR_NAME, "payment");
    }
}
