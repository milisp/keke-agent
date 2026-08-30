//! Credentials, model lists, and what keke thinks its own setup is.

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialRef;
use keke_config::Config;

use crate::api_key::ApiKeyAuth;
use crate::cli::LoginArgs;
use crate::cli::VendorArgs;
use crate::compose::Composed;
use crate::ui::TerminalLoginUi;

pub(super) async fn login(args: LoginArgs, composed: Composed) -> Result<()> {
    let auth = composed
        .auth
        .get(&args.vendor)
        .with_context(|| format!("unknown vendor `{}`", args.vendor))?;

    auth.login(Arc::new(TerminalLoginUi)).await?;

    let snapshot = auth.snapshot();
    println!("Signed in to {} via {}.", args.vendor, snapshot.source);
    if let Some(account) = snapshot.account_id {
        println!("  account: {account}");
    }
    Ok(())
}

pub(super) async fn logout(args: VendorArgs, composed: Composed) -> Result<()> {
    let auth = composed
        .auth
        .get(&args.vendor)
        .with_context(|| format!("unknown vendor `{}`", args.vendor))?;
    auth.logout().await?;

    // An imported credential belongs to another tool's file, which keke will
    // not write. Reporting a clean sign-out while the next request still
    // authenticates would be a lie the user only discovers later.
    if auth.has_usable_credential() {
        let source = auth.snapshot().source;
        println!(
            "Removed keke's stored credentials for {}, but it is still signed in \n\
             through a credential keke does not own ({source}). keke reads the \n\
             codex and grok CLIs' logins and never writes to their files — sign \n\
             out with that tool to remove it.",
            args.vendor
        );
    } else {
        println!("Signed out of {}.", args.vendor);
    }
    Ok(())
}

pub(super) async fn models(args: VendorArgs, composed: Composed) -> Result<()> {
    let provider = composed.providers.get(&args.vendor)?;
    let models = provider.list_models().await?;
    if models.is_empty() {
        println!("{} does not publish a model list.", args.vendor);
        return Ok(());
    }
    for model in models {
        println!("{}", model.id);
        // Indented under the id rather than beside it: the id is what gets
        // typed and copied, and a line that starts with it stays greppable.
        if model.display_name != model.id {
            println!("  {}", model.display_name);
        }
        if let Some(description) = &model.description {
            println!("  {description}");
        }
        if let Some(window) = model.context_window {
            println!("  context: {window} tokens");
        }
        if model.supports_reasoning() {
            let levels: Vec<&str> = model
                .reasoning_efforts
                .iter()
                .map(|effort| effort.as_str())
                .collect();
            let default = model
                .starting_effort()
                .map_or_else(String::new, |effort| format!(" (default: {effort})"));
            println!("  effort: {}{default}", levels.join(", "));
        }
    }
    Ok(())
}

pub(super) fn doctor(config: Config, composed: Composed) -> Result<()> {
    println!("workspace: {}", config.home.workspace_root);
    println!("home:      {}", config.home.home);
    println!(
        "model:     {} / {}",
        config.model.provider, config.model.model
    );

    println!("\nconfig layers:");
    if config.sources.is_empty() {
        println!("  (none; all defaults)");
    }
    for source in &config.sources {
        println!("  {}", source.describe());
    }

    println!("\nproviders:");
    let routes: Vec<String> = composed.providers.routes().map(str::to_string).collect();
    for route in routes {
        println!("  {route}: {}", credential_status(&composed, &route));
    }

    Ok(())
}

/// Describe a provider's credentials in terms of what to do about them.
///
/// "Not configured" is only useful when it says what would configure it, so a
/// route with a login flow points at `keke login` and a key-only endpoint names
/// the variable to export.
fn credential_status(composed: &Composed, route: &str) -> String {
    if let Some(auth) = composed.auth_for(route) {
        return if auth.has_usable_credential() {
            format!("signed in ({})", auth.snapshot().source)
        } else {
            format!("not signed in — run `keke login {route}`")
        };
    }

    // No registered login flow: the endpoint takes an API key, and
    // `ProviderInfo::env_key` names it. Ask through the credential store rather
    // than the process environment — the key may be in the keyring or the file
    // layer, which reading `env` directly would miss.
    let Ok(provider) = composed.providers.get(route) else {
        return "unknown".to_string();
    };
    let Some(key) = provider.info().env_key.as_deref() else {
        return "no credentials required".to_string();
    };
    let Ok(reference) = CredentialRef::new(key) else {
        return format!("`{key}` is not a usable credential name");
    };

    let auth = ApiKeyAuth::new(reference, Arc::clone(&composed.credentials));
    if auth.has_usable_credential() {
        format!("{key} is set")
    } else {
        format!("not configured — export {key}")
    }
}
