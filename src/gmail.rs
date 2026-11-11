//! Gmail REST API, OAuth (installed-app loopback + PKCE), and MIME building.
//! Everything here is blocking; the UI runs it on the background executor.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
// gmail.modify covers read, label changes (archive / mark read) and send.
const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------- config & accounts on disk ----------

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedAccount {
    pub email: String,
    pub refresh_token: String,
}

fn dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| ".".into())
        .join("mymail")
}

/// Env vars win over `~/.config/mymail/client.json` (macOS: `~/Library/Application Support/mymail`).
pub fn load_client() -> Result<OAuthClient> {
    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("MYMAIL_CLIENT_ID"),
        std::env::var("MYMAIL_CLIENT_SECRET"),
    ) {
        return Ok(OAuthClient {
            client_id,
            client_secret,
        });
    }
    let path = dir().join("client.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "No OAuth client. Put {{\"client_id\":..,\"client_secret\":..}} in {} or set MYMAIL_CLIENT_ID / MYMAIL_CLIENT_SECRET.",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(err)
}

pub fn load_accounts() -> Vec<SavedAccount> {
    let path = dir().join("accounts.json");
    let Ok(text) = std::fs::read(&path) else {
        return vec![];
    };
    serde_json::from_slice(&text).unwrap_or_else(|_| {
        // Keep the unreadable file instead of overwriting its tokens on the next save.
        let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
        vec![]
    })
}

// shortcut: refresh tokens live in a 0600 file, not the OS keychain; move to keyring if the machine is shared.
pub fn save_accounts(accounts: &[SavedAccount]) -> Result<()> {
    std::fs::create_dir_all(dir()).map_err(err)?;
    let path = dir().join("accounts.json");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&path).map_err(err)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(err)?;
    f.write_all(
        serde_json::to_string_pretty(accounts)
            .map_err(err)?
            .as_bytes(),
    )
    .map_err(err)
}

// ---------- OAuth ----------

fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("os rng");
    URL_SAFE_NO_PAD.encode(buf)
}

/// A pending browser login: bind the loopback port, open `url`, then call `finish`.
pub struct Login {
    listener: TcpListener,
    verifier: String,
    state: String,
    redirect: String,
    pub url: String,
}

impl Login {
    pub fn start(client: &OAuthClient) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(err)?;
        let redirect = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().map_err(err)?.port()
        );
        let verifier = random_b64(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_b64(16);
        let url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&access_type=offline&prompt=consent\
             &client_id={}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            enc(&client.client_id),
            enc(&redirect),
            enc(SCOPE)
        );
        Ok(Login {
            listener,
            verifier,
            state,
            redirect,
            url,
        })
    }

    /// Blocks until the browser redirects back, then exchanges the code.
    pub fn finish(self, client: &OAuthClient) -> Result<SavedAccount> {
        // Browsers may open empty preconnect sockets or ask for /favicon.ico; wait for the redirect.
        let (mut stream, line) = loop {
            let (stream, _) = self.listener.accept().map_err(err)?;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_ok() && line.contains("state=") {
                break (stream, line);
            }
        };
        let query = line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .split_once('?')
            .map(|x| x.1)
            .unwrap_or("");
        let param = |k: &str| {
            query.split('&').find_map(|kv| {
                kv.split_once('=')
                    .filter(|(a, _)| *a == k)
                    .map(|(_, v)| dec(v))
            })
        };
        let ok = param("state").as_deref() == Some(self.state.as_str());
        let msg = if ok {
            "Signed in. You can close this tab."
        } else {
            "Sign-in failed."
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{msg}"
        );
        if !ok {
            return Err("OAuth state mismatch".into());
        }
        let code =
            param("code").ok_or_else(|| param("error").unwrap_or_else(|| "no code".into()))?;
        let resp: Value = ureq::post(TOKEN_URL)
            .send_form([
                ("code", code.as_str()),
                ("client_id", &client.client_id),
                ("client_secret", &client.client_secret),
                ("redirect_uri", &self.redirect),
                ("grant_type", "authorization_code"),
                ("code_verifier", &self.verifier),
            ])
            .map_err(err)?
            .body_mut()
            .read_json()
            .map_err(err)?;
        let refresh_token = resp["refresh_token"]
            .as_str()
            .ok_or("no refresh_token in response")?
            .to_string();
        let access = resp["access_token"].as_str().ok_or("no access_token")?;
        let profile: Value = ureq::get(format!("{API}/profile"))
            .header("Authorization", format!("Bearer {access}"))
            .call()
            .map_err(err)?
            .body_mut()
            .read_json()
            .map_err(err)?;
        let email = profile["emailAddress"]
            .as_str()
            .ok_or("no emailAddress")?
            .to_string();
        Ok(SavedAccount {
            email,
            refresh_token,
        })
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- API client ----------

#[derive(Clone, Debug)]
pub struct Summary {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub snippet: String,
    pub unread: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub from: String,
    pub reply_to: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
    /// Plain text: shown when there is no HTML, and used for reply/forward quoting.
    pub body: String,
    pub html: Option<String>,
}

pub struct Client {
    pub email: String,
    oauth: OAuthClient,
    refresh_token: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl Client {
    pub fn new(oauth: OAuthClient, acct: SavedAccount) -> Self {
        Client {
            email: acct.email,
            oauth,
            refresh_token: acct.refresh_token,
            token: Mutex::new(None),
        }
    }

    fn token(&self) -> Result<String> {
        let mut guard = self.token.lock().unwrap();
        if let Some((t, _)) = guard.as_ref().filter(|(_, exp)| *exp > Instant::now()) {
            return Ok(t.clone());
        }
        let resp: Value = ureq::post(TOKEN_URL)
            .send_form([
                ("client_id", self.oauth.client_id.as_str()),
                ("client_secret", &self.oauth.client_secret),
                ("refresh_token", &self.refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .map_err(|e| {
                format!(
                    "token refresh failed for {} ({e}); re-add the account",
                    self.email
                )
            })?
            .body_mut()
            .read_json()
            .map_err(err)?;
        let t = resp["access_token"]
            .as_str()
            .ok_or("no access_token")?
            .to_string();
        let ttl = resp["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(60);
        *guard = Some((t.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(t)
    }

    fn get(&self, path: &str) -> Result<Value> {
        ureq::get(format!("{API}{path}"))
            .header("Authorization", format!("Bearer {}", self.token()?))
            .call()
            .map_err(err)?
            .body_mut()
            .read_json()
            .map_err(err)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        ureq::post(format!("{API}{path}"))
            .header("Authorization", format!("Bearer {}", self.token()?))
            .send_json(body)
            .map_err(err)?
            .body_mut()
            .read_json()
            .map_err(err)
    }

    /// Gmail search syntax, newest first. Metadata is fetched in parallel.
    // shortcut: first 50 results only, no paging; add nextPageToken if searches need more.
    pub fn list(&self, query: &str) -> Result<Vec<Summary>> {
        let ids: Vec<String> =
            self.get(&format!("/messages?maxResults=50&q={}", enc(query)))?["messages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
        let results: Vec<Result<Summary>> = std::thread::scope(|s| {
            let handles: Vec<_> = ids
                .chunks(5)
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .iter()
                            .map(|id| {
                                self.get(&format!(
                                    "/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date"
                                ))
                                .map(|m| summary(&m))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        // Keep what loaded; one throttled message shouldn't blank the inbox.
        let (ok, errs): (Vec<_>, Vec<_>) = results.into_iter().partition(|r| r.is_ok());
        match (ok.is_empty(), errs.into_iter().next()) {
            (true, Some(Err(e))) => Err(e),
            _ => Ok(ok.into_iter().map(Result::unwrap).collect()),
        }
    }

    pub fn message(&self, id: &str) -> Result<Message> {
        let mut m = self.get(&format!("/messages/{id}?format=full"))?;
        inline_text_bodies(&mut m["payload"], &mut |att| {
            Ok(self.get(&format!("/messages/{id}/attachments/{att}"))?["data"].clone())
        })?;
        Ok(parse_message(&m))
    }

    pub fn modify(&self, id: &str, remove: &[&str]) -> Result<()> {
        self.post(
            &format!("/messages/{id}/modify"),
            json!({ "removeLabelIds": remove }),
        )
        .map(|_| ())
    }

    pub fn archive(&self, id: &str) -> Result<()> {
        self.modify(id, &["INBOX"])
    }

    pub fn send(&self, raw: &str, thread_id: Option<&str>) -> Result<()> {
        let mut body = json!({ "raw": URL_SAFE.encode(raw) });
        if let Some(t) = thread_id {
            body["threadId"] = t.into();
        }
        self.post("/messages/send", body).map(|_| ())
    }
}

/// Decode RFC 2047 encoded words (`=?UTF-8?B?...?=`); Gmail returns header values raw.
/// From/To/Cc stay raw in `Message` so they remain valid as reply recipients.
pub fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    let mut prev_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, tail) = rest.split_at(start);
        let word = (|| {
            let mut it = tail[2..].splitn(3, '?');
            let (cs, enc, more) = (it.next()?, it.next()?, it.next()?);
            let text = &more[..more.find("?=")?];
            let bytes = match enc {
                "B" | "b" => STANDARD.decode(text).ok()?,
                "Q" | "q" => q_decode(text),
                _ => return None,
            };
            let label = cs.split('*').next()?; // drop RFC 2231 language tag
            let encoding = encoding_rs::Encoding::for_label(label.as_bytes())?;
            Some((
                encoding.decode(&bytes).0.into_owned(),
                2 + cs.len() + 1 + enc.len() + 1 + text.len() + 2,
            ))
        })();
        match word {
            Some((decoded, len)) => {
                // Whitespace between adjacent encoded words is not part of the text.
                if !(prev_word && before.trim().is_empty()) {
                    out += before;
                }
                out += &decoded;
                rest = &tail[len..];
                prev_word = true;
            }
            None => {
                out += before;
                out += "=?";
                rest = &tail[2..];
                prev_word = false;
            }
        }
    }
    out + rest
}

fn q_decode(text: &str) -> Vec<u8> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'='),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn header(payload: &Value, name: &str) -> String {
    payload["headers"]
        .as_array()
        .and_then(|hs| {
            hs.iter().find(|h| {
                h["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
        })
        .and_then(|h| h["value"].as_str())
        .unwrap_or("")
        .to_string()
}

fn summary(m: &Value) -> Summary {
    let p = &m["payload"];
    Summary {
        id: m["id"].as_str().unwrap_or("").into(),
        from: display_name(&decode_words(&header(p, "From"))),
        subject: decode_words(&header(p, "Subject")),
        date: header(p, "Date"),
        snippet: unescape_html(m["snippet"].as_str().unwrap_or("")),
        unread: m["labelIds"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x == "UNREAD")),
    }
}

/// `"Jane Doe" <jane@x.com>` -> `jane@x.com`
pub fn address(from: &str) -> &str {
    from.rsplit_once('<')
        .map_or(from, |(_, a)| a.trim_end_matches('>'))
        .trim()
}

/// `"Jane Doe" <jane@x.com>` -> `Jane Doe`
pub fn display_name(from: &str) -> String {
    match from.split_once('<') {
        Some((name, _)) if !name.trim().is_empty() => name.trim().trim_matches('"').to_string(),
        _ => from.trim().trim_matches(['<', '>']).to_string(),
    }
}

fn unescape_html(s: &str) -> String {
    s.replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn parse_message(m: &Value) -> Message {
    let p = &m["payload"];
    let (mut plain, mut html) = (None, None);
    find_bodies(p, &mut plain, &mut html);
    // Prefer HTML: senders' text/plain parts are often empty, placeholders, or spacer junk.
    let html_text = html.as_deref().map(html_to_text).unwrap_or_default();
    // HTML with nothing visible (no text, no images) counts as absent.
    let html =
        html.filter(|h| !html_text.trim().is_empty() || h.to_ascii_lowercase().contains("<img"));
    let body = Some(html_text)
        .filter(|t| !t.trim().is_empty())
        .or(plain)
        .unwrap_or_default();
    Message {
        id: m["id"].as_str().unwrap_or("").into(),
        thread_id: m["threadId"].as_str().unwrap_or("").into(),
        from: header(p, "From"),
        reply_to: header(p, "Reply-To"),
        to: header(p, "To"),
        cc: header(p, "Cc"),
        subject: decode_words(&header(p, "Subject")),
        date: header(p, "Date"),
        message_id: header(p, "Message-ID"),
        references: header(p, "References"),
        body: body.replace("\r\n", "\n"),
        html,
    }
}

/// Large text parts arrive as `body.attachmentId` instead of `body.data`; fetch them inline.
fn inline_text_bodies(
    part: &mut Value,
    fetch: &mut impl FnMut(&str) -> Result<Value>,
) -> Result<()> {
    let is_text = matches!(part["mimeType"].as_str(), Some("text/plain" | "text/html"));
    let no_name = part["filename"].as_str().is_none_or(str::is_empty);
    if is_text
        && no_name
        && part["body"]["data"].as_str().is_none_or(str::is_empty)
        && let Some(att) = part["body"]["attachmentId"].as_str()
    {
        part["body"]["data"] = fetch(att)?;
    }
    for p in part["parts"].as_array_mut().into_iter().flatten() {
        inline_text_bodies(p, fetch)?;
    }
    Ok(())
}

/// Decode with the part's declared charset (e.g. ISO-8859-1); UTF-8 when absent or unknown.
fn decode_charset(content_type: &str, bytes: &[u8]) -> String {
    let label = content_type
        .split(';')
        .find_map(|p| {
            p.split_once('=')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("charset"))
        })
        .map(|(_, v)| v.trim().trim_matches('"'))
        .unwrap_or("utf-8");
    let enc = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

/// Remove invisible preheader padding. ZWJ/ZWNJ are kept between visible characters,
/// where emoji sequences and Persian/Indic text need them.
fn strip_padding(line: &str) -> String {
    const PAD: &str = "\u{34f}\u{200b}\u{2800}\u{ad}\u{feff}";
    let invisible = |c: Option<&char>| {
        c.is_none_or(|c| {
            c.is_whitespace() || PAD.contains(*c) || matches!(c, '\u{200c}' | '\u{200d}')
        })
    };
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    for (i, c) in chars.iter().enumerate() {
        let joiner = matches!(c, '\u{200c}' | '\u{200d}');
        let keep = if PAD.contains(*c) {
            false
        } else if joiner {
            !invisible(i.checked_sub(1).and_then(|p| chars.get(p))) && !invisible(chars.get(i + 1))
        } else {
            true
        };
        if keep {
            out.push(*c);
        }
    }
    out
}

/// Readable plain text: no markup, link footnotes or table borders; lines wrap in the reader.
fn html_to_text(html: &str) -> String {
    let text = html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
        .no_table_borders()
        .link_footnotes(false)
        .string_from_read(html.as_bytes(), 400)
        .unwrap_or_default();
    // Drop invisible preheader padding and collapse the blank runs that layout tables leave.
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = strip_padding(line);
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out += line;
        out += "\n";
    }
    out.trim().to_string()
}

fn find_bodies(part: &Value, plain: &mut Option<String>, html: &mut Option<String>) {
    let mime = part["mimeType"].as_str().unwrap_or("");
    if part["filename"].as_str().is_some_and(|f| !f.is_empty()) {
        return; // attachment
    }
    if let Some(data) = part["body"]["data"].as_str() {
        let decoded = URL_SAFE
            .decode(data)
            .or_else(|_| URL_SAFE_NO_PAD.decode(data));
        if let Ok(bytes) = decoded {
            let text = decode_charset(&header(part, "Content-Type"), &bytes);
            match mime {
                "text/plain" if plain.is_none() => *plain = Some(text),
                "text/html" if html.is_none() => *html = Some(text),
                _ => {}
            }
        }
    }
    for p in part["parts"].as_array().into_iter().flatten() {
        find_bodies(p, plain, html);
    }
}

// ---------- MIME building ----------

/// Header values from user input must not carry CR/LF (header injection).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect()
}

fn encode_word(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(s))
    }
}

fn quote(msg: &Message) -> String {
    let q: String = msg.body.lines().map(|l| format!("> {l}\n")).collect();
    format!("\nOn {}, {} wrote:\n{q}", msg.date, decode_words(&msg.from))
}

pub fn prefixed(prefix: &str, subject: &str) -> String {
    if subject
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        subject.to_string()
    } else {
        format!("{prefix} {subject}")
    }
}

fn build(from: &str, to: &str, subject: &str, extra: &[(&str, &str)], body: &str) -> String {
    let mut out = format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n",
        clean(from),
        clean(to),
        encode_word(&clean(subject))
    );
    for (k, v) in extra {
        if !v.is_empty() {
            out += &format!("{k}: {}\r\n", clean(v));
        }
    }
    out += "\r\n";
    let b64 = STANDARD.encode(body.replace("\r\n", "\n").replace('\n', "\r\n"));
    for chunk in b64.as_bytes().chunks(76) {
        out += std::str::from_utf8(chunk).unwrap();
        out += "\r\n";
    }
    out
}

pub fn reply_raw(me: &str, to: &str, msg: &Message, text: &str) -> String {
    let refs = format!("{} {}", msg.references, msg.message_id)
        .trim()
        .to_string();
    build(
        me,
        to,
        &prefixed("Re:", &msg.subject),
        &[("In-Reply-To", &msg.message_id), ("References", &refs)],
        &format!("{text}\n{}", quote(msg)),
    )
}

pub fn forward_raw(me: &str, to: &str, msg: &Message, text: &str) -> String {
    let fwd = format!(
        "{text}\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n{}",
        decode_words(&msg.from),
        msg.date,
        msg.subject,
        decode_words(&msg.to),
        msg.body
    );
    build(me, to, &prefixed("Fwd:", &msg.subject), &[], &fwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_body(raw: &str) -> String {
        let b64: String = raw
            .split("\r\n\r\n")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .collect();
        String::from_utf8(STANDARD.decode(b64).unwrap()).unwrap()
    }

    fn sample() -> Message {
        Message {
            id: "1".into(),
            thread_id: "t1".into(),
            from: "Jane <jane@x.com>".into(),
            to: "me@x.com".into(),
            subject: "Hello".into(),
            date: "Mon, 1 Jan 2026".into(),
            message_id: "<abc@x.com>".into(),
            references: "<prev@x.com>".into(),
            body: "line1\nline2".into(),
            ..Default::default()
        }
    }

    #[test]
    fn reply_threads_and_quotes() {
        let raw = reply_raw("me@x.com", "Jane <jane@x.com>", &sample(), "Thanks");
        assert!(raw.contains("Subject: Re: Hello\r\n"));
        assert!(raw.contains("In-Reply-To: <abc@x.com>\r\n"));
        assert!(raw.contains("References: <prev@x.com> <abc@x.com>\r\n"));
        let body = decode_body(&raw);
        assert!(body.starts_with("Thanks\r\n"));
        assert!(body.contains("> line1\r\n> line2"));
    }

    #[test]
    fn forward_and_header_injection() {
        let raw = forward_raw("me@x.com", "a@b.c\r\nBcc: evil@x.com", &sample(), "fyi");
        assert!(raw.contains("Subject: Fwd: Hello\r\n"));
        assert!(!raw.contains("\r\nBcc:"));
        assert!(decode_body(&raw).contains("Forwarded message"));
    }

    #[test]
    fn subject_encoding_and_prefix() {
        assert_eq!(prefixed("Re:", "RE: x"), "RE: x");
        let mut m = sample();
        m.subject = "Hyvää päivää".into();
        let raw = reply_raw("me@x.com", "a@b.c", &m, "");
        assert!(raw.contains("Subject: =?UTF-8?B?"));
    }

    #[test]
    fn parses_multipart_and_html_fallback() {
        let data = URL_SAFE.encode("hi there");
        let m = json!({"id":"1","threadId":"t","payload":{"mimeType":"multipart/alternative",
            "headers":[{"name":"Subject","value":"S"},{"name":"Message-Id","value":"<m@x>"}],
            "parts":[{"mimeType":"text/html","body":{"data":URL_SAFE.encode("<p>html</p>")}},
                     {"mimeType":"text/plain","body":{"data":data}}]}});
        let msg = parse_message(&m);
        assert_eq!(msg.body, "html"); // HTML wins over text/plain
        assert_eq!(msg.message_id, "<m@x>");
        let m = json!({"payload":{"mimeType":"text/html","body":{"data":URL_SAFE.encode("<p>only <b>html</b></p>")}}});
        assert!(parse_message(&m).body.contains("only"));
        // Placeholder plain part + real HTML with preheader padding and footnote-free links.
        let m = json!({"payload":{"mimeType":"multipart/alternative","parts":[
            {"mimeType":"text/plain","body":{"data":URL_SAFE.encode("Plain text version not available")}},
            {"mimeType":"text/html","body":{"data":URL_SAFE.encode(
                "<p>Deal \u{34f}\u{2800}\u{200c} \u{34f}\u{2800}\u{200c}</p><table><tr><td><a href='https://x.y'>Shop</a></td></tr></table>")}}]}});
        assert_eq!(parse_message(&m).body, "Deal\n\nShop");
        // Empty HTML falls back to plain.
        let m = json!({"payload":{"mimeType":"multipart/alternative","parts":[
            {"mimeType":"text/plain","body":{"data":URL_SAFE.encode("hi")}},
            {"mimeType":"text/html","body":{"data":URL_SAFE.encode("<div> </div>")}}]}});
        let msg = parse_message(&m);
        assert_eq!(msg.body, "hi");
        assert!(
            msg.html.is_none(),
            "reader must fall back to the plain part"
        );
    }

    #[test]
    fn fetches_text_bodies_stored_as_attachments() {
        let mut m = json!({"id":"1","payload":{"mimeType":"multipart/mixed","parts":[
            {"mimeType":"text/plain","filename":"","body":{"attachmentId":"A1","size":90000}},
            {"mimeType":"text/html","body":{"attachmentId":"A3","data":""}},
            {"mimeType":"application/pdf","filename":"x.pdf","body":{"attachmentId":"A2"}}]}});
        let mut asked = vec![];
        inline_text_bodies(&mut m["payload"], &mut |att| {
            asked.push(att.to_string());
            Ok(json!(URL_SAFE.encode("big body")))
        })
        .unwrap();
        assert_eq!(asked, ["A1", "A3"]);
        assert_eq!(parse_message(&m).body, "big body");
    }

    #[test]
    fn login_skips_preconnects_and_checks_state() {
        let client = OAuthClient {
            client_id: "id".into(),
            client_secret: "s".into(),
        };
        let login = Login::start(&client).unwrap();
        let addr = login.listener.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            drop(std::net::TcpStream::connect(addr).unwrap()); // empty preconnect
            let mut fav = std::net::TcpStream::connect(addr).unwrap();
            fav.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").unwrap();
            drop(fav);
            let mut cb = std::net::TcpStream::connect(addr).unwrap();
            cb.write_all(b"GET /?state=forged&code=x HTTP/1.1\r\n\r\n")
                .unwrap();
            let mut resp = String::new();
            let _ = std::io::Read::read_to_string(&mut cb, &mut resp);
            resp
        });
        assert_eq!(
            login.finish(&client).err().as_deref(),
            Some("OAuth state mismatch")
        );
        assert!(t.join().unwrap().contains("Sign-in failed"));
    }

    #[test]
    fn decodes_declared_charset() {
        let m = json!({"payload":{"mimeType":"text/plain",
            "headers":[{"name":"Content-Type","value":"text/plain; Charset = \"ISO-8859-1\""}],
            "body":{"data":URL_SAFE.encode(b"Hyv\xe4\xe4 p\xe4iv\xe4\xe4")}}});
        assert_eq!(parse_message(&m).body, "Hyvää päivää");
    }

    #[test]
    fn decodes_encoded_words() {
        assert_eq!(
            decode_words("=?UTF-8?B?SHl2w6TDpCBww6RpdsOkw6Q=?="),
            "Hyvää päivää"
        );
        assert_eq!(
            decode_words("=?iso-8859-1?Q?J=E4ne_D?= <j@x.com>"),
            "Jäne D <j@x.com>"
        );
        assert_eq!(decode_words("=?UTF-8?Q?a?= =?UTF-8?Q?b?= c"), "ab c");
        assert_eq!(decode_words("plain =? not encoded"), "plain =? not encoded");
    }

    #[test]
    fn padding_strip_keeps_joiners() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(
            strip_padding(&format!("Hi {family}")),
            format!("Hi {family}")
        );
        assert_eq!(strip_padding("می\u{200c}خواهم"), "می\u{200c}خواهم");
        assert_eq!(
            strip_padding("Deal \u{34f}\u{2800}\u{200c} \u{34f}\u{2800}\u{200c}").trim_end(),
            "Deal"
        );
    }

    #[test]
    fn url_codec_and_names() {
        assert_eq!(dec(&enc("a b/c?d=é")), "a b/c?d=é");
        assert_eq!(dec("4%2F0A%zz"), "4/0A%zz");
        assert_eq!(display_name("\"Jane D\" <j@x.com>"), "Jane D");
        assert_eq!(display_name("<j@x.com>"), "j@x.com");
        assert_eq!(address("\"Jane\" <J@x.com>"), "J@x.com");
        assert_eq!(address("j@x.com"), "j@x.com");
    }
}
