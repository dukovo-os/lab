// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A WS-Management client: just enough to drive Intel AMT.
//!
//! AMT answers SOAP messages posted to `http://<address>:16992/wsman`,
//! behind HTTP digest sign-in (RFC 2617, MD5, `qop=auth`). Each call here
//! is one message: a transfer Get, an enumeration (Enumerate, then Pull),
//! or a method invoked on a class with its arguments as XML. Answers are
//! read with [`elements`] and [`text`], which find elements by their
//! local name whatever their namespace prefix, as AMT's answers are
//! simple and regular; there is no general XML parser here.
//!
//! The lab reaches AMT only on its own network, so it uses plain HTTP;
//! digest sign-in keeps the password off the wire, and the password is
//! never written to a log.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::hash::md5_hex;
use crate::http;

/// AMT's port for WS-Management over plain HTTP.
pub const PORT: u16 = 16992;

/// How long one message may take: AMT is slow, but not this slow.
const MESSAGE_TIME: Duration = Duration::from_secs(20);
/// How long a message keeps trying while AMT refuses to connect. With
/// the machine powered off and AMT left alone for a while, its engine
/// dozes, and the first connection is refused while it wakes (seen on an
/// OptiPlex 7040 with AMT 11.8): a few seconds later it answers again.
const WAKE_TIME: Duration = Duration::from_secs(20);

pub const ADDRESSING: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";
pub const WSMAN: &str = "http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd";
const ANONYMOUS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";
const ENUMERATION: &str = "http://schemas.xmlsoap.org/ws/2004/09/enumeration";
const TRANSFER: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer";

/// A selector: which instance of a class a message is about.
pub type Selector<'a> = (&'a str, &'a str);

/// A client for one AMT.
pub struct Client {
    /// `host:port`.
    address: String,
    user: String,
    password: String,
    /// The last digest challenge, reused until AMT says it is stale.
    challenge: std::sync::Mutex<Option<Challenge>>,
    /// Message ids, unique within the client.
    next_id: AtomicU32,
}

/// What AMT's `WWW-Authenticate: Digest` header asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Challenge {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    /// How many times this nonce has been used.
    count: u32,
}

impl Client {
    /// A client for AMT at `host` (an address, with or without a port).
    pub fn new(host: &str, user: &str, password: &str) -> Client {
        let address = if host.contains(':') { host.to_string() } else { format!("{host}:{PORT}") };
        Client {
            address,
            user: user.to_string(),
            password: password.to_string(),
            challenge: std::sync::Mutex::new(None),
            next_id: AtomicU32::new(1),
        }
    }

    /// Gets one instance of `class` (its resource URI).
    pub fn get(&self, class: &str, selectors: &[Selector<'_>]) -> Result<String, String> {
        self.call(class, &format!("{TRANSFER}/Get"), selectors, "")
    }

    /// Puts `instance` (as [`instance`] writes one) in place of the
    /// instance of `class` that `selectors` pick.
    pub fn put(
        &self,
        class: &str,
        selectors: &[Selector<'_>],
        instance: &str,
    ) -> Result<String, String> {
        self.call(class, &format!("{TRANSFER}/Put"), selectors, instance)
    }

    /// Lists every instance of `class`: an Enumerate, then Pulls until AMT
    /// says the list has ended. Gives each Pull's items, joined.
    pub fn enumerate(&self, class: &str) -> Result<String, String> {
        let started = self.call(
            class,
            &format!("{ENUMERATION}/Enumerate"),
            &[],
            &format!("<Enumerate xmlns=\"{ENUMERATION}\"/>"),
        )?;
        let mut context = text(&started, "EnumerationContext")
            .ok_or("AMT started no enumeration: its answer has no EnumerationContext")?;
        let mut items = String::new();
        // AMT gives everything in one Pull with these limits; the bound
        // only stops a firmware that never ends the list.
        for _ in 0..16 {
            let pulled = self.call(
                class,
                &format!("{ENUMERATION}/Pull"),
                &[],
                &format!(
                    "<Pull xmlns=\"{ENUMERATION}\"><EnumerationContext>{}</EnumerationContext>\
                     <MaxElements>999</MaxElements><MaxCharacters>99999</MaxCharacters></Pull>",
                    escape(&context)
                ),
            )?;
            for found in elements(&pulled, "Items") {
                items.push_str(found);
            }
            if elements(&pulled, "EndOfSequence").is_empty() {
                context = text(&pulled, "EnumerationContext")
                    .ok_or("AMT's Pull neither ended the list nor gave a context to go on")?;
            } else {
                return Ok(items);
            }
        }
        Err("AMT's list did not end".into())
    }

    /// Invokes `method` of `class` on the instance `selectors` pick, with
    /// `arguments` (the method's `_INPUT` element). Gives the answer when
    /// its `ReturnValue` is 0, else says which it was.
    pub fn invoke(
        &self,
        class: &str,
        method: &str,
        selectors: &[Selector<'_>],
        arguments: &str,
    ) -> Result<String, String> {
        let answer = self.call(class, &format!("{class}/{method}"), selectors, arguments)?;
        match text(&answer, "ReturnValue").as_deref() {
            Some("0") => Ok(answer),
            Some(value) => Err(format!("{method} returned {value}")),
            None => Err(format!("{method}'s answer has no ReturnValue")),
        }
    }

    /// Sends one message and gives the answer's text: a SOAP fault, or an
    /// HTTP status other than 200, is an error.
    pub fn call(
        &self,
        class: &str,
        action: &str,
        selectors: &[Selector<'_>],
        body: &str,
    ) -> Result<String, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = envelope(class, action, selectors, body, id);
        let answer = self.post(message.as_bytes())?;
        let text_of = String::from_utf8_lossy(&answer.body).into_owned();
        if answer.status != 200 {
            let why = text(&text_of, "Text")
                .or_else(|| text(&text_of, "Reason"))
                .unwrap_or_else(|| format!("HTTP status {}", answer.status));
            return Err(format!("AMT refused {}: {why}", short_action(action)));
        }
        if !elements(&text_of, "Fault").is_empty() {
            let why = text(&text_of, "Text").unwrap_or_else(|| "a SOAP fault".into());
            return Err(format!("AMT refused {}: {why}", short_action(action)));
        }
        Ok(text_of)
    }

    /// Posts `message` to /wsman, signing in with HTTP digest: with the
    /// challenge from before when there is one, else, or when AMT turns
    /// that down (its nonces go stale), with the one AMT gives back.
    fn post(&self, message: &[u8]) -> Result<http::Answer, String> {
        for _ in 0..2 {
            let mut headers = vec![("Content-Type", "application/soap+xml;charset=UTF-8".into())];
            if let Some(authorization) = self.authorization() {
                headers.push(("Authorization", authorization));
            }
            let answer = http::send(
                &self.address,
                "POST",
                "/wsman",
                &headers,
                message,
                MESSAGE_TIME,
                WAKE_TIME,
            )
            .map_err(|err| format!("AMT at {}: {err}", self.address))?;
            if answer.status != 401 {
                return Ok(answer);
            }
            let Some(challenge) = answer.header("www-authenticate").and_then(parse_challenge)
            else {
                return Err(format!(
                    "{} answered HTTP 401 without a digest challenge: is it AMT?",
                    self.address
                ));
            };
            if let Ok(mut old) = self.challenge.lock() {
                *old = Some(challenge);
            }
        }
        Err(format!(
            "AMT at {} did not take the user and password (HTTP 401): check the machine \
             file's amt.user and the password in its amt.password_file",
            self.address
        ))
    }

    /// The Authorization header for the next message, from the challenge
    /// AMT gave last; none before the first.
    fn authorization(&self) -> Option<String> {
        let mut guard = self.challenge.lock().ok()?;
        let challenge = guard.as_mut()?;
        challenge.count += 1;
        let cnonce = crate::netboot::random_hex(8).unwrap_or_else(|_| "0a4f113b".into());
        Some(digest_header(&self.user, &self.password, challenge, "POST", "/wsman", &cnonce))
    }
}

/// The `Authorization: Digest` header's value for a request (RFC 2617,
/// `qop=auth`).
fn digest_header(
    user: &str,
    password: &str,
    challenge: &Challenge,
    method: &str,
    uri: &str,
    cnonce: &str,
) -> String {
    let count = format!("{:08x}", challenge.count);
    let ha1 = md5_hex(format!("{user}:{}:{password}", challenge.realm).as_bytes());
    let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
    let response =
        md5_hex(format!("{ha1}:{}:{count}:{cnonce}:auth:{ha2}", challenge.nonce).as_bytes());
    let mut header = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", \
         qop=auth, nc={count}, cnonce=\"{cnonce}\", response=\"{response}\"",
        challenge.realm, challenge.nonce
    );
    if let Some(opaque) = &challenge.opaque {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    header
}

/// Reads a `WWW-Authenticate: Digest ...` header's fields.
fn parse_challenge(header: &str) -> Option<Challenge> {
    let rest =
        header.trim().strip_prefix("Digest").or_else(|| header.trim().strip_prefix("digest"))?;
    let mut fields = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let name: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if name.is_empty() {
            break;
        }
        let value: String = if chars.peek() == Some(&'"') {
            chars.next();
            let mut value = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
            value
        } else {
            chars.by_ref().take_while(|c| *c != ',').collect::<String>().trim().to_string()
        };
        fields.push((name.trim().to_ascii_lowercase(), value));
    }
    let field = |name: &str| fields.iter().find(|(key, _)| key == name).map(|(_, v)| v.clone());
    Some(Challenge {
        realm: field("realm")?,
        nonce: field("nonce")?,
        opaque: field("opaque"),
        count: 0,
    })
}

/// A whole SOAP message for AMT.
fn envelope(class: &str, action: &str, selectors: &[Selector<'_>], body: &str, id: u32) -> String {
    let mut selector_set = String::new();
    if !selectors.is_empty() {
        selector_set.push_str("<w:SelectorSet>");
        for (name, value) in selectors {
            selector_set
                .push_str(&format!("<w:Selector Name=\"{name}\">{}</w:Selector>", escape(value)));
        }
        selector_set.push_str("</w:SelectorSet>");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <Envelope xmlns=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"{ADDRESSING}\" \
         xmlns:w=\"{WSMAN}\"><Header><a:Action>{action}</a:Action><a:To>/wsman</a:To>\
         <w:ResourceURI>{class}</w:ResourceURI>\
         <a:MessageID>uuid:00000000-8086-8086-8086-{id:012}</a:MessageID>\
         <a:ReplyTo><a:Address>{ANONYMOUS}</a:Address></a:ReplyTo>\
         <w:OperationTimeout>PT60S</w:OperationTimeout>{selector_set}</Header>\
         <Body>{body}</Body></Envelope>"
    )
}

/// An endpoint reference to the instance of `class` that `selectors`
/// pick, as a method's argument: the inside of an element such as
/// `<h:ManagedElement>`.
pub fn reference(class: &str, selectors: &[Selector<'_>]) -> String {
    let mut out = format!(
        "<Address xmlns=\"{ADDRESSING}\">{ANONYMOUS}</Address>\
         <ReferenceParameters xmlns=\"{ADDRESSING}\">\
         <ResourceURI xmlns=\"{WSMAN}\">{class}</ResourceURI><SelectorSet xmlns=\"{WSMAN}\">"
    );
    for (name, value) in selectors {
        out.push_str(&format!("<Selector Name=\"{name}\">{}</Selector>", escape(value)));
    }
    out.push_str("</SelectorSet></ReferenceParameters>");
    out
}

/// The last part of an action's URI, for messages.
fn short_action(action: &str) -> &str {
    action.rsplit('/').next().unwrap_or(action)
}

/// `text` with XML's special characters escaped.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `text` with XML's five named entities read back.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The insides of every element whose local name is `name`, in order;
/// an empty element (`<a:X/>`) gives an empty inside. Elements of the
/// same name nested in each other are not supported, and AMT sends none.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(open) = xml[at..].find('<').map(|offset| at + offset) {
        let tag_end = match xml[open..].find('>') {
            Some(offset) => open + offset,
            None => break,
        };
        let tag = &xml[open + 1..tag_end];
        at = tag_end + 1;
        if tag.starts_with(['/', '?', '!']) {
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag_name = tag.trim_end_matches('/').split_whitespace().next().unwrap_or_default();
        let local = tag_name.rsplit(':').next().unwrap_or(tag_name);
        if local != name {
            continue;
        }
        if self_closing {
            found.push("");
            continue;
        }
        let close = format!("</{tag_name}>");
        if let Some(offset) = xml[at..].find(&close) {
            found.push(&xml[at..at + offset]);
            at += offset + close.len();
        }
    }
    found
}

/// The properties of one instance, as a Get gives it (the inside of the
/// class's element): each child element's local name and text, in order.
/// Only flat instances, whose properties hold text alone, read back
/// whole, and the ones the lab puts back are flat.
pub fn properties(instance: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(open) = instance[at..].find('<').map(|offset| at + offset) {
        let Some(tag_end) = instance[open..].find('>').map(|offset| open + offset) else {
            break;
        };
        let tag = &instance[open + 1..tag_end];
        at = tag_end + 1;
        if tag.starts_with(['/', '?', '!']) {
            continue;
        }
        let tag_name = tag.trim_end_matches('/').split_whitespace().next().unwrap_or_default();
        let local = tag_name.rsplit(':').next().unwrap_or(tag_name).to_string();
        if tag.ends_with('/') {
            found.push((local, String::new()));
            continue;
        }
        let close = format!("</{tag_name}>");
        let Some(offset) = instance[at..].find(&close) else { break };
        found.push((local, unescape(instance[at..at + offset].trim())));
        at += offset + close.len();
    }
    found
}

/// An instance of `class` (its resource URI) holding `properties`, in
/// order, for a Put.
pub fn instance(class: &str, properties: &[(String, String)]) -> String {
    let name = class.rsplit('/').next().unwrap_or(class);
    let mut out = format!("<r:{name} xmlns:r=\"{class}\">");
    for (property, value) in properties {
        out.push_str(&format!("<r:{property}>{}</r:{property}>", escape(value)));
    }
    out.push_str(&format!("</r:{name}>"));
    out
}

/// The text of the first element whose local name is `name`, trimmed
/// and unescaped.
pub fn text(xml: &str, name: &str) -> Option<String> {
    elements(xml, name).first().map(|inside| unescape(inside.trim()))
}

/// The texts of every element whose local name is `name`.
pub fn texts(xml: &str, name: &str) -> Vec<String> {
    elements(xml, name).iter().map(|inside| unescape(inside.trim())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_in_as_rfc_2617_does() {
        // RFC 2617's own example, section 3.5.
        let challenge = Challenge {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".into()),
            count: 1,
        };
        let header = digest_header(
            "Mufasa",
            "Circle Of Life",
            &challenge,
            "GET",
            "/dir/index.html",
            "0a4f113b",
        );
        assert!(header.contains("response=\"6629fae49393a05397450978507c4ef1\""), "{header}");
        assert!(header.contains("nc=00000001"), "{header}");
        assert!(header.ends_with("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""), "{header}");
    }

    #[test]
    fn reads_challenges() {
        let challenge = parse_challenge(
            "Digest realm=\"Digest:A3829B3827DE4D33D4449B366831FD01\", \
             nonce=\"3D3m0EQAAAAAAAAAyIw1cWQ0E5eGlWx2\",stale=\"false\",qop=\"auth\"",
        )
        .unwrap();
        assert_eq!(challenge.realm, "Digest:A3829B3827DE4D33D4449B366831FD01");
        assert_eq!(challenge.nonce, "3D3m0EQAAAAAAAAAyIw1cWQ0E5eGlWx2");
        assert_eq!(challenge.opaque, None);
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn finds_elements_whatever_their_prefix() {
        let xml = "<a:Envelope><a:Body><g:PullResponse><g:Items>\
                   <h:CIM_SoftwareIdentity><h:InstanceID>AMT</h:InstanceID>\
                   <h:VersionString>11.8.50</h:VersionString></h:CIM_SoftwareIdentity>\
                   <h:CIM_SoftwareIdentity><h:InstanceID>Flash</h:InstanceID>\
                   <h:VersionString>11.8.50 &amp; up</h:VersionString></h:CIM_SoftwareIdentity>\
                   </g:Items><g:EndOfSequence/></g:PullResponse></a:Body></a:Envelope>";
        let items = elements(xml, "CIM_SoftwareIdentity");
        assert_eq!(items.len(), 2);
        assert_eq!(text(items[1], "VersionString").as_deref(), Some("11.8.50 & up"));
        assert_eq!(elements(xml, "EndOfSequence"), [""]);
        assert_eq!(texts(xml, "InstanceID"), ["AMT", "Flash"]);
        assert_eq!(text(xml, "Missing"), None);
    }

    #[test]
    fn instances_read_back_and_are_put_whole() {
        let answer = "<a:Body><g:IPS_KVMRedirectionSettingData>\
                      <g:ElementName>Intel(r) KVM Redirection Settings</g:ElementName>\
                      <g:EnabledByMEBx>true</g:EnabledByMEBx><g:DefaultScreen/>\
                      <g:SessionTimeout> 3 </g:SessionTimeout><g:Note>a &amp; b</g:Note>\
                      </g:IPS_KVMRedirectionSettingData></a:Body>";
        let inside = elements(answer, "IPS_KVMRedirectionSettingData")[0];
        let properties = properties(inside);
        let names: Vec<&str> = properties.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["ElementName", "EnabledByMEBx", "DefaultScreen", "SessionTimeout", "Note"]
        );
        assert_eq!(properties[3].1, "3");
        assert_eq!(properties[4].1, "a & b");
        let put = instance("http://x/IPS_KVMRedirectionSettingData", &properties);
        assert!(put.starts_with(
            "<r:IPS_KVMRedirectionSettingData xmlns:r=\"http://x/IPS_KVMRedirectionSettingData\">\
             <r:ElementName>Intel(r) KVM Redirection Settings</r:ElementName>"
        ));
        assert!(
            put.contains(
                "<r:DefaultScreen></r:DefaultScreen><r:SessionTimeout>3</r:SessionTimeout>"
            )
        );
        assert!(put.ends_with("<r:Note>a &amp; b</r:Note></r:IPS_KVMRedirectionSettingData>"));
    }

    #[test]
    fn messages_name_their_class_action_and_instance() {
        let message = envelope(
            "http://x/CIM_BootService",
            "http://x/CIM_BootService/SetBootConfigRole",
            &[("Name", "Intel(r) AMT Boot Service")],
            "<h:SetBootConfigRole_INPUT/>",
            7,
        );
        assert!(message.contains("<w:ResourceURI>http://x/CIM_BootService</w:ResourceURI>"));
        assert!(
            message.contains("<w:Selector Name=\"Name\">Intel(r) AMT Boot Service</w:Selector>")
        );
        assert!(message.contains("<Body><h:SetBootConfigRole_INPUT/></Body>"));
        assert!(message.contains("8086-000000000007</a:MessageID>"));
    }
}
