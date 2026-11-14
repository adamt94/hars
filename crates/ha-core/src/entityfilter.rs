//! HA's `helpers/entityfilter.py`: include/exclude lists of domains,
//! entity ids and `fnmatch` globs, decided in the same six cases.

use std::collections::HashSet;

/// One `fnmatch` glob, translated as `fnmatch.translate` does and matched
/// against the whole id (`re.match` on `(?s:…)\z`).
#[derive(Debug, Clone)]
pub struct Glob(Vec<Tok>);

#[derive(Debug, Clone)]
enum Tok {
    /// `*`: any run of characters, newlines included.
    Star,
    /// `?`: any one character.
    Any,
    Lit(char),
    /// `[…]`: inclusive ranges (a single character is a one-wide range).
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    /// An empty class, which matches nothing.
    Never,
}

impl Glob {
    pub fn new(pattern: &str) -> Self {
        let pat: Vec<char> = pattern.chars().collect();
        let n = pat.len();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < n {
            let c = pat[i];
            i += 1;
            match c {
                '*' => {
                    while i < n && pat[i] == '*' {
                        i += 1;
                    }
                    toks.push(Tok::Star);
                }
                '?' => toks.push(Tok::Any),
                '[' => {
                    let mut j = i;
                    if j < n && pat[j] == '!' {
                        j += 1;
                    }
                    if j < n && pat[j] == ']' {
                        j += 1;
                    }
                    while j < n && pat[j] != ']' {
                        j += 1;
                    }
                    if j >= n {
                        toks.push(Tok::Lit('['));
                    } else {
                        toks.push(class(&pat, i, j));
                        i = j + 1;
                    }
                }
                c => toks.push(Tok::Lit(c)),
            }
        }
        Self(toks)
    }

    pub fn matches(&self, s: &str) -> bool {
        let s: Vec<char> = s.chars().collect();
        let one = |t: &Tok, c: char| match t {
            Tok::Any => true,
            Tok::Lit(l) => *l == c,
            Tok::Class { negated, ranges } => {
                ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated
            }
            Tok::Never | Tok::Star => false,
        };
        // The classic wildcard walk: each token but `*` takes one character;
        // on a mismatch, the last `*` takes one more.
        let (mut t, mut i) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while i < s.len() {
            match self.0.get(t) {
                Some(Tok::Star) => {
                    star = Some((t, i));
                    t += 1;
                }
                Some(tok) if one(tok, s[i]) => {
                    t += 1;
                    i += 1;
                }
                _ => match star {
                    Some((st, si)) => {
                        star = Some((st, si + 1));
                        t = st + 1;
                        i = si + 1;
                    }
                    None => return false,
                },
            }
        }
        self.0[t..].iter().all(|t| matches!(t, Tok::Star))
    }
}

/// `fnmatch._translate`'s bracket branch for `pat[i..j]` (between `[` and
/// `]`), then the class as `re` reads what it wrote.
fn class(pat: &[char], mut i: usize, j: usize) -> Tok {
    let stuff = &pat[i..j];
    // The class text as fnmatch writes it, with escapes as `(char, escaped)`.
    let mut text: Vec<(char, bool)> = Vec::new();
    if !stuff.contains(&'-') {
        text.extend(stuff.iter().map(|&c| (c, false)));
    } else {
        // Split at the hyphens that make ranges; the rest are literal.
        let mut chunks: Vec<Vec<char>> = Vec::new();
        let mut k = if pat[i] == '!' { i + 2 } else { i + 1 };
        while let Some(found) = (k..j).find(|&x| pat[x] == '-') {
            chunks.push(pat[i..found].to_vec());
            i = found + 1;
            k = found + 3;
        }
        let chunk = &pat[i..j];
        if !chunk.is_empty() {
            chunks.push(chunk.to_vec());
        } else if let Some(last) = chunks.last_mut() {
            last.push('-');
        }
        // Remove empty ranges.
        let mut k = chunks.len();
        while k > 1 {
            k -= 1;
            let (Some(&hi), Some(&lo)) = (chunks[k - 1].last(), chunks[k].first()) else {
                // Python's IndexError: the filter can't be built.
                return Tok::Never;
            };
            if hi > lo {
                let tail = chunks.remove(k);
                chunks[k - 1].pop();
                chunks[k - 1].extend_from_slice(&tail[1..]);
            }
        }
        for (n, chunk) in chunks.iter().enumerate() {
            if n > 0 {
                text.push(('-', false));
            }
            text.extend(chunk.iter().map(|&c| (c, c == '-')));
        }
    }
    if text.is_empty() {
        return Tok::Never;
    }
    if text == [('!', false)] {
        return Tok::Any;
    }
    let negated = text[0] == ('!', false);
    let body = if negated { &text[1..] } else { &text[..] };
    // `re`'s class parser: `a-b` is a range unless the hyphen is escaped.
    let mut ranges = Vec::new();
    let mut n = 0;
    while n < body.len() {
        let lo = body[n].0;
        n += 1;
        if body.get(n) == Some(&('-', false)) && n + 1 < body.len() {
            let hi = body[n + 1].0;
            n += 2;
            // A reversed range is `re.error`; nothing can match it.
            if lo <= hi {
                ranges.push((lo, hi));
            }
        } else {
            ranges.push((lo, lo));
        }
    }
    Tok::Class { negated, ranges }
}

/// `EntityFilter`: what `convert_include_exclude_filter` builds.
#[derive(Debug, Clone, Default)]
pub struct EntityFilter {
    include_e: HashSet<String>,
    exclude_e: HashSet<String>,
    include_d: HashSet<String>,
    exclude_d: HashSet<String>,
    include_g: Vec<Glob>,
    exclude_g: Vec<Glob>,
}

/// One side of a filter: `{domains, entities, entity_globs}`.
#[derive(Debug, Clone, Default)]
pub struct FilterLists {
    pub domains: Vec<String>,
    pub entities: Vec<String>,
    pub entity_globs: Vec<String>,
}

impl EntityFilter {
    pub fn new(include: FilterLists, exclude: FilterLists) -> Self {
        let globs = |g: Vec<String>| {
            let unique: HashSet<String> = g.into_iter().collect();
            unique.iter().map(|g| Glob::new(g)).collect()
        };
        Self {
            include_e: include.entities.into_iter().collect(),
            exclude_e: exclude.entities.into_iter().collect(),
            include_d: include.domains.into_iter().collect(),
            exclude_d: exclude.domains.into_iter().collect(),
            include_g: globs(include.entity_globs),
            exclude_g: globs(exclude.entity_globs),
        }
    }

    /// `empty_filter`: no lists at all, so everything passes.
    pub fn is_empty(&self) -> bool {
        !self.have_include() && !self.have_exclude()
    }

    fn have_include(&self) -> bool {
        !(self.include_e.is_empty() && self.include_d.is_empty() && self.include_g.is_empty())
    }

    fn have_exclude(&self) -> bool {
        !(self.exclude_e.is_empty() && self.exclude_d.is_empty() && self.exclude_g.is_empty())
    }

    /// `_generate_filter_from_sets_and_pattern_lists`, cases 1–6.
    pub fn matches(&self, entity_id: &str) -> bool {
        let domain = entity_id.split('.').next().unwrap_or(entity_id);
        let include_g = || self.include_g.iter().any(|g| g.matches(entity_id));
        let exclude_g = || self.exclude_g.iter().any(|g| g.matches(entity_id));
        match (self.have_include(), self.have_exclude()) {
            (false, false) => true,
            (true, false) => {
                self.include_e.contains(entity_id) || self.include_d.contains(domain) || include_g()
            }
            (false, true) => {
                !(self.exclude_e.contains(entity_id)
                    || self.exclude_d.contains(domain)
                    || exclude_g())
            }
            _ if !self.include_d.is_empty() || !self.include_g.is_empty() => {
                self.include_e.contains(entity_id)
                    || (!self.exclude_e.contains(entity_id)
                        && (include_g() || (self.include_d.contains(domain) && !exclude_g())))
            }
            _ if !self.exclude_d.is_empty() || !self.exclude_g.is_empty() => {
                if self.exclude_d.contains(domain) || exclude_g() {
                    self.include_e.contains(entity_id)
                } else {
                    !self.exclude_e.contains(entity_id)
                }
            }
            _ => self.include_e.contains(entity_id),
        }
    }
}

#[cfg(test)]
mod tests;
