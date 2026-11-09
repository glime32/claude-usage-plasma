// claude-usage: Claude usage (5 h session + week) for the Plasma widget,
// plus PC and phone notifications when a limit resets.
//
// No daemon: the widget runs this from time to time (every 10 to 30 min).
// Rust port of the old Python script: same files, same JSON, a few ms of CPU.
//
//   claude-usage            -> one JSON line (read by the widget)
//   claude-usage --status   -> human summary
//   claude-usage --test     -> test notification on the PC and the phone

use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"; // Claude Code's
const UA: &str = "claude-usage-widget/2.0";
const NTFY_MAX_DELAY: f64 = (3 * 86400 - 900) as f64; // ntfy.sh schedules at most 3 days ahead
const MIN_GAP: f64 = 120.0; // at most one API call every 2 min
const RATE_GAP: f64 = 300.0; // after an HTTP 429, 5 min without calling the API

const WINDOWS: [(&str, &str, &str); 2] = [
    ("five_hour", "Claude : session réinitialisée", "Ta limite de session (5 h) est revenue à 0 %"),
    ("seven_day", "Claude : semaine réinitialisée", "Ta limite hebdomadaire est revenue à 0 %"),
];

struct Paths {
    cred: PathBuf,
    conf: PathBuf,
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
    Paths {
        cred: home.join(".claude/.credentials.json"),
        conf: xdg("XDG_CONFIG_HOME", ".config").join("claude-usage/config.json"),
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
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut cfg = format!("url = {}\nsilent\nmax-time = {timeout}\nwrite-out = \"\\n%{{http_code}}\"\n", quote(url));
    cfg += &format!("header = {}\n", quote(&format!("User-Agent: {UA}")));
    for (k, v) in headers {
        cfg += &format!("header = {}\n", quote(&format!("{k}: {v}")));
    }
    if let Some(d) = data {
        cfg += &format!("header = {}\n", quote("Content-Type: application/json"));
        cfg += &format!("data-binary = {}\n", quote(&d.to_string()));
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
    let out = {
        let _lk = lock(&p.sdir.join("run.lock")); // one run at a time
        run(&p, &cfg)
    };
    if args.iter().any(|a| a == "--status") {
        const DAYS: [&str; 7] = ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."];
        let line = |name: &str, u: &Value, r: i64, day: bool| {
            let mut l = format!("{name} : {u} %");
            if r != 0 {
                let tm = local(r);
                let when = if day {
                    format!("{} {:02}/{:02} {:02}:{:02}", DAYS[tm.tm_wday as usize], tm.tm_mday, tm.tm_mon + 1, tm.tm_hour, tm.tm_min)
                } else {
                    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
                };
                l += &format!("  (réinit. {when})");
            }
            l
        };
        println!("{}", line("Session ", &out["s"], out["sr"].as_i64().unwrap_or(0), false));
        println!("{}", line("Semaine ", &out["w"], out["wr"].as_i64().unwrap_or(0), true));
        if let Some(e) = out["e"].as_str().filter(|e| !e.is_empty()) {
            println!("Erreur  : {e}");
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
}
