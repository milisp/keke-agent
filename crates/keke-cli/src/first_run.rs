//! A one-time picker shown the first time `keke` opens the TUI with no
//! `$KEKE_HOME/config.toml` on disk, so a fresh install doesn't silently land
//! on whatever [`DEFAULT_PROVIDER`](keke_config) happens to be this week.
//!
//! It settles the four things a first request needs — how you authenticate,
//! which endpoint, which model, how hard it thinks — and writes all of them,
//! because a picker that records only the vendor leaves the rest answered by
//! compiled-in constants the person never saw.
//!
//! The credential is asked for *first*, because it is the answer a person
//! actually has: one arrives holding an API key, another holding a ChatGPT or
//! grok subscription, and which endpoints are reachable follows from that
//! rather than the other way round. Both lists are read off the registry —
//! a vendor that gains a login flow, or a plugin that adds an endpoint, shows
//! up here without this file changing.
//!
//! A vendor is not limited to the ones compiled in either: the last entry
//! under "API key" declares an endpoint, which is the same `[providers.*]`
//! table a person would have written by hand.
//!
//! Gated on `Command::Tui` plus [`is_interactive`] — `keke exec` and friends
//! never see this, so scripts and CI keep running against the hardcoded
//! default exactly as before.

use std::io::Write;
use std::io::stdin;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use keke_auth_api::CredentialRef;
use keke_config::Config;
use keke_config::LayerSource;
use keke_config_types::DeclaredWireApi;
use keke_config_types::ProviderDeclaration;
use keke_config_types::ReasoningEffort;
use keke_provider_api::ModelInfo;

use crate::compose::Composed;
use crate::ui::TerminalLoginUi;

/// Whether the picker changed something the composition root already read.
///
/// A declared endpoint is registered at composition time, so a person who adds
/// one here would otherwise have to restart before their own answer worked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Unchanged,
    /// `config.providers` gained an entry; rebuild before using the registry.
    ProvidersChanged,
    /// The picker ended without a usable credential — nothing was written, and
    /// the surface must not open.
    ///
    /// Starting the interface anyway is worse than stopping: the first thing a
    /// person would do in it is send a message, and the only possible outcome
    /// is an authentication error several screens away from the question they
    /// just abandoned.
    Abandoned,
}

/// The levels offered when the endpoint publishes none of its own — a plain
/// `/models` listing is a bag of ids and says nothing about thinking.
///
/// `Ultra` is left out: the vendors that take it take it on a handful of
/// models, and offering it for a model that has not claimed it would make a
/// fresh install's most likely first request an error.
const FALLBACK_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

/// The wire formats a declared endpoint can speak, with what to call them.
const WIRES: &[(DeclaredWireApi, &str)] = &[
    (
        DeclaredWireApi::ChatCompletions,
        "chat_completions (OpenAI-compatible: Ollama, vLLM, most gateways)",
    ),
    (DeclaredWireApi::Responses, "responses (OpenAI)"),
    (DeclaredWireApi::Messages, "messages (Anthropic)"),
];

/// Local servers common enough that typing their address is friction rather
/// than configuration.
///
/// Presets, not support: each one becomes an ordinary `[providers.*]` table
/// with a base URL the person can edit afterwards, and nothing in the engine
/// knows these names. Anything not listed is the same three questions under
/// "Something else".
const LOCAL_PRESETS: &[(&str, &str, &str)] = &[
    ("ollama", "Ollama", "http://localhost:11434/v1"),
    ("lmstudio", "LM Studio", "http://localhost:1234/v1"),
    ("vllm", "vLLM", "http://localhost:8000/v1"),
];

/// One route as the picker needs to describe it.
struct Route {
    route: String,
    display_name: String,
    /// The variable its key is filed under, when it accepts one.
    env_key: Option<String>,
    /// Whether `keke login` has something to open for it.
    has_login: bool,
}

/// What the picker settled about the endpoint, before the model is asked for.
struct Picked {
    route: String,
    /// Present only when the person described a new endpoint.
    declaration: Option<ProviderDeclaration>,
}

/// Whether the endpoint being described is reached with a credential.
///
/// The answer is already known before the questions start — it is the branch
/// the person took at the first prompt — so a blank credential name under
/// "API key" is a contradiction rather than a choice, and asking it as one
/// writes a declaration that authenticates with nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialNeed {
    /// A local server: no credential, and demanding one would make the
    /// commonest declared provider unconfigurable here.
    None,
    /// An API key was the answer to "how do you want to authenticate?".
    Required,
}

/// Ask how to authenticate, which endpoint, which model, and how hard it
/// thinks, then persist all of it — but only on a fresh install's first
/// interactive run.
pub(crate) async fn maybe_run(config: &mut Config, composed: &Composed) -> Result<Outcome> {
    let already_configured = config
        .sources
        .iter()
        .any(|source| matches!(source, LayerSource::User(_)));
    if already_configured {
        return Ok(Outcome::Unchanged);
    }

    let routes = routes(composed);
    let Some(picked) = pick(&routes, composed).await? else {
        return Ok(Outcome::Abandoned);
    };

    // Asked after the credential was supplied rather than assumed from the
    // supplying: a login flow can return having minted nothing, and a key
    // prompt can be answered with a deliberate blank.
    if let Some(guidance) = unusable(&picked, composed) {
        println!("\n{guidance}");
        return Ok(Outcome::Abandoned);
    }

    let Some(model) = pick_model(&picked, composed, config).await else {
        return Ok(Outcome::Abandoned);
    };

    // The levels this model actually accepts, not a fixed menu: a level
    // invented here is one a person could select and the endpoint would then
    // reject, several screens away from this prompt.
    let efforts: Vec<ReasoningEffort> = if model.reasoning_efforts.is_empty() {
        FALLBACK_EFFORTS.to_vec()
    } else {
        model.reasoning_efforts.clone()
    };
    // keke's preference is `high`, but only where the model takes it; where it
    // does not, the vendor's own starting point is a better answer than the
    // nearest rung keke would have picked for it.
    let preferred = if efforts.contains(&ReasoningEffort::High) {
        ReasoningEffort::High
    } else {
        model
            .default_reasoning_effort
            .filter(|effort| efforts.contains(effort))
            .or_else(|| efforts.last().copied())
            .unwrap_or(ReasoningEffort::High)
    };

    println!();
    for (index, effort) in efforts.iter().enumerate() {
        println!("  {}) {}", index + 1, effort.as_str());
    }
    let Some(effort) = choose(
        "Reasoning effort",
        efforts.len(),
        preferred.as_str(),
        |index| efforts[index],
        || preferred,
    ) else {
        return Ok(Outcome::Abandoned);
    };
    let model = model.id;

    config.model.provider = picked.route.clone();
    config.model.model = model.clone();
    config.reasoning_effort = Some(effort);
    let outcome = if let Some(declared) = &picked.declaration {
        config.providers.push(declared.clone());
        Outcome::ProvidersChanged
    } else {
        Outcome::Unchanged
    };
    persist(config, picked.declaration.as_ref())?;
    println!(
        "\nUsing {} with {model} at {} effort — change it any time in config.toml.\n",
        picked.route,
        effort.as_str()
    );
    Ok(outcome)
}

/// Offer the endpoint's own catalog, falling back to typing an id.
///
/// The catalog is what the vendor publishes — ids, display names, and the
/// levels each model thinks at — so asking a person to remember `gpt-5.6-sol`
/// when the endpoint will happily list it is friction, not configuration. A
/// listing that fails or comes back empty is not an error: plenty of endpoints
/// serve models they do not enumerate, and typing the id still works.
async fn pick_model(picked: &Picked, composed: &Composed, config: &Config) -> Option<ModelInfo> {
    // A declared endpoint is not registered until the config is written, so
    // there is nothing to ask yet.
    let listing = match (&picked.declaration, composed.providers.get(&picked.route)) {
        (None, Ok(handle)) => handle.list_models().await.unwrap_or_default(),
        _ => Vec::new(),
    };

    if listing.is_empty() {
        let suggestion = picked
            .declaration
            .as_ref()
            .and_then(|declared| declared.default_model.clone())
            .unwrap_or_else(|| default_model(&picked.route, config));
        let id = ask("Model", &suggestion)?;
        return (!id.is_empty()).then(|| ModelInfo::new(id));
    }

    println!("\nWhich model?\n");
    for (index, model) in listing.iter().enumerate() {
        let description = model
            .description
            .as_deref()
            .map(|line| format!(" — {line}"))
            .unwrap_or_default();
        println!(
            "  {}) {} ({}){description}",
            index + 1,
            model.display_name,
            model.id
        );
    }
    println!();

    // The compiled-in default first, when this endpoint serves it: on the
    // default provider that is the model keke would otherwise have chosen
    // silently, so it is the one to offer by name.
    let preferred = listing
        .iter()
        .position(|model| model.id == config.model.model)
        .unwrap_or(0);
    let chosen = choose(
        "Choice",
        listing.len(),
        &listing[preferred].id,
        |index| index,
        || preferred,
    )?;
    Some(listing[chosen].clone())
}

/// Why the chosen endpoint still cannot make a request, if it cannot.
///
/// An endpoint that wants no credential — a local server — is usable by
/// definition, so absence of a credential is only a problem where one was
/// asked for.
fn unusable(picked: &Picked, composed: &Composed) -> Option<String> {
    if let Some(declared) = &picked.declaration {
        // Not yet registered, so the declaration is what there is to ask.
        let env_key = declared.env_key.as_deref()?;
        return match CredentialRef::new(env_key) {
            Ok(reference)
                if composed
                    .credentials
                    .load(&reference)
                    .ok()
                    .flatten()
                    .is_some() =>
            {
                None
            }
            _ => Some(format!(
                "No {env_key} stored. Export it, then start keke again — \
                 your other answers were not saved."
            )),
        };
    }

    if let Some(auth) = composed.auth_for(&picked.route) {
        if auth.has_usable_credential() {
            return None;
        }
        return Some(format!(
            "Not signed in to {}. Run `keke login {}` and start keke again — \
             your other answers were not saved.",
            picked.route, picked.route
        ));
    }

    let env_key = composed
        .providers
        .get(&picked.route)
        .ok()
        .and_then(|handle| handle.info().env_key.clone())?;
    match CredentialRef::new(env_key.clone()) {
        Ok(reference)
            if composed
                .credentials
                .load(&reference)
                .ok()
                .flatten()
                .is_some() =>
        {
            None
        }
        _ => Some(format!(
            "No {env_key} stored. Export it, then start keke again — \
             your other answers were not saved."
        )),
    }
}

/// Everything the registry knows, in the shape the prompts need.
fn routes(composed: &Composed) -> Vec<Route> {
    composed
        .providers
        .routes()
        .filter_map(|route| {
            let handle = composed.providers.get(route).ok()?;
            let info = handle.info();
            Some(Route {
                route: route.to_string(),
                display_name: info.display_name.clone(),
                env_key: info.env_key.clone(),
                // Asked through the auth registry rather than through
                // `auth_id` alone: a route naming a login that is not
                // installed has no login to offer.
                has_login: composed.auth_for(route).is_some(),
            })
        })
        .collect()
}

/// Ask for the credential, then for the endpoint it unlocks.
///
/// `None` means the person answered nothing — the caller writes no config, so
/// the next run asks again rather than adopting a choice never made.
async fn pick(routes: &[Route], composed: &Composed) -> Result<Option<Picked>> {
    let logins: Vec<&Route> = routes.iter().filter(|route| route.has_login).collect();

    // Local last, because it is the answer fewest people arrive with — but
    // present, because a person running Ollama has no key to offer and would
    // otherwise read this menu as "keke needs an account".
    let local = logins.len() + 2;
    println!("How do you want to authenticate?\n");
    println!("  1) API key");
    for (index, route) in logins.iter().enumerate() {
        println!(
            "  {}) Sign in with {} ({})",
            index + 2,
            route.display_name,
            route.route
        );
    }
    println!("  {local}) Nothing — a local server (Ollama, LM Studio, vLLM)");
    println!();

    let Some(choice) = choose("Choice", local, "API key", |index| index, || 0) else {
        return Ok(None);
    };

    if choice == local - 1 {
        return pick_local(routes);
    }

    if choice > 0 {
        let route = logins[choice - 1];
        let Some(auth) = composed.auth_for(&route.route) else {
            return Ok(None);
        };
        if !auth.has_usable_credential() {
            println!("\nSigning in to {}.", route.display_name);
            auth.login(Arc::new(TerminalLoginUi)).await?;
        }
        return Ok(Some(Picked {
            route: route.route.clone(),
            declaration: None,
        }));
    }

    pick_key_endpoint(routes, composed)
}

/// A local server, which needs an address rather than a credential.
fn pick_local(routes: &[Route]) -> Result<Option<Picked>> {
    println!("\nWhich local server?\n");
    for (index, (_, name, base_url)) in LOCAL_PRESETS.iter().enumerate() {
        println!("  {}) {name} — {base_url}", index + 1);
    }
    println!("  {}) Something else", LOCAL_PRESETS.len() + 1);
    println!();

    let Some(index) = choose(
        "Choice",
        LOCAL_PRESETS.len() + 1,
        LOCAL_PRESETS[0].1,
        |index| index,
        || 0,
    ) else {
        return Ok(None);
    };

    if index == LOCAL_PRESETS.len() {
        let Some(declared) = declare(routes, CredentialNeed::None)? else {
            return Ok(None);
        };
        return Ok(Some(Picked {
            route: declared.route.clone(),
            declaration: Some(declared),
        }));
    }

    let (route, display_name, base_url) = LOCAL_PRESETS[index];
    // A preset's name can still be taken — by another preset already declared,
    // or by a plugin's route — and two providers claiming one route is an
    // error rather than a silent pick, so it is caught while it can be fixed.
    let route = if routes.iter().any(|existing| existing.route == route) {
        let Some(chosen) = ask("That name is taken; use", &format!("{route}-local")) else {
            return Ok(None);
        };
        chosen
    } else {
        route.to_string()
    };

    // Offered rather than assumed: the same server is just as often reached
    // over the network from another machine.
    let Some(base_url) = ask("Base URL", base_url) else {
        return Ok(None);
    };

    Ok(Some(Picked {
        route: route.clone(),
        declaration: Some(ProviderDeclaration {
            route,
            kind: None,
            account: None,
            display_name: Some(display_name.to_string()),
            base_url: Some(base_url),
            wire: Some(DeclaredWireApi::ChatCompletions),
            env_key: None,
            default_model: None,
            ca_cert_path: None,
            proxy: None,
            proxy_username: None,
            proxy_password_env_key: None,
            headers: Default::default(),
        }),
    }))
}

/// The endpoints an API key can be spent at, plus the option to name one keke
/// has never heard of.
fn pick_key_endpoint(routes: &[Route], composed: &Composed) -> Result<Option<Picked>> {
    let keyed: Vec<&Route> = routes
        .iter()
        .filter(|route| route.env_key.is_some())
        .collect();

    println!("\nWhich endpoint is the key for?\n");
    for (index, route) in keyed.iter().enumerate() {
        let key = route.env_key.as_deref().unwrap_or_default();
        println!(
            "  {}) {} ({}) — {key}",
            index + 1,
            route.display_name,
            route.route
        );
    }
    // Always last, and always present: without it the list reads as the set of
    // endpoints keke supports, which is not what it is.
    println!("  {}) Something else — any HTTP endpoint", keyed.len() + 1);
    println!();

    let Some(index) = choose(
        "Choice",
        keyed.len() + 1,
        &keyed
            .first()
            .map_or_else(|| "1".to_string(), |route| route.route.clone()),
        |index| index,
        || 0,
    ) else {
        return Ok(None);
    };

    if index == keyed.len() {
        let Some(declared) = declare(routes, CredentialNeed::Required)? else {
            return Ok(None);
        };
        // Stored now so the key is in place before the rebuilt registry reads
        // it; the route itself only exists once config is written.
        if let Some(env_key) = declared.env_key.clone() {
            let reference = CredentialRef::new(env_key.clone())
                .with_context(|| format!("the credential name for `{}`", declared.route))?;
            if !store_key(composed, &reference, &env_key)? {
                return Ok(None);
            }
        }
        return Ok(Some(Picked {
            route: declared.route.clone(),
            declaration: Some(declared),
        }));
    }

    let route = keyed[index];
    let env_key = route.env_key.clone().unwrap_or_default();
    let reference = CredentialRef::new(env_key.clone())
        .with_context(|| format!("the credential name for `{}`", route.route))?;
    if composed
        .credentials
        .load(&reference)
        .ok()
        .flatten()
        .is_some()
    {
        println!("\n{env_key} is already set — using it.");
    } else if !store_key(composed, &reference, &env_key)? {
        return Ok(None);
    }

    Ok(Some(Picked {
        route: route.route.clone(),
        declaration: None,
    }))
}

/// Ask for an endpoint keke has no compiled-in knowledge of.
///
/// The result is exactly a `[providers.<route>]` table, so what this writes is
/// what the documentation tells a person to write by hand — the picker is a
/// convenience over that file, never a second way of configuring keke.
fn declare(taken: &[Route], need: CredentialNeed) -> Result<Option<ProviderDeclaration>> {
    println!();
    let route = loop {
        let Some(route) = ask("Short name (e.g. `nvidia`, `ollama`)", "") else {
            return Ok(None);
        };
        if route.is_empty() {
            println!("A name is needed to select this endpoint later.");
        } else if taken.iter().any(|existing| existing.route == route) {
            println!("`{route}` is already a provider — pick another name.");
        } else {
            break route;
        }
    };

    let base_url = loop {
        let Some(url) = ask("Base URL (e.g. `http://localhost:11434/v1`)", "") else {
            return Ok(None);
        };
        if url.is_empty() {
            println!("An address is needed.");
        } else {
            break url;
        }
    };

    println!();
    for (index, (_, label)) in WIRES.iter().enumerate() {
        println!("  {}) {label}", index + 1);
    }
    let Some(wire) = choose(
        "Wire format",
        WIRES.len(),
        "chat_completions",
        |index| WIRES[index].0,
        || DeclaredWireApi::ChatCompletions,
    ) else {
        return Ok(None);
    };

    // A declaration with no credential name sends no `authorization` header at
    // all, and the endpoint's answer to that is a 401 whose body is the only
    // explanation the person gets. So it is refused where a key was the stated
    // answer, and offered only where none is wanted.
    let suggested = match need {
        CredentialNeed::None => String::new(),
        CredentialNeed::Required => suggested_env_key(&route),
    };
    if need == CredentialNeed::None {
        println!("\nLeave blank if this endpoint needs no credential.");
    } else {
        println!("\nThe variable your key is read from — it is not stored in config.toml.");
    }
    let env_key = loop {
        let Some(name) = ask("Credential variable (e.g. `NVIDIA_API_KEY`)", &suggested) else {
            return Ok(None);
        };
        if name.is_empty() {
            if need == CredentialNeed::None {
                break None;
            }
            println!(
                "This endpoint is reached with an API key, so it needs a variable to read it from."
            );
            continue;
        }
        match CredentialRef::new(name.clone()) {
            Ok(reference) => break Some(reference),
            Err(error) => println!("{error}"),
        }
    };

    let default_model = ask("Default model", "").unwrap_or_default();

    Ok(Some(ProviderDeclaration {
        route,
        kind: None,
        account: None,
        display_name: None,
        base_url: Some(base_url),
        wire: Some(wire),
        env_key: env_key.map(|reference| reference.to_string()),
        default_model: (!default_model.is_empty()).then_some(default_model),
        ca_cert_path: None,
        proxy: None,
        proxy_username: None,
        proxy_password_env_key: None,
        headers: Default::default(),
    }))
}

/// The variable name offered for a keyed endpoint, derived from its route —
/// `nvidia` becomes `NVIDIA_API_KEY`, which is what the vendor's own
/// documentation tells a person to export.
fn suggested_env_key(route: &str) -> String {
    let stem: String = route
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}_API_KEY")
}

/// The model offered by default for `provider`.
///
/// Only the compiled-in default provider has a model to suggest from config;
/// for anything else the merged value belongs to a different vendor and would
/// be a wrong answer presented as the safe one, so nothing is prefilled.
fn default_model(provider: &str, config: &Config) -> String {
    if provider == config.model.provider {
        config.model.model.clone()
    } else {
        String::new()
    }
}

/// Prompt for a key and file it under `reference`.
fn store_key(composed: &Composed, reference: &CredentialRef, env_key: &str) -> Result<bool> {
    println!("\nPaste the key, or press Enter to export {env_key} yourself later.");
    let Some(key) = ask("API key", "") else {
        return Ok(false);
    };
    if key.is_empty() {
        // Nothing stored on purpose: the variable still resolves at request
        // time, so this is a deferral rather than an unconfigured install.
        return Ok(true);
    }
    composed
        .credentials
        .save(reference, &key)
        .with_context(|| format!("storing {env_key}"))?;
    println!("Stored.");
    Ok(true)
}

/// Read one line, showing `default` when there is one.
///
/// `None` means stdin ended — not actually a terminal despite `is_interactive`
/// reporting one (e.g. `/dev/null` piped in) — and every caller treats that as
/// "ask again next time" rather than hanging or guessing.
fn ask(prompt: &str, default: &str) -> Option<String> {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{default}]: ");
    }
    std::io::stdout().flush().ok();

    let mut line = String::new();
    if stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let line = line.trim();
    Some(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

/// Read a 1-based choice out of `count`, re-asking until it is one.
fn choose<T>(
    prompt: &str,
    count: usize,
    default_label: &str,
    pick: impl Fn(usize) -> T,
    fallback: impl Fn() -> T,
) -> Option<T> {
    loop {
        print!("{prompt} [1-{count}, default {default_label}]: ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            return Some(fallback());
        }
        match line.parse::<usize>() {
            Ok(n) if n >= 1 && n <= count => return Some(pick(n - 1)),
            _ => println!("Enter a number between 1 and {count}."),
        }
    }
}

/// Write `$KEKE_HOME/config.toml` with what was chosen, so this picker never
/// runs again for this install.
fn persist(config: &Config, declared: Option<&ProviderDeclaration>) -> Result<()> {
    let home = config.home.home.as_path();
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let path = home.join("config.toml");

    let mut contents = format!(
        "provider = \"{}\"\nmodel = \"{}\"\n",
        config.model.provider, config.model.model
    );
    if let Some(effort) = config.reasoning_effort {
        contents.push_str(&format!("reasoning_effort = \"{}\"\n", effort.as_str()));
    }
    if let Some(declared) = declared {
        let table = toml::to_string(declared).context("rendering the declared provider")?;
        contents.push_str(&format!("\n[providers.{}]\n{table}", declared.route));
    }

    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the picker writes must be what the loader reads: a declaration
    /// rendered here and refused there would leave an install that never runs
    /// the picker again *and* cannot start.
    #[test]
    fn a_declared_endpoint_is_written_in_the_shape_config_parses() {
        let declared = ProviderDeclaration {
            route: "nvidia".to_string(),
            kind: None,
            account: None,
            display_name: None,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            wire: Some(DeclaredWireApi::ChatCompletions),
            env_key: Some("NVIDIA_API_KEY".to_string()),
            default_model: Some("qwen3-coder".to_string()),
            ca_cert_path: None,
            proxy: None,
            proxy_username: None,
            proxy_password_env_key: None,
            headers: Default::default(),
        };
        let table = toml::to_string(&declared).expect("renders");
        let document = format!("provider = \"nvidia\"\n\n[providers.nvidia]\n{table}");

        let file: keke_config::ConfigFile = toml::from_str(&document).expect("parses");
        let parsed = file.providers.get("nvidia").expect("declared");
        assert_eq!(parsed.base_url, declared.base_url);
        assert_eq!(parsed.env_key, declared.env_key);
        assert_eq!(parsed.default_model, declared.default_model);
    }

    /// A route whose declaration names no variable authenticates with nothing,
    /// so the suggestion offered under "API key" must be a name the credential
    /// reference accepts — one refused there would leave the person re-typing.
    #[test]
    fn a_suggested_credential_name_is_one_a_reference_accepts() {
        for route in ["nvidia", "my-gateway", "openrouter"] {
            let suggested = suggested_env_key(route);
            assert!(
                CredentialRef::new(suggested.clone()).is_ok(),
                "{suggested} is not a usable credential name"
            );
        }
        assert_eq!(suggested_env_key("nvidia"), "NVIDIA_API_KEY");
        assert_eq!(suggested_env_key("my-gateway"), "MY_GATEWAY_API_KEY");
    }
}
