//! Postfix clauses on a semantic call.
//!
//! A call-site clause wins over `DEFAULT CLAUSES` on the function, which
//! wins over the `semantic.*` GUCs. The reducer is the Reduce step. A
//! failed `UNTIL` starts another RetryWave. `CHECK` drops takes.
//! `FALLBACK` redispatches once. `BUDGET` aborts when the wave's cost
//! would pass the cap. Same-model takes with equal scores are reported
//! as identical. A live Jev run varied by about 0.03 across takes, so
//! identical was false. A second phrasing moved one row from about 0.67
//! to about 0.83, more than extra takes did.

use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq)]
pub struct Clauses {
    pub model: Option<String>,
    pub backend: Option<String>,
    pub fallback: Option<String>,
    pub takes: Option<u32>,
    pub across: Vec<String>,
    pub phrasings: Vec<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub pick: Option<Pick>,
    pub retry: Option<u32>,
    pub until: Option<Cond>,
    pub hint: Option<String>,
    pub check_input: Option<Cond>,
    pub check_output: Option<Cond>,
    pub cache: Option<String>,
    pub timeout_ms: Option<u64>,
    pub budget: Option<f64>,
    pub threshold: Option<f64>,
}

impl Default for Clauses {
    fn default() -> Self {
        Self {
            model: None,
            backend: None,
            fallback: None,
            takes: None,
            across: Vec::new(),
            phrasings: Vec::new(),
            temperature: None,
            max_tokens: None,
            pick: None,
            retry: None,
            until: None,
            hint: None,
            check_input: None,
            check_output: None,
            cache: None,
            timeout_ms: None,
            budget: None,
            threshold: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Vote,
    FirstValid,
    Judge,
    Consensus,
    MeanScore,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    ScoreGte(f64),
    ScoreLt(f64),
    LenGte(usize),
    NonEmpty,
}

#[derive(Clone, Debug)]
pub struct Take {
    pub model: String,
    pub score: f64,
    pub value: String,
    pub cost: f64,
    pub error: bool,
    pub judge: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub winner: Option<String>,
    pub score: Option<f64>,
    pub retries: u32,
    pub identical: bool,
    pub fallback: bool,
    pub error: Option<String>,
}

/// Session spend for `BUDGET`. Not a Postgres counter.
static SPENT: Mutex<f64> = Mutex::new(0.0);

type GucRead = fn(&str) -> Option<String>;
static GUC: Mutex<Option<GucRead>> = Mutex::new(None);

pub fn set_guc_reader(read: GucRead) {
    *GUC.lock().unwrap_or_else(|e| e.into_inner()) = Some(read);
}

fn guc_str(name: &str) -> Option<String> {
    let read = (*GUC.lock().unwrap_or_else(|e| e.into_inner()))?;
    let value = read(name)?;
    let value = value.trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

/// Unset call and function fields fall through to these `semantic.*` values.
pub fn guc_clauses() -> Clauses {
    let mut c = Clauses::default();
    if let Some(v) = guc_str("semantic.model") {
        c.model = Some(v);
    }
    if let Some(v) = guc_str("semantic.takes") {
        c.takes = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.temperature") {
        c.temperature = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.max_tokens") {
        c.max_tokens = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.timeout") {
        c.timeout_ms = v.parse::<f64>().ok().map(|secs| (secs * 1000.0) as u64);
    }
    if let Some(v) = guc_str("semantic.budget") {
        c.budget = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.retry") {
        c.retry = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.threshold") {
        c.threshold = v.parse().ok();
    }
    if let Some(v) = guc_str("semantic.pick") {
        c.pick = parse_tail(&format!("PICK BY {v}")).ok().and_then(|parsed| parsed.pick);
    }
    c
}

/// `semantic.rerank_model`. Empty means the caller should keep jev noul.
pub fn guc_rerank_model() -> Option<String> {
    guc_str("semantic.rerank_model")
}

pub fn reset_budget() {
    *SPENT.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
}

pub fn spent() -> f64 {
    *SPENT.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn add_spent(cost: f64) {
    *SPENT.lock().unwrap_or_else(|e| e.into_inner()) += cost;
}

/// Call-site fields win, then the function default, then the GUC.
pub fn merge(call: Clauses, function: &Clauses, guc: &Clauses) -> Clauses {
    Clauses {
        model: call.model.or_else(|| function.model.clone()).or_else(|| guc.model.clone()),
        backend: call.backend.or_else(|| function.backend.clone()).or_else(|| guc.backend.clone()),
        fallback: call.fallback.or_else(|| function.fallback.clone()).or_else(|| guc.fallback.clone()),
        takes: call.takes.or(function.takes).or(guc.takes),
        across: if call.across.is_empty() {
            if function.across.is_empty() { guc.across.clone() } else { function.across.clone() }
        } else {
            call.across
        },
        phrasings: if call.phrasings.is_empty() { function.phrasings.clone() } else { call.phrasings },
        temperature: call.temperature.or(function.temperature).or(guc.temperature),
        max_tokens: call.max_tokens.or(function.max_tokens).or(guc.max_tokens),
        pick: call.pick.or(function.pick).or(guc.pick),
        retry: call.retry.or(function.retry).or(guc.retry),
        until: call.until.clone().or_else(|| function.until.clone()).or_else(|| guc.until.clone()),
        hint: call.hint.or_else(|| function.hint.clone()).or_else(|| guc.hint.clone()),
        check_input: call.check_input.clone().or_else(|| function.check_input.clone()).or_else(|| guc.check_input.clone()),
        check_output: call.check_output.clone().or_else(|| function.check_output.clone()).or_else(|| guc.check_output.clone()),
        cache: call.cache.or_else(|| function.cache.clone()).or_else(|| guc.cache.clone()),
        timeout_ms: call.timeout_ms.or(function.timeout_ms).or(guc.timeout_ms),
        budget: call.budget.or(function.budget).or(guc.budget),
        threshold: call.threshold.or(function.threshold).or(guc.threshold),
    }
}

pub fn parse_tail(input: &str) -> Result<Clauses, String> {
    let mut p = Parser { s: input, i: 0 };
    let mut c = Clauses::default();
    p.skip();
    while p.i < p.s.len() {
        let word = p.word();
        if word.is_empty() || is_clause_terminator(&word) {
            break;
        }
        match word.as_str() {
            "using" => {
                p.expect_word("model")?;
                c.model = Some(p.string_or_word()?);
                if p.peek_word() == "on" {
                    p.word();
                    p.expect_word("backend")?;
                    c.backend = Some(p.string_or_word()?);
                }
                if p.peek_word() == "fallback" {
                    p.word();
                    c.fallback = Some(p.string_or_word()?);
                }
            }
            "with" => {
                while p.i < p.s.len() {
                    match p.peek_word().as_str() {
                        "takes" => {
                            p.word();
                            c.takes = Some(p.number()?.trunc() as u32);
                        }
                        "across" => {
                            p.word();
                            p.expect_word("models")?;
                            c.across = p.string_list()?;
                        }
                        "phrasings" => {
                            p.word();
                            c.phrasings = p.string_list()?;
                        }
                        "temperature" => {
                            p.word();
                            c.temperature = Some(p.number()?);
                        }
                        "max" => {
                            p.word();
                            p.expect_word("tokens")?;
                            c.max_tokens = Some(p.number()?.trunc() as u32);
                        }
                        _ => break,
                    }
                }
            }
            "pick" => {
                p.expect_word("by")?;
                c.pick = Some(match p.peek_word().as_str() {
                    "vote" => { p.word(); Pick::Vote }
                    "first" => { p.word(); p.expect_word("valid")?; Pick::FirstValid }
                    "judge" => { p.word(); Pick::Judge }
                    "consensus" => { p.word(); Pick::Consensus }
                    "mean" => { p.word(); p.expect_word("score")?; Pick::MeanScore }
                    other => return Err(format!("unknown PICK {other}")),
                });
            }
            "retry" => {
                c.retry = Some(p.number()?.trunc() as u32);
                if p.peek_word() == "until" {
                    p.word();
                    c.until = Some(scan_cond(&mut p)?);
                }
                if p.peek_word() == "hint" {
                    p.word();
                    c.hint = Some(p.string_or_word()?);
                }
            }
            "check" => {
                let which = p.word();
                let cond = scan_cond(&mut p)?;
                if which == "input" {
                    c.check_input = Some(cond);
                } else if which == "output" {
                    c.check_output = Some(cond);
                } else {
                    return Err(format!("CHECK {which} is not INPUT or OUTPUT"));
                }
            }
            "cache" => c.cache = Some("cache".into()),
            "no" => {
                p.expect_word("cache")?;
                c.cache = Some("off".into());
            }
            "refresh" => c.cache = Some("refresh".into()),
            "timeout" => c.timeout_ms = Some((p.number()? * 1000.0) as u64),
            "budget" => c.budget = Some(p.number()?),
            "threshold" => c.threshold = Some(p.number()?),
            // `REPORT` asks the rewriter for the JSON report. It is not a
            // hutch knob, so the parser accepts it and ignores it.
            "report" => {}
            other => return Err(format!("unknown clause {other}")),
        }
        p.skip();
    }
    Ok(c)
}

fn is_clause_terminator(word: &str) -> bool {
    matches!(
        word,
        "as" | "from" | "where" | "group" | "order" | "limit" | "offset" | "union" | "having"
            | "window" | "join" | "and" | "or" | "select" | "intersect" | "except"
    )
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip(&mut self) {
        let b = self.s.as_bytes();
        while self.i < b.len() && b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek_word(&mut self) -> String {
        let save = self.i;
        let word = self.word();
        self.i = save;
        word
    }

    fn word(&mut self) -> String {
        self.skip();
        let b = self.s.as_bytes();
        let start = self.i;
        while self.i < b.len() && (b[self.i].is_ascii_alphanumeric() || b[self.i] == b'_') {
            self.i += 1;
        }
        self.s[start..self.i].to_ascii_lowercase()
    }

    fn expect_word(&mut self, want: &str) -> Result<(), String> {
        let got = self.word();
        if got != want {
            return Err(format!("expected {want}, got {got}"));
        }
        Ok(())
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip();
        let b = self.s.as_bytes();
        let start = self.i;
        if self.i < b.len() && b[self.i] == b'-' {
            self.i += 1;
        }
        while self.i < b.len() && (b[self.i].is_ascii_digit() || b[self.i] == b'.') {
            self.i += 1;
        }
        self.s[start..self.i].parse().map_err(|_| format!("expected a number at {start}"))
    }

    fn string_or_word(&mut self) -> Result<String, String> {
        self.skip();
        if self.s[self.i..].starts_with('\'') {
            return self.sql_string();
        }
        let word = self.word();
        if word.is_empty() {
            return Err("expected a name".into());
        }
        Ok(word)
    }

    fn sql_string(&mut self) -> Result<String, String> {
        self.skip();
        let b = self.s.as_bytes();
        if self.i >= b.len() || b[self.i] != b'\'' {
            return Err("expected a string".into());
        }
        self.i += 1;
        let mut out = String::new();
        while self.i < b.len() {
            if b[self.i] == b'\'' {
                if self.i + 1 < b.len() && b[self.i + 1] == b'\'' {
                    out.push('\'');
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                return Ok(out);
            }
            out.push(b[self.i] as char);
            self.i += 1;
        }
        Err("unterminated string".into())
    }

    fn string_list(&mut self) -> Result<Vec<String>, String> {
        self.skip();
        let b = self.s.as_bytes();
        if self.i >= b.len() || b[self.i] != b'(' {
            return Err("expected a parenthesized list".into());
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            self.skip();
            if self.i < b.len() && b[self.i] == b')' {
                self.i += 1;
                break;
            }
            out.push(self.sql_string()?);
            self.skip();
            if self.i < b.len() && b[self.i] == b',' {
                self.i += 1;
            }
        }
        Ok(out)
    }

}

/// `>=` and `<` are not identifier words. Parse them before `word()`.
fn scan_cond(p: &mut Parser) -> Result<Cond, String> {
    p.skip();
    let word = p.peek_word();
    if word == "score" {
        p.word();
        p.skip();
        let rest = &p.s[p.i..];
        if let Some(stripped) = rest.strip_prefix(">=") {
            p.i += 2;
            let _ = stripped;
            return Ok(Cond::ScoreGte(p.number()?));
        }
        if rest.starts_with('<') {
            p.i += 1;
            return Ok(Cond::ScoreLt(p.number()?));
        }
        return Err("score comparison must be >= or <".into());
    }
    if word == "len" {
        p.word();
        p.skip();
        if p.s[p.i..].starts_with(">=") {
            p.i += 2;
        }
        return Ok(Cond::LenGte(p.number()? as usize));
    }
    if word == "nonempty" {
        p.word();
        return Ok(Cond::NonEmpty);
    }
    Err(format!("unknown condition {word}"))
}

pub fn run_scripted(clauses: &Clauses, input_len: usize, waves: &[Vec<Take>]) -> Report {
    if let Some(cond) = &clauses.check_input {
        if !match cond {
            Cond::LenGte(n) => input_len >= *n,
            Cond::NonEmpty => input_len > 0,
            Cond::ScoreGte(_) | Cond::ScoreLt(_) => true,
        } {
            return Report {
                winner: None,
                score: None,
                retries: 0,
                identical: false,
                fallback: false,
                error: Some("check-input".into()),
            };
        }
    }
    let mut retries = 0u32;
    let mut fallback = false;
    let limit = clauses.retry.unwrap_or(0);
    let threshold = clauses.threshold.unwrap_or(0.5);
    for (i, wave) in waves.iter().enumerate() {
        if i > 0 && !fallback {
            retries += 1;
        }
        let live: Vec<&Take> = wave
            .iter()
            .filter(|t| !t.error && passes_output(clauses, t))
            .collect();
        if live.is_empty() {
            if clauses.fallback.is_some() && !fallback && i + 1 < waves.len() {
                fallback = true;
                continue;
            }
            return Report {
                winner: None,
                score: None,
                retries,
                identical: false,
                fallback,
                error: Some("no valid take".into()),
            };
        }
        let identical = identical_takes(&live);
        let (winner, score) = reduce(clauses.pick.unwrap_or(Pick::MeanScore), threshold, &live);
        let cost: f64 = wave.iter().map(|t| t.cost).sum();
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(budget) = clauses.budget {
            if *spent + cost > budget + 1e-12 {
                return Report {
                    winner: None,
                    score: None,
                    retries,
                    identical,
                    fallback,
                    error: Some("budget".into()),
                };
            }
        }
        *spent += cost;
        drop(spent);
        // A split consensus has no winner. That fails the wave even when
        // UNTIL was omitted, so RETRY can ask for another wave.
        let consensus_split = clauses.pick == Some(Pick::Consensus) && winner.is_empty();
        let ok = !consensus_split && clauses.until.as_ref().is_none_or(|cond| match cond {
            Cond::ScoreGte(n) => score >= *n,
            Cond::ScoreLt(n) => score < *n,
            Cond::NonEmpty => !winner.is_empty(),
            Cond::LenGte(_) => true,
        });
        if ok || retries >= limit {
            if !ok {
                return Report {
                    winner: None,
                    score: Some(score),
                    retries,
                    identical,
                    fallback,
                    error: Some("retry exhausted".into()),
                };
            }
            return Report {
                winner: Some(winner),
                score: Some(score),
                retries,
                identical,
                fallback,
                error: None,
            };
        }
        if i + 1 == waves.len() {
            return Report {
                winner: None,
                score: Some(score),
                retries,
                identical,
                fallback,
                error: Some("need wave".into()),
            };
        }
    }
    Report {
        winner: None,
        score: None,
        retries,
        identical: false,
        fallback,
        error: Some("no wave".into()),
    }
}

fn passes_output(clauses: &Clauses, take: &Take) -> bool {
    let Some(cond) = &clauses.check_output else {
        return true;
    };
    match cond {
        Cond::ScoreGte(n) => take.score >= *n,
        Cond::ScoreLt(n) => take.score < *n,
        Cond::NonEmpty => !take.value.is_empty(),
        Cond::LenGte(n) => take.value.len() >= *n,
    }
}

fn identical_takes(takes: &[&Take]) -> bool {
    if takes.len() < 2 {
        return false;
    }
    let model = &takes[0].model;
    let score = takes[0].score;
    takes.iter().all(|t| &t.model == model && (t.score - score).abs() < 1e-9)
}

fn reduce(pick: Pick, threshold: f64, takes: &[&Take]) -> (String, f64) {
    match pick {
        Pick::Vote => {
            let yes = takes.iter().filter(|t| t.value == "true").count();
            let no = takes.len() - yes;
            let mean = takes.iter().map(|t| t.score).sum::<f64>() / takes.len() as f64;
            let winner = if yes > no {
                "true"
            } else if no > yes {
                "false"
            } else if mean >= threshold {
                "true"
            } else {
                "false"
            };
            (winner.into(), mean)
        }
        Pick::MeanScore => {
            let mean = takes.iter().map(|t| t.score).sum::<f64>() / takes.len() as f64;
            let winner = if mean >= threshold { "true" } else { "false" };
            (winner.into(), mean)
        }
        Pick::FirstValid => {
            let t = takes[0];
            (t.value.clone(), t.score)
        }
        Pick::Judge => {
            let t = takes.iter().find(|t| t.judge).unwrap_or(&takes[0]);
            (t.value.clone(), t.score)
        }
        Pick::Consensus => {
            let first = &takes[0].value;
            if takes.iter().all(|t| &t.value == first) {
                let mean = takes.iter().map(|t| t.score).sum::<f64>() / takes.len() as f64;
                (first.clone(), mean)
            } else {
                (String::new(), 0.0)
            }
        }
    }
}

/// Pull the clause tail off a statement. `None` when there is no clause
/// keyword, so plain MEANS and datetime EXTRACT stay untouched.
pub fn split_tail(sql: &str) -> Option<(&str, Clauses)> {
    let lower = sql.to_ascii_lowercase();
    let keys = [
        " using model ",
        " with takes ",
        " pick by ",
        " retry ",
        " check input ",
        " check output ",
        " no cache",
        " refresh",
        " timeout ",
        " budget ",
    ];
    let mut at = None;
    for key in keys {
        if let Some(i) = lower.find(key) {
            at = Some(at.map(|a: usize| a.min(i)).unwrap_or(i));
        }
    }
    // A bare CACHE word is too common. Require it as its own clause only
    // when another clause keyword is already present, or as ` cache` at
    // the end of a semantic call. Handled when another key matched and
    // the tail parser sees it.
    let at = at?;
    let tail = sql[at..].trim().trim_end_matches(';').trim();
    let clauses = parse_tail(tail).ok()?;
    Some((sql[..at].trim(), clauses))
}

/// One slot in a live wave: a model, a take index, and an optional
/// rephrasing of the question. `judge` marks the take `PICK BY JUDGE` keeps.
#[derive(Clone, Debug)]
pub struct WaveSpec {
    pub model: String,
    pub take: u32,
    pub phrasing: String,
    pub judge: bool,
}

/// Models × takes × phrasings. A fallback wave is one call to `FALLBACK`.
pub fn wave_specs(clauses: &Clauses, fallback_wave: bool) -> Vec<WaveSpec> {
    if fallback_wave {
        let model = clauses.fallback.clone().unwrap_or_default();
        return vec![WaveSpec { model, take: 0, phrasing: String::new(), judge: true }];
    }
    let models = if clauses.across.is_empty() {
        vec![clauses.model.clone().unwrap_or_else(|| "default".into())]
    } else {
        clauses.across.clone()
    };
    let n = clauses.takes.unwrap_or(1).clamp(1, 8);
    let phrasings = if clauses.phrasings.is_empty() {
        vec![String::new()]
    } else {
        clauses.phrasings.clone()
    };
    let mut out = Vec::new();
    for model in &models {
        for phrasing in &phrasings {
            for take in 0..n {
                out.push(WaveSpec {
                    model: model.clone(),
                    take,
                    phrasing: phrasing.clone(),
                    judge: clauses.model.as_deref() == Some(model.as_str()),
                });
            }
        }
    }
    if out.iter().all(|s| !s.judge) {
        if let Some(last) = out.last_mut() {
            last.judge = true;
        }
    }
    out
}

/// Same reducer as [`run_scripted`]. `produce(true)` is the fallback wave.
/// Each attempt resets the session spend so a rebuilt wave list is not billed twice.
pub fn run_generated<F>(clauses: &Clauses, input_len: usize, mut produce: F) -> Report
where
    F: FnMut(bool) -> Vec<Take>,
{
    let mut waves = vec![produce(false)];
    let mut fallback_used = false;
    for _ in 0..8 {
        reset_budget();
        let report = run_scripted(clauses, input_len, &waves);
        if report.error.as_deref() == Some("need wave") {
            waves.push(produce(false));
            continue;
        }
        if report.error.as_deref() == Some("no valid take")
            && clauses.fallback.is_some()
            && !report.fallback
            && !fallback_used
        {
            fallback_used = true;
            waves.push(produce(true));
            continue;
        }
        return report;
    }
    reset_budget();
    run_scripted(clauses, input_len, &waves)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clause_tail_stops_at_as_and_other_terminators() {
        let aliased = parse_tail("WITH TAKES 3 AS r").unwrap();
        assert_eq!(aliased.takes, Some(3));
        let comma = parse_tail("WITH TAKES 3, other").unwrap();
        assert_eq!(comma.takes, Some(3));
        let from = parse_tail("WITH TAKES 2 FROM t").unwrap();
        assert_eq!(from.takes, Some(2));
        let paren = parse_tail("WITH TAKES 4)").unwrap();
        assert_eq!(paren.takes, Some(4));
        let report = parse_tail("WITH TAKES 3 REPORT").unwrap();
        assert_eq!(report.takes, Some(3));
    }

    #[test]
    fn call_beats_function_beats_guc() {
        let guc = parse_tail("WITH TAKES 1 TEMPERATURE 0 PICK BY MEAN SCORE").unwrap();
        let function = parse_tail("WITH TAKES 3 PICK BY VOTE").unwrap();
        let call = parse_tail("WITH TEMPERATURE 0.2").unwrap();
        let merged = merge(call, &function, &guc);
        assert_eq!(merged.takes, Some(3));
        assert_eq!(merged.pick, Some(Pick::Vote));
        assert_eq!(merged.temperature, Some(0.2));
        let _ = guc;
    }

    #[test]
    fn clauses_sql_scripted_fake() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../regress/rvbbit/clauses.sql");
        let sql = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let mut case = String::new();
        let mut waves: Vec<Vec<Take>> = Vec::new();
        let mut wave: Vec<Take> = Vec::new();
        let mut expect = String::new();
        let mut input_len = 32usize;
        let mut saw = 0;
        for line in sql.lines().chain(std::iter::once("-- end")) {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("-- case ") {
                if !case.is_empty() {
                    if !wave.is_empty() {
                        waves.push(std::mem::take(&mut wave));
                    }
                    finish(&case, &expect, input_len, &waves);
                    saw += 1;
                }
                case = rest.to_string();
                waves.clear();
                wave.clear();
                expect.clear();
                input_len = 32;
                continue;
            }
            if line == "-- wave" {
                if !wave.is_empty() {
                    waves.push(std::mem::take(&mut wave));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("-- take ") {
                wave.push(parse_take(rest));
                continue;
            }
            if let Some(rest) = line.strip_prefix("-- expect ") {
                expect = rest.to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("-- sql ") {
                if !expect.is_empty() {
                    expect.push(' ');
                }
                expect.push_str("sql=");
                expect.push_str(rest.trim());
                continue;
            }
            if let Some(rest) = line.strip_prefix("-- input-len ") {
                input_len = rest.trim().parse().unwrap();
                continue;
            }
            if line.starts_with("-- end") && !case.is_empty() {
                if !wave.is_empty() {
                    waves.push(std::mem::take(&mut wave));
                }
                finish(&case, &expect, input_len, &waves);
                saw += 1;
                case.clear();
            }
        }
        assert!(saw >= 6, "clauses.sql should drive the fake, saw {saw}");
    }

    fn finish(case: &str, expect: &str, input_len: usize, waves: &[Vec<Take>]) {
        reset_budget();
        let sql_line = expect_sql(expect);
        let clauses = if sql_line.is_empty() {
            let pick_name = expect_field(expect, "pick").unwrap_or_else(|| "mean".into());
            Clauses {
                pick: Some(match pick_name.as_str() {
                    "vote" => Pick::Vote,
                    "consensus" => Pick::Consensus,
                    "judge" => Pick::Judge,
                    "first" => Pick::FirstValid,
                    _ => Pick::MeanScore,
                }),
                ..Clauses::default()
            }
        } else {
            parse_tail(sql_line.trim()).unwrap_or_else(|e| panic!("{case}: {e}"))
        };
        let report = run_scripted(&clauses, input_len, waves);
        if let Some(err) = expect_field(expect, "error") {
            assert_eq!(report.error.as_deref(), Some(err.as_str()), "{case} {report:?}");
        } else {
            assert!(report.error.is_none(), "{case} {report:?}");
            let winner = expect_field(expect, "winner").unwrap();
            assert_eq!(report.winner.as_deref(), Some(winner.as_str()), "{case}");
        }
        if let Some(n) = expect_field(expect, "retries") {
            assert_eq!(report.retries, n.parse::<u32>().unwrap(), "{case}");
        }
        if let Some(flag) = expect_field(expect, "identical") {
            assert_eq!(report.identical, flag == "true", "{case}");
        }
        if let Some(flag) = expect_field(expect, "fallback") {
            assert_eq!(report.fallback, flag == "true", "{case}");
        }
    }

    fn expect_sql(expect: &str) -> String {
        expect.find("sql=").map(|i| expect[i + 4..].trim().to_string()).unwrap_or_default()
    }

    fn expect_field(expect: &str, name: &str) -> Option<String> {
        for part in expect.split_whitespace() {
            if let Some((k, v)) = part.split_once('=') {
                if k == name {
                    return Some(v.to_string());
                }
            }
        }
        None
    }

    fn parse_take(rest: &str) -> Take {
        let mut take = Take {
            model: "jev".into(),
            score: 0.0,
            value: "true".into(),
            cost: 0.0,
            error: false,
            judge: false,
        };
        for part in rest.split_whitespace() {
            let Some((k, v)) = part.split_once('=') else {
                if part == "error" {
                    take.error = true;
                }
                if part == "judge" {
                    take.judge = true;
                }
                continue;
            };
            match k {
                "model" => take.model = v.into(),
                "score" => take.score = v.parse().unwrap(),
                "value" => take.value = v.into(),
                "cost" => take.cost = v.parse().unwrap(),
                _ => {}
            }
        }
        take
    }
}
