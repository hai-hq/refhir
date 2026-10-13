//! SMART authorize UI: sign-in, patient selection, and consent.

use std::collections::HashMap;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use refhir_fhir::run;

use super::oauth::{
    issuer, now, oauth_error, parse_authorize_form, query_encode, redirect_with,
    scopes_need_patient, storage_unavailable,
};
use super::oidc::{
    constant_time_eq, cookie_value, FhirUserLookup, Identity, Oidc, OidcConfig, SignInError,
};
use super::store::{code_ttl, launch_ttl, normalize_patient_id, AuthCode, PendingSession};
use crate::http::AppState;

const OIDC_COOKIE: &str = "refhir_oidc";

/// How users prove who they are on the authorize page.
enum Login<'a> {
    Oidc(&'a Oidc),
    /// Any username is accepted. Testing only.
    Demo,
    Disabled,
}

fn login_mode(app: &AppState) -> Login<'_> {
    if let Some(oidc) = app.oidc.as_deref() {
        Login::Oidc(oidc)
    } else if app.config.smart_demo_login {
        Login::Demo
    } else {
        Login::Disabled
    }
}

fn login_disabled() -> Response {
    oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        "no login method is configured: set [smart.oidc], or enable demo login for testing",
    )
}

pub fn interact_get(app: &AppState, headers: &HeaderMap, session_id: &str) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let Some(session) = app.smart.pending(session_id) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "authorization session expired or unknown",
        );
    };
    match login_mode(app) {
        Login::Disabled => return login_disabled(),
        Login::Oidc(oidc) if session.identity.is_none() => {
            return begin_oidc(app, headers, oidc, session_id, session);
        }
        _ => {}
    }
    html_response(render_page(app, headers, session_id, &session, ""))
}

fn callback_uri(app: &AppState, headers: &HeaderMap, oidc: &Oidc) -> String {
    oidc.config()
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("{}/oauth/oidc/callback", issuer(app, headers)))
}

fn browser_cookie_name(session_id: &str) -> String {
    format!("{OIDC_COOKIE}_{session_id}")
}

/// The binder cookie this browser holds for `session_id`, if any.
fn browser_binder(headers: &HeaderMap, session_id: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookies| cookie_value(cookies, &browser_cookie_name(session_id)))
}

fn clear_browser_cookie(session_id: &str) -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        browser_cookie_name(session_id)
    )
}

fn with_cookie(mut response: Response, cookie: String) -> Response {
    if let Ok(value) = header::HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Send the browser to the identity provider, remembering the flow on the pending session.
fn begin_oidc(
    app: &AppState,
    headers: &HeaderMap,
    oidc: &Oidc,
    session_id: &str,
    mut session: PendingSession,
) -> Response {
    let redirect_uri = callback_uri(app, headers, oidc);
    let (flow, url) = match oidc.begin(session_id, &redirect_uri) {
        Ok(started) => started,
        Err(err) => {
            tracing::error!("openid connect login could not start: {err}");
            return oauth_error(
                StatusCode::BAD_GATEWAY,
                "temporarily_unavailable",
                "the identity provider is unreachable",
            );
        }
    };
    let secure = if redirect_uri.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    // One cookie per session, so parallel logins in one browser do not overwrite each other.
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        browser_cookie_name(session_id),
        flow.binder,
        launch_ttl()
    );
    session.oidc = Some(flow);
    if app
        .smart
        .put_pending(session_id.to_string(), session)
        .is_err()
    {
        return storage_unavailable();
    }
    with_cookie(
        (StatusCode::FOUND, [(header::LOCATION, url)]).into_response(),
        cookie,
    )
}

/// Where the identity provider sends the browser back to after sign-in.
pub fn oidc_callback(app: &AppState, headers: &HeaderMap, raw_query: &str) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let Some(oidc) = app.oidc.as_deref() else {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "openid connect login is not enabled",
        );
    };
    let params = parse_authorize_form(raw_query.as_bytes());
    let state = params.get("state").cloned().unwrap_or_default();
    let bad_state = || {
        oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "sign-in session expired or unknown",
        )
    };
    let Some((session_id, _)) = state.split_once('.') else {
        return bad_state();
    };
    // Check state and browser before consuming, so a forged callback cannot end the login.
    let flow_matches = |session: &PendingSession| {
        session
            .oidc
            .as_ref()
            .is_some_and(|flow| constant_time_eq(&flow.state, &state))
    };
    let Some(peeked) = app.smart.pending(session_id).filter(|s| flow_matches(s)) else {
        return bad_state();
    };
    let same_browser = browser_binder(headers, session_id).is_some_and(|binder| {
        peeked
            .oidc
            .as_ref()
            .is_some_and(|flow| constant_time_eq(&binder, &flow.binder))
    });
    if !same_browser {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "sign-in must be completed in the browser that started it",
        );
    }
    // Single use: a state value cannot be replayed.
    let Some(mut session) = app
        .smart
        .take_pending(session_id)
        .filter(|s| flow_matches(s))
    else {
        return bad_state();
    };
    let flow = session.oidc.take().expect("checked above");
    let clear = clear_browser_cookie(session_id);
    let (client_redirect, client_state) = (session.redirect_uri.clone(), session.state.clone());
    let refuse = |code: &str, description: &str| {
        with_cookie(
            redirect_with(
                &client_redirect,
                &[
                    ("error", code),
                    ("error_description", description),
                    ("state", &client_state),
                ],
            ),
            clear.clone(),
        )
    };
    if params.contains_key("error") {
        return refuse("access_denied", "sign-in was not completed");
    }
    let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
        return refuse("invalid_request", "the identity provider returned no code");
    };
    match oidc.complete(code, &flow, &callback_uri(app, headers, oidc)) {
        Ok(mut identity) => {
            if let Err(description) =
                bind_identity(app, headers, oidc.config(), &mut identity, &mut session)
            {
                tracing::warn!(
                    "openid connect sign-in refused for {}: {description}",
                    identity.sub
                );
                return refuse("access_denied", description);
            }
            session.identity = Some(identity);
            // The cookie stays set: the consent POST must come from this same browser.
            session.browser = Some(flow.binder);
            if app
                .smart
                .put_pending(session_id.to_string(), session)
                .is_err()
            {
                return storage_unavailable();
            }
            redirect_to_interact(session_id)
        }
        Err(SignInError::NotPermitted(reason)) => {
            tracing::warn!("openid connect sign-in not permitted: {reason}");
            refuse(
                "access_denied",
                "this account is not permitted to sign in to this server",
            )
        }
        Err(SignInError::Failed(err)) => {
            tracing::warn!("openid connect sign-in failed: {err}");
            refuse("access_denied", "sign-in failed")
        }
    }
}

/// Work out which FHIR resource the signed-in person is, and what that means for the session.
///
/// The ID token's own claim wins when it is usable. Otherwise, when the launch has not named a
/// person either, a configured lookup searches for one. A `Patient` binding can then fix the
/// launch patient, so the picker never offers anyone else. `Err` is shown to the client.
fn bind_identity(
    app: &AppState,
    headers: &HeaderMap,
    cfg: &OidcConfig,
    identity: &mut Identity,
    session: &mut PendingSession,
) -> Result<(), &'static str> {
    identity.fhir_user = identity
        .fhir_user
        .take()
        .filter(|reference| reference.contains("://") || valid_fhir_reference(reference));
    // Used here and nowhere after: it is not kept on the stored session.
    let lookup_key = identity.lookup_key.take();
    let wants_user = session.scopes.iter().any(|scope| scope == "fhirUser");
    let wants_patient = cfg.patient_from_fhir_user && scopes_need_patient(&session.scopes);
    if identity.fhir_user.is_none() && session.fhir_user.is_none() && (wants_user || wants_patient)
    {
        if let (Some(lookup), Some(key)) = (&cfg.fhir_user_lookup, &lookup_key) {
            identity.fhir_user = lookup_fhir_user(app, lookup, key);
        }
    }
    if !wants_patient {
        return Ok(());
    }
    let iss = issuer(app, headers);
    let Some(bound) = identity
        .fhir_user
        .as_deref()
        .and_then(|reference| bound_patient(&iss, reference))
    else {
        return Ok(());
    };
    if session
        .patient
        .as_deref()
        .is_some_and(|launched| normalize_patient_id(launched) != bound)
    {
        return Err("the signed-in user is not the patient this launch is for");
    }
    if !patient_exists(app, &bound) {
        return Err("the patient bound to the signed-in user does not exist");
    }
    session.patient = Some(bound);
    session.needs_patient_pick = false;
    Ok(())
}

/// The id of `reference` when it names a `Patient` on this server (relative, or under the issuer).
pub(crate) fn bound_patient(iss: &str, reference: &str) -> Option<String> {
    let relative = match reference.contains("://") {
        true => reference
            .strip_prefix(iss.trim_end_matches('/'))?
            .trim_start_matches('/'),
        false => reference,
    };
    let (kind, id) = relative.split_once('/')?;
    (kind == "Patient" && valid_fhir_reference(relative)).then(|| id.to_string())
}

/// The search value for `lookup`, or `None` when `key` cannot be put in a search safely.
///
/// `,` `$` `\` are search syntax (OR, composite, escape) and `|` splits system from code; the
/// engine does not unescape, so a key holding them is refused rather than escaped, which keeps
/// a person from matching more than their own resource. `|` is fine after a system.
pub(crate) fn lookup_query(lookup: &FhirUserLookup, key: &str) -> Option<String> {
    let reserved = |c: char| matches!(c, ',' | '$' | '\\');
    if key.is_empty() || key.contains(reserved) {
        return None;
    }
    match &lookup.system {
        Some(system) if !system.contains(reserved) && !system.contains('|') => {
            Some(format!("{system}|{key}"))
        }
        Some(_) => None,
        None => (!key.contains('|')).then(|| key.to_string()),
    }
}

/// `Type/id` of the one resource the lookup finds for `key`. No hit, several hits, or a failed
/// search all bind nobody.
fn lookup_fhir_user(app: &AppState, lookup: &FhirUserLookup, key: &str) -> Option<String> {
    let Some(value) = lookup_query(lookup, key) else {
        tracing::warn!(
            "fhir_user_lookup: the {} claim value cannot be searched",
            lookup.claim
        );
        return None;
    };
    let catalog = app.catalog.read().expect("catalog");
    let path = format!("{}/search", lookup.resource_type);
    let args = json!({ lookup.search_param.as_str(): value, "_count": "2" });
    let result = match run(&app.db, &catalog, &path, &args) {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("fhir_user_lookup: {path} failed: {err}");
            return None;
        }
    };
    let ids: Vec<&str> = result
        .value
        .get("entry")
        .and_then(|entries| entries.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.pointer("/resource/id").and_then(|id| id.as_str()))
        .collect();
    match ids.as_slice() {
        [id] => Some(format!("{}/{id}", lookup.resource_type)),
        [] => {
            tracing::info!(
                "fhir_user_lookup: no {} matches the signed-in user",
                lookup.resource_type
            );
            None
        }
        _ => {
            tracing::warn!(
                "fhir_user_lookup: more than one {} matches the signed-in user; binding nobody",
                lookup.resource_type
            );
            None
        }
    }
}

pub fn interact_post(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    if matches!(login_mode(app), Login::Disabled) {
        return login_disabled();
    }
    let Some(session_id) = form.get("session").filter(|v| !v.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "session is required",
        );
    };
    // A provider-verified identity may only be used by the browser that signed in.
    if let Some(expected) = app.smart.pending(session_id).and_then(|s| s.browser) {
        let same_browser = browser_binder(headers, session_id)
            .is_some_and(|binder| constant_time_eq(&binder, &expected));
        if !same_browser {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "consent must be given in the browser that signed in",
            );
        }
    }
    let Some(session) = app.smart.take_pending(session_id) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "authorization session expired or unknown",
        );
    };
    if form.get("action").map(|v| v.as_str()) == Some("deny") {
        return with_cookie(
            redirect_with(
                &session.redirect_uri,
                &[
                    ("error", "access_denied"),
                    ("error_description", "user denied the request"),
                    ("state", &session.state),
                ],
            ),
            clear_browser_cookie(session_id),
        );
    }
    // With OpenID Connect the identity is the one the provider verified, never form input.
    let identity = match login_mode(app) {
        Login::Oidc(_) => match session.identity.clone() {
            Some(identity) => Some(identity),
            None => {
                if app.smart.put_pending(session_id.clone(), session).is_err() {
                    return storage_unavailable();
                }
                return redirect_to_interact(session_id);
            }
        },
        _ => None,
    };
    let username = match &identity {
        Some(identity) => identity.sub.clone(),
        None => {
            let typed = form.get("username").map(|v| v.trim()).unwrap_or("");
            if !valid_username(typed) {
                return reprompt(
                    app,
                    headers,
                    session_id,
                    &session,
                    "username is required and may contain only letters, digits, '-' and '.'",
                );
            }
            typed.to_string()
        }
    };
    let mut patient = session.patient.clone();
    if session.needs_patient_pick {
        let picked = form
            .get("patient_id")
            .map(|v| normalize_patient_id(v))
            .filter(|v| !v.is_empty());
        if picked.is_none() {
            return reprompt(app, headers, session_id, &session, "select a patient");
        }
        let picked = picked.unwrap();
        if !patient_exists(app, &picked) {
            return reprompt(app, headers, session_id, &session, "patient does not exist");
        }
        patient = Some(picked);
    }
    let mut encounter = session.encounter.clone();
    if session.needs_encounter_pick {
        encounter = form
            .get("encounter_id")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        if encounter.is_none() {
            return reprompt(
                app,
                headers,
                session_id,
                &session,
                "encounter id is required",
            );
        }
    }
    if scopes_need_patient(&session.scopes) && patient.is_none() {
        return redirect_with(
            &session.redirect_uri,
            &[
                ("error", "invalid_request"),
                (
                    "error_description",
                    "patient context is required for the requested scopes",
                ),
                ("state", &session.state),
            ],
        );
    }
    let iss = issuer(app, headers);
    let mut fhir_user = session.fhir_user.clone();
    if let Some(identity) = &identity {
        // The provider is authoritative over anything the launch supplied.
        let from_provider = identity
            .fhir_user
            .as_deref()
            .filter(|reference| reference.contains("://") || valid_fhir_reference(reference))
            .map(|reference| absolute_fhir_user(&iss, reference));
        if from_provider.is_some() {
            fhir_user = from_provider;
        }
    }
    if session.scopes.iter().any(|scope| scope == "fhirUser") && fhir_user.is_none() {
        if identity.is_some() {
            return redirect_with(
                &session.redirect_uri,
                &[
                    ("error", "invalid_request"),
                    (
                        "error_description",
                        "the identity provider did not supply a valid fhirUser claim, and no FHIR resource is bound to this user",
                    ),
                    ("state", &session.state),
                ],
            );
        }
        let reference = match app
            .config
            .smart_fhir_user
            .as_deref()
            .filter(|v| !v.is_empty())
        {
            Some(configured) => configured.to_string(),
            None => format!("Practitioner/{username}"),
        };
        fhir_user = Some(absolute_fhir_user(&iss, &reference));
    }
    let code = format!("code-{}", uuid::Uuid::new_v4());
    let stored = app.smart.put_code(
        code.clone(),
        AuthCode {
            client_id: session.client_id,
            redirect_uri: session.redirect_uri.clone(),
            scopes: session.scopes,
            challenge: session.challenge,
            patient,
            encounter,
            fhir_user,
            sub: username,
            nonce: session.nonce,
            exp: now() + code_ttl(),
        },
    );
    if stored.is_err() {
        return storage_unavailable();
    }
    with_cookie(
        redirect_with(
            &session.redirect_uri,
            &[("code", &code), ("state", &session.state)],
        ),
        clear_browser_cookie(session_id),
    )
}

/// Show the form again with `error`. The session goes back under the same id, so the
/// browser's binder cookie still matches. It is rendered from the value in hand, not re-read.
fn reprompt(
    app: &AppState,
    headers: &HeaderMap,
    session_id: &str,
    session: &PendingSession,
    error: &str,
) -> Response {
    if app
        .smart
        .put_pending(session_id.to_string(), session.clone())
        .is_err()
    {
        return storage_unavailable();
    }
    html_response(render_page(app, headers, session_id, session, error))
}

fn render_page(
    app: &AppState,
    headers: &HeaderMap,
    session_id: &str,
    session: &PendingSession,
    error: &str,
) -> String {
    let patients = if session.needs_patient_pick {
        list_patients(app)
    } else {
        Vec::new()
    };
    let patient_options = patients
        .into_iter()
        .map(|(id, label)| {
            format!(
                "<option value=\"{}\">{}</option>",
                html_escape(&id),
                html_escape(&label)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let patient_block = if session.needs_patient_pick {
        format!(
            "<label>Patient <select name=\"patient_id\" required><option value=\"\">Select…</option>{patient_options}</select></label>"
        )
    } else {
        String::new()
    };
    let encounter_block = if session.needs_encounter_pick {
        "<label>Encounter id <input name=\"encounter_id\" required></label>".to_string()
    } else {
        String::new()
    };
    let login_block = match &session.identity {
        Some(identity) => format!(
            "<p>Signed in as <strong>{}</strong></p>",
            html_escape(identity.display.as_deref().unwrap_or(&identity.sub))
        ),
        None => "<p class=\"err\">Demo login: any username is accepted and nothing is verified. Testing only.</p>\
<label>Username <input name=\"username\" required autocomplete=\"username\"></label>"
            .to_string(),
    };
    let err = if error.is_empty() {
        String::new()
    } else {
        format!("<p class=\"err\">{}</p>", html_escape(error))
    };
    let context = match (&session.patient, &session.encounter) {
        (Some(p), Some(e)) => format!("Launch context: patient {}, encounter {}", p, e),
        (Some(p), None) => format!("Launch context: patient {}", p),
        _ => String::new(),
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Authorize</title>
<style>body{{font-family:system-ui;max-width:32rem;margin:2rem auto;}}
label{{display:block;margin:.75rem 0}} .err{{color:#b00020}}</style></head>
<body>
<h1>Authorize application</h1>
<p>Client: <strong>{client}</strong></p>
<p>Scopes: <code>{scopes}</code></p>
<p>{context}</p>
{err}
<form method="post" action="/oauth/interact">
<input type="hidden" name="session" value="{session_id}">
{login_block}
{patient_block}
{encounter_block}
<button type="submit" name="action" value="approve">Approve</button>
<button type="submit" name="action" value="deny">Deny</button>
</form>
<p><small>Issuer: {iss}</small></p>
</body></html>"#,
        client = html_escape(&session.client_id),
        scopes = html_escape(&session.scopes.join(" ")),
        context = html_escape(&context),
        err = err,
        login_block = login_block,
        session_id = html_escape(session_id),
        patient_block = patient_block,
        encounter_block = encounter_block,
        iss = html_escape(&issuer(app, headers)),
    )
}

fn html_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn html_escape(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            _ => ch.to_string(),
        })
        .collect()
}

pub(crate) fn patient_exists(app: &AppState, id: &str) -> bool {
    let catalog = app.catalog.read().expect("catalog");
    let id = normalize_patient_id(id);
    run(&app.db, &catalog, "Patient/read", &json!({"id": id})).is_ok()
}

fn list_patients(app: &AppState) -> Vec<(String, String)> {
    let catalog = app.catalog.read().expect("catalog");
    let Ok(result) = run(
        &app.db,
        &catalog,
        "Patient/search",
        &json!({"_count": "100"}),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let entries = result
        .value
        .get("entry")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        let resource = entry.get("resource").unwrap_or(&entry);
        let id = resource.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let name = resource
            .pointer("/name/0/family")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        out.push((id.to_string(), format!("{} ({})", name, id)));
    }
    out
}

/// Resolve a FHIR reference to an absolute URL under the issuer. Absolute URLs pass through.
pub(crate) fn absolute_fhir_user(iss: &str, reference: &str) -> String {
    if reference.contains("://") {
        return reference.to_string();
    }
    format!(
        "{}/{}",
        iss.trim_end_matches('/'),
        reference.trim_start_matches('/')
    )
}

/// Usernames become the token `sub` and part of the `fhirUser` reference, so they are limited
/// to the FHIR id alphabet (starting with a letter or digit) and 64 characters.
pub(crate) fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// A relative `ResourceType/id` reference with no path tricks or URL metacharacters.
pub(crate) fn valid_fhir_reference(reference: &str) -> bool {
    let Some((kind, id)) = reference.split_once('/') else {
        return false;
    };
    !kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphabetic()) && valid_username(id)
}

pub(crate) fn redirect_to_interact(session_id: &str) -> Response {
    let location = format!("/oauth/interact?session={}", query_encode(session_id));
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(axum::body::Body::empty())
        .expect("redirect")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_limited_to_fhir_id_characters() {
        assert!(valid_username("dr.smith-01"));
        for bad in [
            "",
            "x/../Patient/123",
            "a?b",
            "a#b",
            "-lead",
            "a b",
            &"a".repeat(65),
        ] {
            assert!(!valid_username(bad), "{bad}");
        }
    }

    #[test]
    fn a_bound_patient_is_a_patient_on_this_server() {
        let iss = "http://h/fhir";
        assert_eq!(bound_patient(iss, "Patient/p1").as_deref(), Some("p1"));
        assert_eq!(
            bound_patient(iss, "http://h/fhir/Patient/p1").as_deref(),
            Some("p1")
        );
        for other in [
            "Practitioner/p1",
            "http://elsewhere/fhir/Patient/p1",
            "Patient/../Patient/p1",
            "Patient/",
        ] {
            assert_eq!(bound_patient(iss, other), None, "{other}");
        }
    }

    #[test]
    fn lookup_values_cannot_widen_the_search() {
        let mut lookup = FhirUserLookup::new("Practitioner");
        assert_eq!(lookup_query(&lookup, "u-1").as_deref(), Some("u-1"));
        assert_eq!(lookup_query(&lookup, "a|b"), None);
        lookup.system = Some("urn:idp".into());
        assert_eq!(lookup_query(&lookup, "u-1").as_deref(), Some("urn:idp|u-1"));
        assert_eq!(
            lookup_query(&lookup, "auth0|u1").as_deref(),
            Some("urn:idp|auth0|u1")
        );
        for bad in ["", "a,b", "a$b", "a\\b"] {
            assert_eq!(lookup_query(&lookup, bad), None, "{bad}");
        }
    }

    #[test]
    fn fhir_references_are_relative_resource_ids() {
        assert!(valid_fhir_reference("Practitioner/abc-1"));
        assert!(!valid_fhir_reference("Practitioner/../Patient/1"));
        assert!(!valid_fhir_reference("abc"));
        assert_eq!(
            absolute_fhir_user("http://h/fhir/", "Patient/1"),
            "http://h/fhir/Patient/1"
        );
    }
}
