// claude-usage: Claude usage (5 h session + week) for the Plasma widget,
// plus PC and phone notifications when a limit resets.
//
// No daemon: the widget runs this from time to time (every 10 to 30 min).
// Rust port of the old Python script: same files, same JSON, a few ms of CPU.
//
//   claude-usage            -> one JSON line (read by the widget)
//   claude-usage --status   -> human summary
//   claude-usage --test     -> test notification on the PC and the phone
//   claude-usage --agenda [client_secret.json]
//                           -> connect Google Calendar once (browser), then sync now
//
// Google Calendar: one event in the primary calendar at each weekly reset, and at
// the 5 h reset only once the 5 h limit hit 100 %. Our events carry a private
// "claude-usage" property; anything without it is never changed or deleted.

use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"; // Claude Code's
const UA: &str = "claude-usage-widget/2.0";
const NTFY_MAX_DELAY: f64 = (3 * 86400 - 900) as f64; // ntfy.sh schedules at most 3 days ahead
const MIN_GAP: f64 = 120.0; // at most one API call every 2 min
const RATE_GAP: f64 = 300.0; // after an HTTP 429, 5 min without calling the API

const GOOGLE_AUTH: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";
const EVENTS: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.owned";
const USAGE_PAGE: &str = "https://claude.ai/settings/usage";
const MARK: &str = "claude-usage";

const WINDOWS: [(&str, &str, &str); 2] = [
    ("five_hour", "Claude : session réinitialisée", "Ta limite de session (5 h) est revenue à 0 %"),
    ("seven_day", "Claude : semaine réinitialisée", "Ta limite hebdomadaire est revenue à 0 %"),
];

struct Paths {
    cred: PathBuf,
    conf: PathBuf,
    google: PathBuf,
    sdir: PathBuf,
    state: PathBuf,
}

fn paths() -> Paths {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let xdg = |var: &str, def: &str| match std::env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(def),
    };
    let sdir = xdg("XDG_STATE_HOME", ".local/state").join("claude-usage");
    let cdir = xdg("XDG_CONFIG_HOME", ".config").join("claude-usage");
    Paths {
        cred: home.join(".claude/.credentials.json"),
        conf: cdir.join("config.json"),
        google: cdir.join("google.json"),
        state: sdir.join("state.json"),
        sdir,
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn load(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn save(path: &Path, obj: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string(obj)?.as_bytes())?;
    drop(f);
    fs::rename(tmp, path)
}

fn lock(path: &Path) -> std::io::Result<File> {
    let f = OpenOptions::new().write(true).create(true).truncate(false).open(path)?;
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    Ok(f) // released when dropped
}

// curl reads its whole request from stdin (-K -), so tokens never show up in `ps`.
fn http(url: &str, data: Option<&Value>, headers: &[(&str, String)], timeout: u32) -> Result<(u16, Vec<u8>), String> {
    let method = if data.is_some() { "POST" } else { "GET" };
    request(method, url, data.map(|d| ("application/json", d.to_string())), headers, timeout)
}

fn request(method: &str, url: &str, body: Option<(&str, String)>, headers: &[(&str, String)], timeout: u32) -> Result<(u16, Vec<u8>), String> {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut cfg = format!("url = {}\nrequest = {method}\nsilent\nmax-time = {timeout}\nwrite-out = \"\\n%{{http_code}}\"\n", quote(url));
    cfg += &format!("header = {}\n", quote(&format!("User-Agent: {UA}")));
    for (k, v) in headers {
        cfg += &format!("header = {}\n", quote(&format!("{k}: {v}")));
    }
    if let Some((ctype, d)) = body {
        cfg += &format!("header = {}\n", quote(&format!("Content-Type: {ctype}")));
        cfg += &format!("data-binary = {}\n", quote(&d));
    }
    let mut child = Command::new("curl")
        .args(["-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("curl : {e}"))?;
    child.stdin.take().unwrap().write_all(cfg.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let body = out.stdout;
    let cut = body.iter().rposition(|&b| b == b'\n').ok_or("pas de réponse")?;
    let code = std::str::from_utf8(&body[cut + 1..]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if code == 0 {
        return Err("pas de réseau".into());
    }
    Ok((code, body[..cut].to_vec()))
}

// ISO 8601 -> epoch rounded to the minute (the API returns varying microseconds)
fn ts(iso: &str) -> Option<i64> {
    let b = iso.as_bytes();
    let num = |a: usize, n: usize| iso.get(a..a + n)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut i = 19;
    let mut frac = 0.0;
    if b.get(i) == Some(&b'.') {
        let start = i;
        i += 1;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        frac = iso[start..i].parse::<f64>().unwrap_or(0.0);
    }
    let offset = match b.get(i) {
        Some(b'+') | Some(b'-') => {
            let sign = if b[i] == b'-' { -1 } else { 1 };
            sign * (num(i + 1, 2)? * 3600 + num(i + 4, 2)? * 60)
        }
        _ => 0,
    };
    // days since 1970-01-01 (Howard Hinnant's civil algorithm)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let t = (days * 86400 + h * 3600 + mi * 60 + s - offset) as f64 + frac;
    Some(((t / 60.0).round_ties_even() * 60.0) as i64)
}

fn local(t: i64) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    tm
}

fn hm(t: i64) -> String {
    let tm = local(t);
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

// Token: we read Claude Code's; if it expired (overnight), refresh it exactly
// like a second Claude Code window would, then write it back.
fn token(p: &Paths) -> Result<String, String> {
    let fresh = |o: &Value| o["expiresAt"].as_f64().unwrap_or(0.0) > now() * 1000.0 + 60_000.0;
    let cred = Value::Object(load(&p.cred));
    let o = &cred["claudeAiOauth"];
    let Some(access) = o["accessToken"].as_str().filter(|s| !s.is_empty()) else {
        return Err("pas connecté : lance `claude` une fois".into());
    };
    if fresh(o) {
        return Ok(access.to_string());
    }
    fs::create_dir_all(&p.sdir).map_err(|e| e.to_string())?;
    let _lk = lock(&p.sdir.join("refresh.lock")).map_err(|e| e.to_string())?;
    let cred: Value = serde_json::from_str(&fs::read_to_string(&p.cred).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let o = &cred["claudeAiOauth"];
    if fresh(o) {
        return Ok(o["accessToken"].as_str().unwrap_or_default().to_string()); // refreshed meanwhile by Claude Code
    }
    let used = o["refreshToken"].as_str().unwrap_or_default().to_string();
    let scopes: Vec<&str> = o["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect()).unwrap_or_default();
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": used,
        "client_id": CLIENT_ID,
        "scope": scopes.join(" "),
    });
    let (st, resp) = http(TOKEN_URL, Some(&body), &[], 30)?;
    if st != 200 {
        return Err(format!("jeton refusé ({st}) : lance `claude` une fois"));
    }
    let d: Value = serde_json::from_slice(&resp).map_err(|e| e.to_string())?;
    let new_access = d["access_token"].as_str().ok_or("réponse du jeton illisible")?.to_string();
    let t = now() * 1000.0;
    let mut cred: Value = serde_json::from_str(&fs::read_to_string(&p.cred).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let o = cred["claudeAiOauth"].as_object_mut().ok_or("identifiants illisibles")?;
    if o.get("refreshToken").and_then(|v| v.as_str()) != Some(used.as_str()) {
        return Ok(new_access); // Claude Code wrote a newer token: leave it alone
    }
    o.insert("accessToken".into(), json!(new_access));
    o.insert("refreshToken".into(), json!(d["refresh_token"].as_str().unwrap_or(&used)));
    o.insert("expiresAt".into(), json!((t + d["expires_in"].as_f64().unwrap_or(0.0) * 1000.0) as i64));
    if let Some(r) = d["refresh_token_expires_in"].as_f64().filter(|r| *r != 0.0) {
        o.insert("refreshTokenExpiresAt".into(), json!((t + r * 1000.0) as i64));
    }
    if let Some(sc) = d["scope"].as_str().filter(|s| !s.is_empty()) {
        o.insert("scopes".into(), json!(sc.split_whitespace().collect::<Vec<_>>()));
    }
    save(&p.cred, &cred).map_err(|e| e.to_string())?;
    Ok(new_access)
}

enum Fail {
    RateLimited,
    Other(String),
}

fn fetch(p: &Paths) -> Result<Value, Fail> {
    let tok = token(p).map_err(Fail::Other)?;
    let headers = [
        ("Authorization", format!("Bearer {tok}")),
        ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ("Accept", "application/json".to_string()),
    ];
    let (st, body) = http(USAGE_URL, None, &headers, 15).map_err(Fail::Other)?;
    match st {
        429 => Err(Fail::RateLimited),
        200 => serde_json::from_slice(&body).map_err(|e| Fail::Other(e.to_string())),
        _ => Err(Fail::Other(format!("API usage : HTTP {st}"))),
    }
}

fn desktop(title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "Claude", "-i", "chronometer", title, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ntfy push. With `at`, the ntfy server sends it at that exact time,
// so it arrives even if the PC is off by then.
fn phone(cfg: &Map<String, Value>, title: &str, body: &str, at: Option<i64>) -> bool {
    let Some(topic) = cfg.get("ntfy_topic").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) else {
        return false;
    };
    let mut msg = json!({"topic": topic, "title": title, "message": body,
                         "tags": ["arrows_counterclockwise"], "click": "https://claude.ai"});
    if let Some(at) = at {
        msg["delay"] = json!(at.to_string());
    }
    let server = cfg.get("ntfy_server").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).unwrap_or("https://ntfy.sh");
    matches!(http(server, Some(&msg), &[], 10), Ok((200, _)))
}

fn obj<'a>(st: &'a mut Map<String, Value>, k: &str) -> &'a mut Map<String, Value> {
    let v = st.entry(k).or_insert_with(|| json!({}));
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().unwrap()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&")
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => match s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn b64url(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..=c.len() {
            s.push(A[(n >> (18 - 6 * k) & 63) as usize] as char);
        }
    }
    s
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| e.to_string())?;
    Ok(b)
}

fn sha256(s: &str) -> Option<Vec<u8>> {
    let mut c = Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    c.stdin.take()?.write_all(s.as_bytes()).ok()?;
    let hex = String::from_utf8(c.wait_with_output().ok()?.stdout).ok()?;
    (0..32).map(|i| hex.get(2 * i..2 * i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

fn utc(t: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::gmtime_r(&tt, &mut tm) };
    format!("{}-{:02}-{:02}T{:02}:{:02}:{:02}Z", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn str_of(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

// One-time Google consent: loopback redirect + PKCE, as Google wants for desktop apps.
fn connect(p: &Paths, client: Option<&str>) -> Result<(), String> {
    let mut g = load(&p.google);
    if let Some(f) = client {
        let c = Value::Object(load(Path::new(f)));
        let c = if c["installed"].is_object() { &c["installed"] } else { &c };
        let (Some(id), Some(secret)) = (c["client_id"].as_str(), c["client_secret"].as_str()) else {
            return Err("fichier client OAuth illisible (il faut un client « Application de bureau »)".into());
        };
        g = Map::new();
        g.insert("client_id".into(), json!(id));
        g.insert("client_secret".into(), json!(secret));
    }
    let (id, secret) = (str_of(&g, "client_id"), str_of(&g, "client_secret"));
    if id.is_empty() {
        return Err("donne le fichier du client OAuth : claude-usage --agenda client_secret….json".into());
    }
    let srv = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}", srv.local_addr().map_err(|e| e.to_string())?.port());
    let verifier = b64url(&random(32)?);
    let challenge = b64url(&sha256(&verifier).ok_or("sha256sum introuvable")?);
    let state = b64url(&random(12)?);
    let url = format!(
        "{GOOGLE_AUTH}?{}",
        form(&[
            ("client_id", &id),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ])
    );
    println!("Autorise l'accès dans le navigateur (sinon ouvre ce lien) :\n{url}\n");
    let _ = Command::new("xdg-open").arg(&url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();

    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(1800));
        eprintln!("Agenda : pas de réponse du navigateur en 30 min, relance claude-usage --agenda");
        std::process::exit(1);
    });
    let code = loop {
        let (mut s, _) = srv.accept().map_err(|e| e.to_string())?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).into_owned();
        let query = req.split_whitespace().nth(1).and_then(|p| p.split_once('?')).map(|x| x.1).unwrap_or("");
        let get = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(dec);
        let (msg, done) = match (get("code"), get("error")) {
            (Some(c), _) if get("state").as_deref() == Some(state.as_str()) => {
                ("C'est fait : claude-usage peut écrire dans ton agenda. Tu peux fermer cet onglet.", Some(Ok(c)))
            }
            (_, Some(e)) => ("Accès refusé, rien n'a changé.", Some(Err(format!("Google a répondu : {e}")))),
            _ => ("", None),
        };
        let resp = if msg.is_empty() {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        } else {
            let page = format!("<!doctype html><meta charset=utf-8><title>claude-usage</title><body style=\"background:#000;color:#ddd;font:18px sans-serif;padding:40px\">{msg}");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len())
        };
        let _ = s.write_all(resp.as_bytes());
        if let Some(r) = done {
            break r?;
        }
    };

    let body = form(&[
        ("code", &code),
        ("client_id", &id),
        ("client_secret", &secret),
        ("redirect_uri", &redirect),
        ("grant_type", "authorization_code"),
        ("code_verifier", &verifier),
    ]);
    let (st, resp) = request("POST", GOOGLE_TOKEN, Some(("application/x-www-form-urlencoded", body)), &[], 30)?;
    let d: Value = serde_json::from_slice(&resp).unwrap_or(Value::Null);
    let Some(refresh) = d["refresh_token"].as_str().filter(|s| !s.is_empty()) else {
        return Err(format!("Google n'a pas donné d'accès durable (HTTP {st} {})", d["error"].as_str().unwrap_or("")));
    };
    if !d["scope"].as_str().unwrap_or("").contains("calendar") {
        return Err("la case « Google Agenda » n'était pas cochée : relance et coche-la".into());
    }
    g.insert("refresh_token".into(), json!(refresh));
    g.insert("access_token".into(), json!(d["access_token"]));
    g.insert("expires".into(), json!(now() as i64 + d["expires_in"].as_i64().unwrap_or(3600)));
    save(&p.google, &Value::Object(g)).map_err(|e| e.to_string())
}

// Err((true, …)) = Google withdrew the access: stop trying until --agenda again.
fn google_token(p: &Paths) -> Result<String, (bool, String)> {
    let mut g = load(&p.google);
    let access = str_of(&g, "access_token");
    if !access.is_empty() && g.get("expires").and_then(|v| v.as_f64()).unwrap_or(0.0) > now() + 60.0 {
        return Ok(access);
    }
    let (id, secret, refresh) = (str_of(&g, "client_id"), str_of(&g, "client_secret"), str_of(&g, "refresh_token"));
    if refresh.is_empty() {
        return Err((true, "agenda pas branché : claude-usage --agenda".into()));
    }
    let body = form(&[("client_id", &id), ("client_secret", &secret), ("refresh_token", &refresh), ("grant_type", "refresh_token")]);
    let (st, resp) = request("POST", GOOGLE_TOKEN, Some(("application/x-www-form-urlencoded", body)), &[], 20).map_err(|e| (false, e))?;
    let d: Value = serde_json::from_slice(&resp).unwrap_or(Value::Null);
    let Some(access) = d["access_token"].as_str().filter(|_| st == 200) else {
        let e = format!("Google refuse le jeton (HTTP {st} {})", d["error"].as_str().unwrap_or(""));
        return Err((st == 400 || st == 401, e));
    };
    g.insert("access_token".into(), json!(access));
    g.insert("expires".into(), json!(now() as i64 + d["expires_in"].as_i64().unwrap_or(3600)));
    let _ = save(&p.google, &Value::Object(g));
    Ok(access.to_string())
}

fn cal(tok: &str, method: &str, url: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
    let auth = [("Authorization", format!("Bearer {tok}"))];
    let (st, b) = request(method, url, body.map(|v| ("application/json", v.to_string())), &auth, 20)?;
    Ok((st, serde_json::from_slice(&b).unwrap_or(Value::Null)))
}

fn cal_err(what: &str, st: u16, d: &Value) -> String {
    let m: String = d["error"]["message"].as_str().unwrap_or("").chars().take(80).collect();
    format!("Google Agenda, {what} : HTTP {st} {m}")
}

fn event(k: &str, title: &str, r: i64) -> Value {
    json!({
        "summary": title,
        "description": format!("Ton utilisation : {USAGE_PAGE}\n\nAjouté tout seul par claude-usage, sur le PC."),
        "start": {"dateTime": utc(r)},
        "end": {"dateTime": utc(r)},
        "colorId": "3",
        "transparency": "transparent",
        "reminders": {"useDefault": false},
        "extendedProperties": {"private": {MARK: k}},
    })
}

fn ours(ev: &Value, k: &str) -> bool {
    ev["extendedProperties"]["private"][MARK].as_str() == Some(k)
}

// Our own ids (base32hex: a-v and digits), so a lost state.json never makes a duplicate.
fn create(tok: &str, k: &str, title: &str, r: i64) -> Result<String, String> {
    let id = format!("claudeusage{}{r}", if k == "five_hour" { "cinqh" } else { "semaine" });
    let mut ev = event(k, title, r);
    ev["id"] = json!(id);
    let (st, d) = cal(tok, "POST", EVENTS, Some(&ev))?;
    if st == 200 {
        return Ok(id);
    }
    if st != 409 {
        return Err(cal_err("création", st, &d));
    }
    let url = format!("{EVENTS}/{id}"); // already exists, maybe deleted earlier: bring it back
    let (st, cur) = cal(tok, "GET", &url, None)?;
    if st != 200 || !ours(&cur, k) {
        return Err(cal_err("événement déjà là sans notre marque", st, &cur));
    }
    ev["status"] = json!("confirmed");
    let (st, d) = cal(tok, "PUT", &url, Some(&ev))?;
    if st == 200 { Ok(id) } else { Err(cal_err("remise en place", st, &d)) }
}

fn moveto(tok: &str, id: &str, k: &str, title: &str, r: i64) -> Result<String, String> {
    let url = format!("{EVENTS}/{id}");
    let (st, cur) = cal(tok, "GET", &url, None)?;
    if st == 404 || st == 410 || cur["status"] == "cancelled" || (st == 200 && !ours(&cur, k)) {
        return create(tok, k, title, r);
    }
    if st != 200 {
        return Err(cal_err("lecture", st, &cur));
    }
    let (st, d) = cal(tok, "PATCH", &url, Some(&json!({"start": {"dateTime": utc(r)}, "end": {"dateTime": utc(r)}})))?;
    if st == 200 { Ok(id.to_string()) } else { Err(cal_err("déplacement", st, &d)) }
}

fn remove(tok: &str, id: &str, k: &str) -> Result<&'static str, String> {
    let url = format!("{EVENTS}/{id}");
    let (st, cur) = cal(tok, "GET", &url, None)?;
    if st == 404 || st == 410 || cur["status"] == "cancelled" {
        return Ok("déjà plus dans l'agenda");
    }
    if st != 200 {
        return Err(cal_err("lecture", st, &cur));
    }
    if !ours(&cur, k) {
        return Ok("pas touché, l'événement n'a pas la marque claude-usage");
    }
    let (st, d) = cal(tok, "DELETE", &url, None)?;
    if st == 204 || st == 410 { Ok("retiré de l'agenda") } else { Err(cal_err("suppression", st, &d)) }
}

// Called only after a successful usage fetch (so never offline); a failure is retried next time.
fn agenda(p: &Paths, st: &mut Map<String, Value>, t: f64) -> Vec<String> {
    let mut log = Vec::new();
    if !p.google.exists() || obj(st, "agenda").get("off").and_then(|v| v.as_bool()) == Some(true) {
        return log;
    }
    let mut todo = Vec::new();
    for (k, title, _) in WINDOWS {
        let last = st.get("last").and_then(|l| l.get(k)).cloned().unwrap_or(json!({}));
        let (u, r) = (last["u"].as_f64().unwrap_or(0.0), last["r"].as_i64());
        let have = obj(st, "agenda").get(k).cloned().unwrap_or(json!({}));
        let (had_r, had_id) = (have["r"].as_i64(), have["id"].as_str().map(String::from));
        if had_r.is_some_and(|hr| hr as f64 <= t) {
            obj(st, "agenda").remove(k); // reset passed: the event stays as history
            continue;
        }
        let want = if k == "five_hour" { r.filter(|&r| u >= 100.0 || had_r == Some(r)) } else { r };
        if want != had_r {
            todo.push((k, title, want, had_id));
        }
    }
    if todo.is_empty() {
        obj(st, "agenda").remove("err");
        return log;
    }
    let tok = match google_token(p) {
        Ok(tok) => tok,
        Err((gone, e)) => {
            if gone {
                obj(st, "agenda").insert("off".into(), json!(true));
                desktop("Claude : agenda débranché", "Google a retiré l'accès. Pour le rebrancher : claude-usage --agenda");
            }
            obj(st, "agenda").insert("err".into(), json!(e));
            log.push(e);
            return log;
        }
    };
    let mut err = String::new();
    for (k, title, want, had_id) in todo {
        let res = match (want, had_id.as_deref()) {
            (Some(r), id) => match id {
                None => create(&tok, k, title, r),
                Some(id) => moveto(&tok, id, k, title, r),
            }
            .map(|id| {
                let moved = had_id.as_deref() == Some(id.as_str());
                obj(st, "agenda").insert(k.into(), json!({"id": id, "r": r}));
                format!("{} {}", if moved { "déplacé au" } else { "ajouté le" }, quand(r))
            }),
            (None, Some(id)) => remove(&tok, id, k).map(|m| {
                obj(st, "agenda").remove(k);
                m.to_string()
            }),
            (None, None) => continue,
        };
        match res {
            Ok(m) => log.push(format!("{title} : {m}")),
            Err(e) => {
                log.push(e.clone());
                err = e;
            }
        }
    }
    if err.is_empty() {
        obj(st, "agenda").remove("err");
    } else {
        obj(st, "agenda").insert("err".into(), json!(err));
    }
    log
}

fn quand(r: i64) -> String {
    const DAYS: [&str; 7] = ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."];
    let tm = local(r);
    format!("{} {:02}/{:02} {:02}:{:02}", DAYS[tm.tm_wday as usize], tm.tm_mday, tm.tm_mon + 1, tm.tm_hour, tm.tm_min)
}

fn run(p: &Paths, cfg: &Map<String, Value>) -> Value {
    let t = now();
    let mut st = load(&p.state);

    // 1) Windows that reached their reset time -> PC notification (works offline).
    for (k, title, body) in WINDOWS {
        let prev = st.get("last").and_then(|l| l.get(k)).cloned().unwrap_or(json!({}));
        let Some(r) = prev["r"].as_i64() else { continue };
        if t < r as f64 {
            continue;
        }
        if obj(&mut st, "notified").get(k).and_then(|v| v.as_i64()) != Some(r) {
            obj(&mut st, "notified").insert(k.into(), json!(r));
            if prev["u"].as_f64().unwrap_or(0.0) > 0.0 {
                let msg = format!("{body} à {}.", hm(r));
                if cfg.get("desktop").and_then(|v| v.as_bool()).unwrap_or(true) {
                    desktop(title, &msg);
                }
                // could not be scheduled earlier: sent now
                if obj(&mut st, "pushed").get(k).and_then(|v| v.as_i64()) != Some(r) && phone(cfg, title, &msg, None) {
                    obj(&mut st, "pushed").insert(k.into(), json!(r));
                }
            }
        }
        obj(&mut st, "last").insert(k.into(), json!({"u": 0, "r": null}));
    }

    // 2) Current usage + 3) phone notification scheduled at the reset time.
    let rate_limited = st.get("rl").and_then(|v| v.as_bool()).unwrap_or(false);
    let tried = st.get("tried").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let err: String;
    if t - tried < if rate_limited { RATE_GAP } else { MIN_GAP } {
        err = st.get("err").and_then(|v| v.as_str()).unwrap_or("").to_string(); // too soon: last result
    } else {
        st.insert("tried".into(), json!(t as i64));
        st.insert("rl".into(), json!(false));
        err = match fetch(p) {
            Ok(data) => {
                for (k, title, body) in WINDOWS {
                    let w = &data[k];
                    let mut u = w["utilization"].as_f64().unwrap_or(0.0);
                    let mut r = w["resets_at"].as_str().and_then(ts);
                    if r.is_some_and(|r| r as f64 <= t) {
                        (u, r) = (0.0, None);
                    }
                    obj(&mut st, "last").insert(k.into(), json!({"u": u, "r": r}));
                    if let Some(r) = r {
                        let pushed = obj(&mut st, "pushed").get(k).and_then(|v| v.as_i64());
                        if u > 0.0 && pushed != Some(r) && r as f64 - t <= NTFY_MAX_DELAY && phone(cfg, title, &format!("{body} à {}.", hm(r)), Some(r)) {
                            obj(&mut st, "pushed").insert(k.into(), json!(r));
                        }
                    }
                }
                st.insert("t".into(), json!(t as i64));
                agenda(p, &mut st, t);
                String::new()
            }
            Err(Fail::RateLimited) => {
                st.insert("rl".into(), json!(true));
                "trop de requêtes, nouvel essai plus tard".into()
            }
            Err(Fail::Other(e)) => e.chars().take(120).collect(),
        };
        st.insert("err".into(), json!(err));
    }
    let _ = save(&p.state, &Value::Object(st.clone()));

    let last = st.get("last").cloned().unwrap_or(json!({}));
    let (f, s) = (&last["five_hour"], &last["seven_day"]);
    let pct = |w: &Value| w["u"].as_f64().map(|u| u.round_ties_even() as i64).unwrap_or(-1);
    let reset = |w: &Value| w["r"].as_i64().unwrap_or(0);
    json!({
        "ok": err.is_empty(), "e": err, "rl": st.get("rl").and_then(|v| v.as_bool()).unwrap_or(false),
        "t": st.get("t").and_then(|v| v.as_i64()).unwrap_or(0),
        "s": pct(f), "sr": reset(f), "w": pct(s), "wr": reset(s),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let p = paths();
    let cfg = load(&p.conf);
    if args.iter().any(|a| a == "--test") {
        desktop("Claude : test", "Les notifications PC marchent.");
        let ok = phone(&cfg, "Claude : test", "Les notifications téléphone marchent.", None);
        println!("PC : envoyé | téléphone : {}", if ok { "envoyé" } else { "échec (topic ntfy manquant ou pas de réseau)" });
        return;
    }
    let _ = fs::create_dir_all(&p.sdir);
    if let Some(i) = args.iter().position(|a| a == "--agenda") {
        let file = args.get(i + 1).map(String::as_str);
        if file.is_some() || !p.google.exists() {
            if let Err(e) = connect(&p, file) {
                eprintln!("Agenda : {e}");
                std::process::exit(1);
            }
            println!("Agenda Google branché.");
        }
        // sync from the last known usage, without calling the usage API
        let _lk = lock(&p.sdir.join("run.lock"));
        let mut st = load(&p.state);
        if file.is_some() {
            obj(&mut st, "agenda").remove("off");
        }
        let log = agenda(&p, &mut st, now());
        let _ = save(&p.state, &Value::Object(st));
        println!("{}", if log.is_empty() { "Agenda : rien à changer.".to_string() } else { log.join("\n") });
        return;
    }
    let out = {
        let _lk = lock(&p.sdir.join("run.lock")); // one run at a time
        run(&p, &cfg)
    };
    if args.iter().any(|a| a == "--status") {
        let line = |name: &str, u: &Value, r: i64, day: bool| {
            let mut l = format!("{name} : {u} %");
            if r != 0 {
                l += &format!("  (réinit. {})", if day { quand(r) } else { hm(r) });
            }
            l
        };
        println!("{}", line("Session ", &out["s"], out["sr"].as_i64().unwrap_or(0), false));
        println!("{}", line("Semaine ", &out["w"], out["wr"].as_i64().unwrap_or(0), true));
        if let Some(e) = out["e"].as_str().filter(|e| !e.is_empty()) {
            println!("Erreur  : {e}");
        }
        if p.google.exists() {
            let a = Value::Object(load(&p.state))["agenda"].clone();
            let mut l: Vec<String> = WINDOWS
                .iter()
                .filter_map(|(k, _, _)| a[k]["r"].as_i64().map(|r| format!("{} {}", if *k == "five_hour" { "5 h" } else { "semaine" }, quand(r))))
                .collect();
            if a["off"] == true {
                l = vec!["débranché, relance claude-usage --agenda".into()];
            } else if l.is_empty() {
                l.push("branché, aucun événement à venir".into());
            }
            if let Some(e) = a["err"].as_str() {
                l.push(format!("erreur : {e}"));
            }
            println!("Agenda   : {}", l.join(" | "));
        }
    } else {
        println!("{out}");
    }
}

#[cfg(test)]
mod tests {
    // reference values computed with Python's datetime.fromisoformat
    #[test]
    fn iso_dates() {
        assert_eq!(super::ts("2026-11-09T14:00:00.123456+00:00"), Some(1794232800));
        assert_eq!(super::ts("2026-11-12T07:59:31.9+01:00"), Some(1794466800));
        assert_eq!(super::ts("2026-03-01T00:00:29-05:30"), Some(1772343000));
        assert_eq!(super::ts("2027-02-28T23:59:30+00:00"), Some(1803859200));
        assert_eq!(super::ts("2026-11-09T13:59:30.000001+00:00"), Some(1794232800));
    }

    #[test]
    fn encodings() {
        assert_eq!(super::b64url(b"\xfb\xff"), "-_8");
        assert_eq!(super::b64url(b"Man"), "TWFu");
        assert_eq!(super::b64url(b"Ma"), "TWE");
        assert_eq!(super::dec("4%2F0Ab%2b-x+y%"), "4/0Ab+-x y%");
        assert_eq!(super::enc("a b/é"), "a%20b%2F%C3%A9");
        assert_eq!(super::utc(1795532400), "2026-11-24T15:00:00Z");
        let v = super::sha256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap();
        assert_eq!(super::b64url(&v), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"); // RFC 7636 appendix B
    }
}
