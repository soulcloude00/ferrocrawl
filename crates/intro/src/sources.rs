//! Relationship sources besides LinkedIn. Real interaction beats a LinkedIn click: someone you've
//! emailed twenty times or met five times is a far better introducer than a connection request
//! from 2019.
//!
//! - **Email** (`.mbox`, e.g. Gmail via Google Takeout): people you write to, and people who
//!   share threads with you, which means they likely know each other.
//! - **Calendar** (`.ics`, Google/Outlook/Apple export): people you've met, and co-attendees.
//! - **Contacts** (`.vcf`, phone/Google/iCloud export): people you saved.
//! - **GitHub**: contributors to the same repositories, members of an organization.
//!
//! Work email domains become organizations (`jane@sequoiacap.com` → works at Sequoia Capital, matched by
//! domain to anything harvested from sequoiacap.com).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::{EdgeKind, Graph, PersonIn, Tx, names};

#[derive(Clone, Debug, Default, Serialize)]
pub struct SourceReport {
    pub kind: String,
    pub items: usize,
    pub people: usize,
    pub new_edges: usize,
    pub skipped: usize,
    pub note: Option<String>,
}

impl SourceReport {
    pub fn line(&self) -> String {
        format!(
            "{}: {} items, {} people, {} new links{}{}",
            self.kind,
            self.items,
            self.people,
            self.new_edges,
            if self.skipped > 0 { format!(", {} skipped", self.skipped) } else { String::new() },
            self.note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default()
        )
    }
}

/// Imports a file by its type: `.csv` (LinkedIn or contacts), `.vcf`, `.mbox`/`.mbx`, `.ics`.
pub fn import_any(graph: &Graph, path: &Path, owner: Option<&str>, me_emails: &[String]) -> Result<SourceReport> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "csv" => {
            let r = crate::linkedin::import_file(graph, path, owner)?;
            Ok(SourceReport { kind: "contacts CSV".into(), items: r.rows, people: r.rows - r.skipped, new_edges: r.new_connections, skipped: r.skipped, note: None })
        }
        "vcf" | "vcard" => vcard(graph, &std::fs::read_to_string(path)?, owner),
        "ics" | "ical" => ics(graph, &std::fs::read_to_string(path)?, owner, me_emails),
        "mbox" | "mbx" | "" => mbox(graph, path, owner, me_emails),
        _ => bail!("don't know how to import .{ext}: use .csv (LinkedIn/contacts), .vcf, .ics or .mbox"),
    }
}

// ------------------------------------------------------------------ addresses

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Addr {
    email: String,
    name: Option<String>,
}

const FREEMAIL: &[&str] = &[
    "gmail.com", "googlemail.com", "yahoo.com", "yahoo.co.in", "yahoo.co.uk", "hotmail.com", "outlook.com", "live.com", "msn.com",
    "icloud.com", "me.com", "mac.com", "aol.com", "proton.me", "protonmail.com", "pm.me", "gmx.com", "gmx.de", "yandex.com", "yandex.ru",
    "mail.ru", "qq.com", "163.com", "126.com", "zoho.com", "fastmail.com", "hey.com", "rediffmail.com", "web.de", "tutanota.com",
];

const ROLE: &[&str] = &[
    "noreply", "no-reply", "no_reply", "donotreply", "do-not-reply", "notifications", "notification", "notify", "mailer-daemon", "postmaster",
    "bounce", "bounces", "newsletter", "news", "updates", "alerts", "alert", "billing", "invoice", "invoices", "receipts", "support", "help",
    "info", "hello", "team", "admin", "sales", "marketing", "jobs", "careers", "calendar-notification", "reply", "digest", "store", "orders",
];

/// A mailbox a person reads, not a robot or a shared inbox.
fn is_personal(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else { return false };
    let l = local.to_lowercase();
    !(ROLE.iter().any(|r| l == *r || l.starts_with(&format!("{r}+")) || l.starts_with(&format!("{r}-")) || l.starts_with(&format!("{r}."))))
        && !l.contains("noreply")
        && !l.contains("no-reply")
        && !domain.contains("calendar.google.com")
        && !domain.starts_with("bounce")
        && !domain.contains("mailchimp")
        && !domain.contains("sendgrid")
        && local.len() < 40
}

/// The organization an email domain belongs to, if it's not a personal mailbox provider.
fn domain_org(email: &str) -> Option<(String, String)> {
    let domain = email.rsplit_once('@')?.1.to_lowercase();
    if FREEMAIL.contains(&domain.as_str()) || !domain.contains('.') {
        return None;
    }
    // mail.acme.co.uk → acme.co.uk; the brand is the label before the public suffix.
    let parts: Vec<&str> = domain.split('.').collect();
    let two_level = parts.len() >= 3 && ["co", "com", "ac", "org", "net", "edu", "gov"].contains(&parts[parts.len() - 2]) && parts[parts.len() - 1].len() == 2;
    let keep = if two_level { 3 } else { 2 };
    let base = parts[parts.len().saturating_sub(keep)..].join(".");
    let label = base.split('.').next().unwrap_or(&base);
    let mut c = label.chars();
    let name = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str())?;
    Some((name, format!("https://{base}/")))
}

/// "jane.doe@x.com" → "Jane Doe" when the address looks like first.last.
fn name_from_email(email: &str) -> Option<String> {
    let local = email.split('@').next()?;
    let parts: Vec<&str> = local.split(['.', '_']).filter(|p| p.len() > 1 && p.chars().all(char::is_alphabetic)).collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    Some(parts.iter().map(|p| {
        let mut c = p.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" "))
}

fn b64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let (mut out, mut buf, mut bits) = (Vec::new(), 0u32, 0);
    for v in s.bytes().filter_map(val) {
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

/// Decodes RFC 2047 words: `=?UTF-8?B?SsO2cmc=?=` → "Jörg".
fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("=?") {
        let Some(end) = rest[start + 2..].find("?=").map(|e| e + start + 2) else { break };
        let inner = &rest[start + 2..end];
        let mut it = inner.splitn(3, '?');
        let (Some(_charset), Some(enc), Some(data)) = (it.next(), it.next(), it.next()) else { break };
        let between = &rest[..start];
        if !between.trim().is_empty() {
            out.push_str(between);
        }
        let bytes = if enc.eq_ignore_ascii_case("b") {
            b64(data)
        } else {
            let mut v = Vec::new();
            let b = data.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'_' => v.push(b' '),
                    b'=' if i + 2 < b.len() => {
                        if let Ok(x) = u8::from_str_radix(&data[i + 1..i + 3], 16) {
                            v.push(x);
                        }
                        i += 2;
                    }
                    c => v.push(c),
                }
                i += 1;
            }
            v
        };
        out.push_str(&String::from_utf8_lossy(&bytes));
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

/// `"Doe, Jane" <jane@x.com>, bob@y.com` → two addresses.
fn parse_addrs(v: &str) -> Vec<Addr> {
    let mut parts = Vec::new();
    let (mut cur, mut quoted, mut angle) = (String::new(), false, false);
    for c in v.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' => angle = true,
            '>' => angle = false,
            ',' | ';' if !quoted && !angle => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
        .into_iter()
        .filter_map(|p| {
            let p = p.trim();
            let (name, email) = match (p.rfind('<'), p.rfind('>')) {
                (Some(a), Some(b)) if b > a => (Some(p[..a].trim().trim_matches('"').trim().to_string()), p[a + 1..b].trim().to_string()),
                _ => (None, p.trim_matches(|c| c == '<' || c == '>').to_string()),
            };
            let email = email.to_lowercase();
            if !email.contains('@') || email.contains(' ') {
                return None;
            }
            let name = name.map(|n| decode_words(&n)).map(|n| {
                // "Doe, Jane" → "Jane Doe"
                match n.split_once(", ") {
                    Some((last, first)) if !first.contains(' ') && !last.contains(' ') => format!("{first} {last}"),
                    _ => n,
                }
            });
            let name = name.filter(|n| !n.is_empty() && !n.contains('@') && n.len() < 60);
            Some(Addr { email, name })
        })
        .collect()
}

fn person_for(t: &Tx, a: &Addr, source: &str) -> Result<Option<i64>> {
    let Some(name) = a.name.clone().or_else(|| name_from_email(&a.email)) else { return Ok(None) };
    if names::person_key(&name).split(' ').count() < 2 && !names::looks_like_name(&name) {
        // A single word ("jane") would merge with every Jane; keep the address as the name.
        return Ok(Some(t.person(&PersonIn { name: format!("{name} <{}>", a.email), email: Some(a.email.clone()), ..Default::default() })?));
    }
    let org = domain_org(&a.email);
    let id = t.person(&PersonIn { name, email: Some(a.email.clone()), org: org.as_ref().map(|o| o.0.clone()), ..Default::default() })?;
    if let Some((oname, home)) = org {
        let o = t.org_at(&oname, Some(&home))?;
        t.link(id, o, EdgeKind::WorksAt, Some(0.5), source, Some(&format!("email domain {}", a.email.rsplit('@').next().unwrap_or(""))))?;
    }
    Ok(Some(id))
}

/// Strength from how many times two people interacted: 1 → base, many → approaching `top`.
fn by_count(n: usize, base: f64, top: f64, scale: f64) -> f64 {
    base + (top - base) * (1.0 - (-(n as f64) / scale).exp())
}

/// The addresses that are you: given, or the one that appears most (as sender, or attendee).
fn infer_me(counts: &HashMap<String, usize>, given: &[String]) -> HashSet<String> {
    let mut me: HashSet<String> = given.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
    if me.is_empty()
        && let Some((e, _)) = counts.iter().max_by_key(|(_, n)| **n)
    {
        me.insert(e.clone());
    }
    me
}

// ------------------------------------------------------------------ email

struct Mail {
    from: Option<Addr>,
    to: Vec<Addr>,
    date: Option<String>,
}

fn read_mbox(path: &Path) -> Result<Vec<Mail>> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut rd = BufReader::with_capacity(1 << 20, f);
    let mut mails = Vec::new();
    let mut buf = Vec::new();
    let (mut in_headers, mut prev_blank) = (false, true);
    let mut headers: Vec<String> = Vec::new();
    let finish = |headers: &mut Vec<String>, mails: &mut Vec<Mail>| {
        if headers.is_empty() {
            return;
        }
        let mut m = Mail { from: None, to: Vec::new(), date: None };
        for h in headers.drain(..) {
            let Some((k, v)) = h.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "from" => m.from = parse_addrs(v).into_iter().next(),
                "to" | "cc" => m.to.extend(parse_addrs(v)),
                "date" => m.date = Some(v.trim().to_string()),
                _ => {}
            }
        }
        mails.push(m);
    };
    loop {
        buf.clear();
        if rd.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\r', '\n']);
        if prev_blank && line.starts_with("From ") {
            finish(&mut headers, &mut mails);
            in_headers = true;
            prev_blank = false;
            continue;
        }
        prev_blank = line.is_empty();
        if !in_headers {
            continue;
        }
        if line.is_empty() {
            in_headers = false;
            finish(&mut headers, &mut mails);
        } else if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else {
            headers.push(line.to_string());
        }
    }
    finish(&mut headers, &mut mails);
    Ok(mails)
}

/// Email: you → people you've written to, weighted by how much you've emailed; people who share
/// small threads with you → each other.
pub fn mbox(graph: &Graph, path: &Path, owner: Option<&str>, me_emails: &[String]) -> Result<SourceReport> {
    let mails = read_mbox(path)?;
    // You're on (nearly) every message, as sender or recipient; a newsletter is only ever a sender.
    let mut senders: HashMap<String, usize> = HashMap::new();
    for m in &mails {
        let mut on: HashSet<&str> = m.to.iter().map(|a| a.email.as_str()).collect();
        if let Some(f) = &m.from {
            on.insert(&f.email);
        }
        for e in on {
            *senders.entry(e.to_string()).or_default() += 1;
        }
    }
    let me = infer_me(&senders, me_emails);
    let mut sent: HashMap<Addr, usize> = HashMap::new();
    let mut recv: HashMap<String, usize> = HashMap::new();
    let mut last: HashMap<String, String> = HashMap::new();
    let mut pairs: HashMap<(String, String), usize> = HashMap::new();
    let mut known: HashMap<String, Addr> = HashMap::new();
    for m in &mails {
        let Some(from) = &m.from else { continue };
        let everyone: Vec<&Addr> = std::iter::once(from).chain(&m.to).filter(|a| !me.contains(&a.email) && is_personal(&a.email)).collect();
        let mine = me.contains(&from.email);
        if !mine && !m.to.iter().any(|a| me.contains(&a.email)) {
            continue; // not your conversation (a list you're BCC'd on)
        }
        for a in &everyone {
            let e = known.entry(a.email.clone()).or_insert_with(|| (*a).clone());
            if e.name.is_none() {
                e.name = a.name.clone();
            }
            if let Some(d) = &m.date {
                last.insert(a.email.clone(), d.clone());
            }
        }
        if mine {
            for a in m.to.iter().filter(|a| !me.contains(&a.email) && is_personal(&a.email)).take(15) {
                *sent.entry(Addr { email: a.email.clone(), name: None }).or_default() += 1;
            }
        } else if is_personal(&from.email) {
            *recv.entry(from.email.clone()).or_default() += 1;
        }
        // A small thread: its people probably know each other.
        if everyone.len() >= 2 && everyone.len() <= 8 {
            for i in 0..everyone.len() {
                for j in i + 1..everyone.len() {
                    let (a, b) = (&everyone[i].email, &everyone[j].email);
                    let k = if a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                    *pairs.entry(k).or_default() += 1;
                }
            }
        }
    }
    let mut rep = SourceReport { kind: "email".into(), items: mails.len(), ..Default::default() };
    rep.note = Some(format!("you: {}", me.iter().cloned().collect::<Vec<_>>().join(", ")));
    let source = format!("email:{}", path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
    graph.batch(|t| {
        let owner_id = match owner {
            Some(o) => t.person(&PersonIn { name: o.into(), ..Default::default() })?,
            None => t.me()?,
        };
        let mut ids: HashMap<String, i64> = HashMap::new();
        // Only people you've written to: received-only mail is mostly people you don't know.
        for (a, n_sent) in &sent {
            let addr = known.get(&a.email).cloned().unwrap_or_else(|| a.clone());
            let Some(id) = person_for(t, &addr, &source)? else {
                rep.skipped += 1;
                continue;
            };
            ids.insert(a.email.clone(), id);
            rep.people += 1;
            let n_recv = recv.get(&a.email).copied().unwrap_or(0);
            let two_way = if n_recv > 0 { 0.08 } else { 0.0 };
            let s = (by_count(n_sent + n_recv, 0.45, 0.88, 8.0) + two_way).min(0.95);
            let ev = format!("{n_sent} sent, {n_recv} received{}", last.get(&a.email).map(|d| format!(", last {d}")).unwrap_or_default());
            rep.new_edges += t.link(owner_id, id, EdgeKind::Emailed, Some(s), &source, Some(&ev))? as usize;
        }
        // People on the same small threads, at least twice, and at least one of them someone you write to.
        for ((a, b), n) in &pairs {
            if *n < 2 || !(ids.contains_key(a) || ids.contains_key(b)) {
                continue;
            }
            let mut id_of = |e: &String| -> Result<Option<i64>> {
                if let Some(id) = ids.get(e) {
                    return Ok(Some(*id));
                }
                let Some(addr) = known.get(e) else { return Ok(None) };
                let id = person_for(t, addr, &source)?;
                if let Some(id) = id {
                    ids.insert(e.clone(), id);
                }
                Ok(id)
            };
            let (Some(x), Some(y)) = (id_of(a)?, id_of(b)?) else { continue };
            rep.new_edges += t.link(x, y, EdgeKind::Emailed, Some(by_count(*n, 0.35, 0.7, 6.0)), &source, Some(&format!("on {n} email threads together")))? as usize;
        }
        Ok(())
    })?;
    Ok(rep)
}

// ------------------------------------------------------------------ calendar

/// RFC 5545 line unfolding: a line starting with a space continues the one before.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            if let Some(l) = out.last_mut() {
                l.push_str(&line[1..]);
            }
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// `ATTENDEE;CN="Jane Doe";ROLE=REQ-PARTICIPANT:mailto:jane@x.com` → Addr.
fn ics_addr(line: &str) -> Option<Addr> {
    let (params, value) = line.split_once(":mailto:").or_else(|| line.split_once(":MAILTO:"))?;
    let name = params.split(';').find_map(|p| p.strip_prefix("CN=")).map(|n| n.trim_matches('"').trim().to_string()).filter(|n| !n.contains('@') && !n.is_empty());
    let email = value.trim().to_lowercase();
    email.contains('@').then_some(Addr { email, name })
}

/// Calendar: you → people you've met (more meetings, stronger); co-attendees of small meetings → each other.
pub fn ics(graph: &Graph, text: &str, owner: Option<&str>, me_emails: &[String]) -> Result<SourceReport> {
    let mut events: Vec<(Vec<Addr>, Option<String>)> = Vec::new();
    let mut cur: Option<(Vec<Addr>, Option<String>)> = None;
    let mut cal_owner: Vec<String> = me_emails.to_vec();
    for line in unfold(text) {
        let upper = line.to_ascii_uppercase();
        // Google names an exported calendar after its owner's address.
        if cur.is_none() && upper.starts_with("X-WR-CALNAME:") && line.contains('@') && cal_owner.is_empty() {
            cal_owner.push(line[13..].trim().to_lowercase());
        }
        if upper == "BEGIN:VEVENT" {
            cur = Some((Vec::new(), None));
        } else if upper == "END:VEVENT" {
            if let Some(e) = cur.take() {
                events.push(e);
            }
        } else if let Some((people, date)) = &mut cur {
            if upper.starts_with("ATTENDEE") || upper.starts_with("ORGANIZER") {
                if let Some(a) = ics_addr(&line)
                    && !people.iter().any(|p: &Addr| p.email == a.email)
                {
                    people.push(a);
                }
            } else if upper.starts_with("DTSTART") {
                let v = line.rsplit(':').next().unwrap_or("");
                if v.len() >= 8 {
                    *date = Some(format!("{}-{}-{}", &v[0..4], &v[4..6], &v[6..8]));
                }
            }
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (p, _) in &events {
        for a in p {
            *seen.entry(a.email.clone()).or_default() += 1;
        }
    }
    let me = infer_me(&seen, &cal_owner);
    let mut met: HashMap<String, (Addr, usize, Option<String>)> = HashMap::new();
    let mut pairs: HashMap<(String, String), usize> = HashMap::new();
    let mut skipped = 0;
    for (people, date) in &events {
        let others: Vec<&Addr> = people.iter().filter(|a| !me.contains(&a.email) && is_personal(&a.email)).collect();
        // All-hands and webinars say nothing about knowing someone.
        if others.is_empty() || people.len() > 15 || !people.iter().any(|a| me.contains(&a.email)) {
            skipped += 1;
            continue;
        }
        for a in &others {
            let e = met.entry(a.email.clone()).or_insert_with(|| ((*a).clone(), 0, None));
            e.1 += 1;
            if date.as_ref() > e.2.as_ref() {
                e.2 = date.clone();
            }
        }
        if others.len() <= 8 {
            for i in 0..others.len() {
                for j in i + 1..others.len() {
                    let (a, b) = (&others[i].email, &others[j].email);
                    *pairs.entry(if a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) }).or_default() += 1;
                }
            }
        }
    }
    let mut rep = SourceReport { kind: "calendar".into(), items: events.len(), skipped, note: Some(format!("you: {}", me.iter().cloned().collect::<Vec<_>>().join(", "))), ..Default::default() };
    graph.batch(|t| {
        let owner_id = match owner {
            Some(o) => t.person(&PersonIn { name: o.into(), ..Default::default() })?,
            None => t.me()?,
        };
        let mut ids = HashMap::new();
        for (email, (addr, n, last)) in &met {
            let Some(id) = person_for(t, addr, "calendar")? else { continue };
            ids.insert(email.clone(), id);
            rep.people += 1;
            let ev = format!("met {n} time{}{}", if *n == 1 { "" } else { "s" }, last.as_ref().map(|d| format!(", last {d}")).unwrap_or_default());
            rep.new_edges += t.link(owner_id, id, EdgeKind::Met, Some(by_count(*n, 0.55, 0.92, 4.0)), "calendar", Some(&ev))? as usize;
        }
        for ((a, b), n) in &pairs {
            let (Some(x), Some(y)) = (ids.get(a), ids.get(b)) else { continue };
            rep.new_edges += t.link(*x, *y, EdgeKind::Met, Some(by_count(*n, 0.4, 0.75, 4.0)), "calendar", Some(&format!("in {n} meeting{} together", if *n == 1 { "" } else { "s" })))? as usize;
        }
        Ok(())
    })?;
    Ok(rep)
}

// ------------------------------------------------------------------ contacts

/// vCard (phone, Google or iCloud contacts): everyone saved is someone you know.
pub fn vcard(graph: &Graph, text: &str, owner: Option<&str>) -> Result<SourceReport> {
    let mut cards: Vec<HashMap<String, String>> = Vec::new();
    let mut cur: Option<HashMap<String, String>> = None;
    for line in unfold(text) {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VCARD" {
            cur = Some(HashMap::new());
        } else if upper == "END:VCARD" {
            if let Some(c) = cur.take() {
                cards.push(c);
            }
        } else if let Some(c) = &mut cur
            && let Some((k, v)) = line.split_once(':')
        {
            // "item1.EMAIL;TYPE=INTERNET" → "EMAIL"
            let key = k.split(';').next().unwrap_or(k).rsplit('.').next().unwrap_or(k).to_ascii_uppercase();
            let v = v.replace("\\,", ",").replace("\\;", ";").replace("\\n", " ");
            let slot = if key == "URL" || key == "X-SOCIALPROFILE" { if v.contains("linkedin.com") { "LINKEDIN".to_string() } else { key } } else { key };
            c.entry(slot).or_insert(v);
        }
    }
    let mut rep = SourceReport { kind: "contacts".into(), items: cards.len(), ..Default::default() };
    graph.batch(|t| {
        let owner_id = match owner {
            Some(o) => t.person(&PersonIn { name: o.into(), ..Default::default() })?,
            None => t.me()?,
        };
        for c in &cards {
            let name = c.get("FN").cloned().filter(|n| !n.trim().is_empty()).or_else(|| {
                c.get("N").map(|n| {
                    let p: Vec<&str> = n.split(';').collect();
                    format!("{} {}", p.get(1).unwrap_or(&""), p.first().unwrap_or(&"")).trim().to_string()
                })
            });
            let Some(name) = name.filter(|n| names::person_key(n).contains(' ')) else {
                rep.skipped += 1;
                continue;
            };
            let email = c.get("EMAIL").map(|e| e.trim().to_lowercase());
            let org = c.get("ORG").map(|o| o.split(';').next().unwrap_or(o).trim().to_string()).filter(|o| !o.is_empty());
            let org = org.or_else(|| email.as_deref().and_then(domain_org).map(|o| o.0));
            let id = t.person(&PersonIn { name, title: c.get("TITLE").cloned(), org: org.clone(), email, linkedin: c.get("LINKEDIN").cloned(), url: None })?;
            rep.people += 1;
            rep.new_edges += t.link(owner_id, id, EdgeKind::Contact, None, "contacts", Some("saved in contacts"))? as usize;
            if let Some(o) = org {
                t.works_at(id, &o, EdgeKind::WorksAt, "contacts", Some(&format!("{o} (contact card)")))?;
            }
        }
        Ok(())
    })?;
    Ok(rep)
}

// ------------------------------------------------------------------ GitHub

async fn gh(client: &reqwest::Client, path: &str) -> Result<Value> {
    let mut req = client
        .get(format!("https://api.github.com/{path}"))
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "snarf")
        .header("x-github-api-version", "2022-11-28");
    if let Ok(tok) = std::env::var("GITHUB_TOKEN") {
        req = req.header("authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub rate limit hit: set GITHUB_TOKEN for 5,000 requests an hour (60 without)");
    }
    if !status.is_success() {
        bail!("GitHub {status} for {path}");
    }
    Ok(serde_json::from_str(&body)?)
}

struct GhUser {
    login: String,
    name: Option<String>,
    company: Option<String>,
    email: Option<String>,
    url: String,
}

async fn gh_user(client: &reqwest::Client, login: &str) -> Result<GhUser> {
    let u = gh(client, &format!("users/{login}")).await?;
    let s = |k: &str| u.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    Ok(GhUser {
        login: login.to_string(),
        name: s("name"),
        // "@acme" → "acme"; "Acme, Inc." stays.
        company: s("company").map(|c| c.trim_start_matches('@').split([',', '|']).next().unwrap_or("").trim().to_string()).filter(|c| !c.is_empty()),
        email: s("email"),
        url: s("html_url").unwrap_or_else(|| format!("https://github.com/{login}")),
    })
}

fn gh_person(t: &Tx, u: &GhUser) -> Result<i64> {
    // A real name merges with the same person elsewhere; a bare login stays its own node.
    let name = u.name.clone().filter(|n| names::person_key(n).contains(' ')).unwrap_or_else(|| format!("{} (GitHub)", u.login));
    let id = t.person(&PersonIn { name, org: u.company.clone(), email: u.email.clone(), url: Some(u.url.clone()), ..Default::default() })?;
    if let Some(c) = &u.company {
        t.works_at(id, c, EdgeKind::WorksAt, "github", Some(&format!("GitHub profile: {c}")))?;
    }
    Ok(id)
}

/// Reads GitHub: `owner/repo` (its top contributors), an organization (its public members), or a
/// user (contributors to their recent repositories). With `as_me`, the user is you.
pub async fn github(client: &reqwest::Client, graph: &Graph, target: &str, as_me: bool) -> Result<SourceReport> {
    let target = target.trim().trim_start_matches("https://github.com/").trim_start_matches('@').trim_end_matches('/');
    let mut rep = SourceReport { kind: "GitHub".into(), ..Default::default() };
    let mut repos: Vec<String> = Vec::new();
    let mut center: Option<GhUser> = None;
    if target.contains('/') {
        repos.push(target.to_string());
    } else {
        let profile = gh(client, &format!("users/{target}")).await?;
        if profile.get("type").and_then(Value::as_str) == Some("Organization") {
            let org_name = gh(client, &format!("orgs/{target}")).await.ok().and_then(|o| o.get("name").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| target.to_string());
            let members = gh(client, &format!("orgs/{target}/members?per_page=100")).await?;
            let logins: Vec<String> = members.as_array().into_iter().flatten().filter_map(|m| m.get("login").and_then(Value::as_str).map(str::to_string)).collect();
            let mut users = Vec::new();
            for l in logins.iter().take(60) {
                if let Ok(u) = gh_user(client, l).await {
                    users.push(u);
                }
            }
            rep.items = logins.len();
            graph.batch(|t| {
                let o = t.org_at(&org_name, Some(&format!("https://github.com/{target}/")))?;
                for u in &users {
                    let id = gh_person(t, u)?;
                    rep.people += 1;
                    rep.new_edges += t.link(id, o, EdgeKind::WorksAt, Some(0.55), "github", Some(&format!("public member of github.com/{target}")))? as usize;
                }
                Ok(())
            })?;
            return Ok(rep);
        }
        center = Some(gh_user(client, target).await?);
        let list = gh(client, &format!("users/{target}/repos?sort=pushed&per_page=30")).await?;
        repos = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| !r.get("fork").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|r| r.get("full_name").and_then(Value::as_str).map(str::to_string))
            .take(6)
            .collect();
    }
    // Contributors of each repo, top first; their profiles give names and employers.
    let mut by_repo: Vec<(String, Vec<(GhUser, u64)>)> = Vec::new();
    let mut cache: HashMap<String, GhUser> = HashMap::new();
    for r in &repos {
        let list = gh(client, &format!("repos/{r}/contributors?per_page=30")).await?;
        let mut people = Vec::new();
        for c in list.as_array().into_iter().flatten().take(if target.contains('/') { 30 } else { 12 }) {
            let (Some(login), n) = (c.get("login").and_then(Value::as_str), c.get("contributions").and_then(Value::as_u64).unwrap_or(0)) else { continue };
            if login.ends_with("[bot]") || c.get("type").and_then(Value::as_str) == Some("Bot") {
                continue;
            }
            let u = match cache.remove(login) {
                Some(u) => u,
                None => match gh_user(client, login).await {
                    Ok(u) => u,
                    Err(e) if e.to_string().contains("rate limit") => return Err(e),
                    Err(_) => continue,
                },
            };
            people.push((u, n));
        }
        for (u, _) in &people {
            cache.insert(u.login.clone(), GhUser { login: u.login.clone(), name: u.name.clone(), company: u.company.clone(), email: u.email.clone(), url: u.url.clone() });
        }
        rep.items += 1;
        by_repo.push((r.clone(), people));
    }
    graph.batch(|t| {
        let me_id = if as_me { Some(t.me()?) } else { None };
        let center_id = match (&center, me_id) {
            (_, Some(m)) => Some(m),
            (Some(c), None) => Some(gh_person(t, c)?),
            _ => None,
        };
        let mut counted = HashSet::new();
        for (r, people) in &by_repo {
            let repo = t.org_at(&format!("github.com/{r}"), None)?;
            t.0.execute("UPDATE nodes SET url = COALESCE(url, ?2) WHERE id = ?1", rusqlite::params![repo, format!("https://github.com/{r}")])?;
            if let Some(c) = center_id {
                rep.new_edges += t.link(c, repo, EdgeKind::Contributor, Some(0.75), "github", Some(&format!("owns github.com/{r}")))? as usize;
            }
            for (u, n) in people {
                if center.as_ref().is_some_and(|c| c.login == u.login) {
                    continue;
                }
                let id = gh_person(t, u)?;
                if counted.insert(id) {
                    rep.people += 1;
                }
                // Many commits = a core maintainer, who knows the others.
                let s = by_count(*n as usize, 0.4, 0.8, 50.0);
                rep.new_edges += t.link(id, repo, EdgeKind::Contributor, Some(s), "github", Some(&format!("{n} commits to github.com/{r}")))? as usize;
            }
        }
        Ok(())
    })?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let a = parse_addrs(r#""Doe, Jane" <Jane.Doe@Acme.com>, bob@y.com, =?UTF-8?B?SsO2cmcgTcO8bGxlcg==?= <jorg@z.de>"#);
        assert_eq!(a[0], Addr { email: "jane.doe@acme.com".into(), name: Some("Jane Doe".into()) });
        assert_eq!(a[1].email, "bob@y.com");
        assert_eq!(a[2].name.as_deref(), Some("Jörg Müller"));
        assert_eq!(domain_org("x@mail.acme.co.uk"), Some(("Acme".into(), "https://acme.co.uk/".into())));
        assert_eq!(domain_org("x@gmail.com"), None);
        assert!(!is_personal("noreply@github.com") && !is_personal("notifications@x.io") && is_personal("jane@acme.com"));
        assert_eq!(name_from_email("jane.doe@x.com").as_deref(), Some("Jane Doe"));
    }

    #[test]
    fn email_calendar_contacts() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("intro-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let mbox = dir.join("mail.mbox");
        let msg = |from: &str, to: &str| format!("From x Mon Jan 1 00:00:00 2024\nFrom: {from}\nTo: {to}\nDate: Mon, 1 Jan 2024 10:00:00 +0000\nSubject: hi\n\nbody\nFrom here on, a body line\n\n");
        let me = "Me Myself <me@mine.dev>";
        let text = [
            msg(me, "Guy Alpha <guy@northwind.io>"),
            msg(me, "Guy Alpha <guy@northwind.io>, Pat Grady <pat@sequoiacap.com>"),
            msg(me, "Guy Alpha <guy@northwind.io>, Pat Grady <pat@sequoiacap.com>"),
            msg("Guy Alpha <guy@northwind.io>", "me@mine.dev"),
            msg("News <newsletter@spam.io>", "me@mine.dev"),
        ]
        .concat();
        std::fs::write(&mbox, text)?;
        let g = Graph::in_memory()?;
        let r = import_any(&g, &mbox, None, &[])?;
        assert_eq!(r.items, 5);
        assert_eq!(r.people, 2, "{r:?}");
        let p = g.paths("Pat Grady", 3)?;
        assert_eq!(p.paths[0].ask, None, "you emailed Pat directly");
        // Sequoia via the email domain, through Pat.
        assert!(g.paths("Sequoiacap", 3)?.paths.iter().any(|p| p.summary.contains("Pat Grady")));

        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART:20250302T100000Z\nORGANIZER;CN=Me:mailto:me@mine.dev\nATTENDEE;CN=\"Dana Fox\":mailto:dana@contoso.com\nATTENDEE;CN=Lee\n Kim:mailto:lee@contoso.com\nEND:VEVENT\nEND:VCALENDAR\n";
        let r = ics_test(&g, ics)?;
        assert_eq!((r.items, r.people), (1, 2));
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nFN:Riya Shah\nORG:Stripe;\nEMAIL;TYPE=INTERNET:riya@stripe.com\nEND:VCARD\nBEGIN:VCARD\nFN:Mom\nEND:VCARD\n";
        let r = vcard(&g, vcf, None)?;
        assert_eq!((r.people, r.skipped), (1, 1));
        assert!(g.paths("Stripe", 1)?.paths[0].summary.contains("Riya Shah"));
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }

    fn ics_test(g: &Graph, s: &str) -> Result<SourceReport> {
        ics(g, s, None, &["me@mine.dev".to_string()])
    }
}
