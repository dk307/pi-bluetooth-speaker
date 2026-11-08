//! Semantic two-line splitting of song titles.
//!
//! See the crate-level documentation for the scoring model. All helpers in
//! this module are private; the public surface is [`split_song_title`],
//! [`split_song_title_with_config`], [`TitleSplit`], [`BreakReason`], and
//! [`BreakerConfig`].

/// Semantic reason the winning breakpoint was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakReason {
    /// Title fit on a single line (`<= min_wrap_chars`).
    SingleLine,
    /// Spaced pipe delimiter (` | `).
    Pipe,
    /// Spaced double-slash delimiter (` // `).
    DoubleSlash,
    /// Spaced em/en dash (` — ` / ` – `).
    EmOrEnDash,
    /// Spaced hyphen (` - `).
    SpacedHyphen,
    /// Colon + space (`: `); the colon itself is stripped.
    Colon,
    /// Spaced tilde (` ~ `).
    Tilde,
    /// Semicolon + space (`; `); the semicolon itself is stripped.
    Semicolon,
    /// Break immediately before an opening bracket (`(` or `[`).
    BeforeBracket,
    /// Break between two adjacent bracket groups (`) (` / `) [` / `] [`).
    BetweenBrackets,
    /// Break immediately before a collaboration connector (`feat.`, `x`, …).
    BeforeFeat,
    /// Break immediately after an Indic post-clause marker (`toh`, `se`,
    /// `mein`, clause-final verbs such as `hai`).
    AfterIndicClause,
    /// Break immediately before an Indic conjunction / privative particle
    /// (`aur`, `bin`, …).
    BeforeIndicConj,
    /// Break immediately before an English conjunction / preposition
    /// (`and`, `without`, `from`, …).
    BeforeEngConj,
    /// Break at a top-level comma + space; the comma stays on line 1.
    Comma,
    /// Break at a plain top-level whitespace.
    TopLevelSpace,
    /// Emergency fallback: break at a whitespace inside an enclosure
    /// (only used when no top-level candidate exists).
    InnerSpace,
    /// Emergency fallback: hard wrap at the midpoint character of a single
    /// continuous word with no whitespace at all.
    HardWrap,
}

/// Output of the two-line song-title breaker.
#[derive(Debug, Clone, PartialEq)]
pub struct TitleSplit {
    /// First line (never empty when a split was performed).
    pub line1: String,
    /// Second line (empty only for [`BreakReason::SingleLine`]).
    pub line2: String,
    /// Semantic reason the winning breakpoint was selected.
    pub reason: BreakReason,
    /// Net score of the winning candidate (finite; 100.0 when the title
    /// fits on one line, 0.0 in the defensive fallback where no candidate
    /// yields two non-empty lines).
    pub score: f64,
}

/// Configuration parameters for UI integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Titles whose char count is `<= min_wrap_chars` stay on one line.
    pub min_wrap_chars: usize,
    /// Optional maximum char budget for line 1. Candidates exceeding it are
    /// heavily penalized so the split shifts left to a marker that fits.
    pub max_line1_chars: Option<usize>,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            min_wrap_chars: 22,
            max_line1_chars: None,
        }
    }
}

/// One candidate breakpoint.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// Byte index where line 1 ends.
    cut_start: usize,
    /// Byte index where line 2 begins.
    cut_end: usize,
    /// Tiered semantic base score.
    base_score: f64,
    /// Semantic reason.
    reason: BreakReason,
}

/// Determines whether a single quote at `idx` is an apostrophe (contraction,
/// possessive, or g-dropping elision) rather than an enclosing quotation
/// mark. Double quotes never reach this function: they always toggle.
fn is_apostrophe(chars: &[char], idx: usize) -> bool {
    let ch = chars[idx];
    if ch != '\'' && ch != '’' {
        return false;
    }
    let prev = idx.checked_sub(1).map(|i| chars[i]);
    let next = chars.get(idx + 1).copied();
    // 1. Internal contraction flanked by alphanumerics ("It's", "Don't").
    if let (Some(p), Some(n)) = (prev, next)
        && p.is_alphanumeric()
        && n.is_alphanumeric()
    {
        return true;
    }
    // 2. Word-final elision/possessive ("Believin'", "Taylor's", "Boys'").
    if let Some(p) = prev
        && p.is_alphanumeric()
        && next.is_none_or(|n| !n.is_alphanumeric())
    {
        return true;
    }
    // 3. Word-initial aphesis/decade ("'Cause", "'90s").
    if let Some(n) = next
        && n.is_alphanumeric()
        && prev.is_none_or(|p| !p.is_alphanumeric())
    {
        return true;
    }
    false
}

/// Computes enclosure state `(bracket_depth, in_quote)` for every byte offset.
fn compute_enclosure_depths(title: &str) -> Vec<(usize, bool)> {
    let chars: Vec<char> = title.chars().collect();
    let mut byte_depths = vec![(0_usize, false); title.len() + 1];
    let mut depth: usize = 0;
    let mut in_quote = false;
    let mut quote_char: Option<char> = None;
    let mut byte_pos = 0_usize;
    for (i, &ch) in chars.iter().enumerate() {
        if matches!(ch, '"' | '“' | '”') {
            // Any double-quote family member closes a double-quote span,
            // including a right curly ” that itself opened the span.
            if in_quote && matches!(quote_char, Some('"' | '“' | '”')) {
                in_quote = false;
                quote_char = None;
            } else if !in_quote {
                in_quote = true;
                quote_char = Some(ch);
            }
        } else if matches!(ch, '\'' | '‘' | '’') && !is_apostrophe(&chars, i) {
            if in_quote && matches!(quote_char, Some('\'' | '‘' | '’')) {
                in_quote = false;
                quote_char = None;
            } else if !in_quote {
                in_quote = true;
                quote_char = Some(ch);
            }
        }
        for slot in byte_depths
            .iter_mut()
            .take(byte_pos + ch.len_utf8())
            .skip(byte_pos)
        {
            *slot = (depth, in_quote);
        }
        if !in_quote {
            match ch {
                '(' | '[' | '{' => depth = depth.saturating_add(1),
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        byte_pos += ch.len_utf8();
    }
    if let Some(last) = byte_depths.last_mut() {
        *last = (depth, in_quote);
    }
    byte_depths
}

/// Whether `byte_idx` is at top level (no open bracket, not inside quotes).
fn is_top_level(depths: &[(usize, bool)], byte_idx: usize) -> bool {
    depths
        .get(byte_idx)
        .is_some_and(|&(depth, in_quote)| depth == 0 && !in_quote)
}

/// Tokenizes whitespace-separated words with their byte spans.
fn tokenize_words_with_spans(title: &str) -> Vec<(&str, usize, usize)> {
    let mut tokens = Vec::new();
    let mut in_word = false;
    let mut word_start = 0_usize;
    for (idx, ch) in title.char_indices() {
        if ch.is_whitespace() {
            if in_word {
                tokens.push((&title[word_start..idx], word_start, idx));
                in_word = false;
            }
        } else if !in_word {
            word_start = idx;
            in_word = true;
        }
    }
    if in_word {
        tokens.push((&title[word_start..title.len()], word_start, title.len()));
    }
    tokens
}

/// Strips leading redundant separator characters from line 2 after a split.
fn clean_line2_leading_delims(s: &str) -> &str {
    let trimmed = s.trim();
    let stripped = trimmed.trim_start_matches(|c: char| {
        matches!(c, '-' | '–' | '—' | '|' | '~' | ':' | '/') || c.is_whitespace()
    });
    if stripped.is_empty() {
        trimmed
    } else {
        stripped
    }
}

/// Normalizes a word for keyword matching: trims surrounding ASCII
/// punctuation (except `.`, significant for `feat.`/`prod.`/`vs.`) and
/// lowercases. Returns `(with_trailing_dot, without_trailing_dot)`.
fn normalize_word(word: &str) -> (String, String) {
    let trimmed = word.trim_matches(|c: char| c.is_ascii_punctuation() && c != '.');
    // A token that is entirely punctuation (e.g. "&") must survive trimming,
    // otherwise the collaboration-connector match below can never see it.
    let lower = if trimmed.is_empty() {
        word.to_lowercase()
    } else {
        trimmed.to_lowercase()
    };
    let no_dot = lower.trim_end_matches('.').to_string();
    (lower, no_dot)
}

/// Splits a song title into two lines using the default configuration.
#[must_use]
pub fn split_song_title(title: &str) -> TitleSplit {
    split_song_title_with_config(title, &BreakerConfig::default())
}

/// Splits a song title into two semantically balanced lines according to
/// `config`.
#[must_use]
pub fn split_song_title_with_config(title: &str, config: &BreakerConfig) -> TitleSplit {
    let trimmed = title.trim();
    let total_chars = trimmed.chars().count();
    if total_chars <= config.min_wrap_chars {
        return TitleSplit {
            line1: trimmed.to_string(),
            line2: String::new(),
            reason: BreakReason::SingleLine,
            score: 100.0,
        };
    }

    let depths = compute_enclosure_depths(trimmed);
    let words = tokenize_words_with_spans(trimmed);
    let mut candidates = collect_candidates(trimmed, &depths, &words);

    // Emergency fallback: inner-enclosure spaces only.
    if candidates.is_empty() {
        for pair in words.windows(2) {
            candidates.push(Candidate {
                cut_start: pair[0].2,
                cut_end: pair[1].1,
                base_score: 10.0,
                reason: BreakReason::InnerSpace,
            });
        }
    }

    // Emergency fallback: hard wrap a single continuous word at mid-char.
    if candidates.is_empty() {
        let mid_char = total_chars / 2;
        let mid_byte = trimmed
            .char_indices()
            .nth(mid_char)
            .map_or(trimmed.len(), |(idx, _)| idx);
        return TitleSplit {
            line1: trimmed[..mid_byte].to_string(),
            line2: trimmed[mid_byte..].to_string(),
            reason: BreakReason::HardWrap,
            score: 0.0,
        };
    }

    score_candidates(trimmed, &candidates, config).unwrap_or_else(|| TitleSplit {
        line1: trimmed.to_string(),
        line2: String::new(),
        reason: BreakReason::SingleLine,
        score: 0.0,
    })
}

/// Collects every candidate breakpoint (Tiers 1-5) for `trimmed`.
fn collect_candidates(
    trimmed: &str,
    depths: &[(usize, bool)],
    words: &[(&str, usize, usize)],
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let char_indices: Vec<(usize, char)> = trimmed.char_indices().collect();
    collect_whitespace_candidates(trimmed, depths, &char_indices, &mut candidates);
    collect_word_candidates(depths, words, &mut candidates);
    candidates
}

/// Whitespace-run scan: Tier 1 delimiters, Tier 2 brackets, Tier 5 spaces.
fn collect_whitespace_candidates(
    trimmed: &str,
    depths: &[(usize, bool)],
    char_indices: &[(usize, char)],
    candidates: &mut Vec<Candidate>,
) {
    let mut i = 0_usize;
    while i < char_indices.len() {
        let (byte_idx, ch) = char_indices[i];
        if !ch.is_whitespace() {
            i += 1;
            continue;
        }
        let ws_start = byte_idx;
        let mut j = i;
        while j < char_indices.len() && char_indices[j].1.is_whitespace() {
            j += 1;
        }
        let ws_end = char_indices.get(j).map_or(trimmed.len(), |&(idx, _)| idx);
        let prev_ch = i.checked_sub(1).map(|k| char_indices[k].1);
        let next_ch = char_indices.get(j).map(|&(_, c)| c);

        // Tier 2B: between adjacent brackets ") (" / ") [" / "] [".
        if matches!(prev_ch, Some(')' | ']')) && matches!(next_ch, Some('(' | '[')) {
            if is_top_level(depths, ws_start) {
                candidates.push(Candidate {
                    cut_start: ws_start,
                    cut_end: ws_end,
                    base_score: 86.0,
                    reason: BreakReason::BetweenBrackets,
                });
            }
        } else if matches!(next_ch, Some('(' | '[')) && is_top_level(depths, ws_start) {
            // Tier 2A: before an opening bracket.
            candidates.push(Candidate {
                cut_start: ws_start,
                cut_end: ws_end,
                base_score: 88.0,
                reason: BreakReason::BeforeBracket,
            });
        }

        collect_delimiter_candidate(
            trimmed,
            depths,
            char_indices,
            j,
            ws_start,
            next_ch,
            candidates,
        );

        // Tier 1/5: colon, semicolon, comma immediately before whitespace.
        if let Some(pc) = prev_ch
            && is_top_level(depths, char_indices[i - 1].0)
        {
            let prev_byte = char_indices[i - 1].0;
            match pc {
                ':' => candidates.push(Candidate {
                    cut_start: prev_byte,
                    cut_end: ws_end,
                    base_score: 80.0,
                    reason: BreakReason::Colon,
                }),
                ';' => candidates.push(Candidate {
                    cut_start: prev_byte,
                    cut_end: ws_end,
                    base_score: 70.0,
                    reason: BreakReason::Semicolon,
                }),
                ',' => candidates.push(Candidate {
                    cut_start: ws_start,
                    cut_end: ws_end,
                    base_score: 45.0,
                    reason: BreakReason::Comma,
                }),
                _ => {}
            }
        }

        // Tier 5: plain top-level whitespace.
        if is_top_level(depths, ws_start) {
            candidates.push(Candidate {
                cut_start: ws_start,
                cut_end: ws_end,
                base_score: 32.0,
                reason: BreakReason::TopLevelSpace,
            });
        }
        i = j;
    }
}

/// Tier 1 spaced-delimiter candidate (` | `, ` — `, ` - `, ` // `, …).
fn collect_delimiter_candidate(
    trimmed: &str,
    depths: &[(usize, bool)],
    char_indices: &[(usize, char)],
    j: usize,
    ws_start: usize,
    next_ch: Option<char>,
    candidates: &mut Vec<Candidate>,
) {
    let Some(nc) = next_ch else { return };
    if !is_top_level(depths, ws_start) {
        return;
    }
    let after_delim = j + 1;
    let has_space_after = char_indices
        .get(after_delim)
        .is_some_and(|&(_, c)| c.is_whitespace());
    if matches!(nc, '|' | '—' | '–' | '-' | '~') && has_space_after {
        let mut k = after_delim;
        while k < char_indices.len() && char_indices[k].1.is_whitespace() {
            k += 1;
        }
        let full_end = char_indices.get(k).map_or(trimmed.len(), |&(idx, _)| idx);
        let (score, reason) = match nc {
            '|' => (95.0, BreakReason::Pipe),
            '—' | '–' => (90.0, BreakReason::EmOrEnDash),
            '-' => (86.0, BreakReason::SpacedHyphen),
            _ => (75.0, BreakReason::Tilde),
        };
        candidates.push(Candidate {
            cut_start: ws_start,
            cut_end: full_end,
            base_score: score,
            reason,
        });
    } else if nc == '/'
        && char_indices
            .get(after_delim)
            .is_some_and(|&(_, c)| c == '/')
    {
        let mut k = after_delim + 1;
        while k < char_indices.len() && char_indices[k].1.is_whitespace() {
            k += 1;
        }
        let full_end = char_indices.get(k).map_or(trimmed.len(), |&(idx, _)| idx);
        candidates.push(Candidate {
            cut_start: ws_start,
            cut_end: full_end,
            base_score: 95.0,
            reason: BreakReason::DoubleSlash,
        });
    }
}

/// Word scan: Tier 3 collaboration connectors and Tier 4 clause markers.
fn collect_word_candidates(
    depths: &[(usize, bool)],
    words: &[(&str, usize, usize)],
    candidates: &mut Vec<Candidate>,
) {
    for (w_idx, &(word_raw, w_start, w_end)) in words.iter().enumerate() {
        let (lower, lower_no_dot) = normalize_word(word_raw);
        if w_idx > 0 && is_top_level(depths, w_start) {
            let prev_end = words[w_idx - 1].2;
            if matches!(
                lower.as_str(),
                "feat."
                    | "feat"
                    | "&"
                    | "ft."
                    | "featuring"
                    | "prod."
                    | "prod"
                    | "vs."
                    | "vs"
                    | "x"
            ) {
                candidates.push(Candidate {
                    cut_start: prev_end,
                    cut_end: w_start,
                    base_score: 72.0,
                    reason: BreakReason::BeforeFeat,
                });
            }
            if matches!(
                lower_no_dot.as_str(),
                "aur" | "ya" | "magar" | "lekin" | "bin" | "bina"
            ) {
                candidates.push(Candidate {
                    cut_start: prev_end,
                    cut_end: w_start,
                    base_score: 65.0,
                    reason: BreakReason::BeforeIndicConj,
                });
            }
            if matches!(
                lower_no_dot.as_str(),
                "and" | "or" | "but" | "without" | "because" | "since" | "from" | "into" | "unto"
            ) {
                candidates.push(Candidate {
                    cut_start: prev_end,
                    cut_end: w_start,
                    base_score: 60.0,
                    reason: BreakReason::BeforeEngConj,
                });
            } else if lower_no_dot == "with" {
                candidates.push(Candidate {
                    cut_start: prev_end,
                    cut_end: w_start,
                    base_score: 58.0,
                    reason: BreakReason::BeforeEngConj,
                });
            }
        }
        if w_idx + 1 < words.len() && is_top_level(depths, w_end) {
            let next_start = words[w_idx + 1].1;
            let after_score = match lower_no_dot.as_str() {
                "toh" => Some(68.0),
                "se" | "mein" | "me" => Some(64.0),
                "hai" | "hain" | "tha" | "thi" | "the" | "hoga" | "hogi" | "gaye" | "gayi"
                | "diya" | "diye" => Some(60.0),
                _ => None,
            };
            if let Some(score) = after_score {
                candidates.push(Candidate {
                    cut_start: w_end,
                    cut_end: next_start,
                    base_score: score,
                    reason: BreakReason::AfterIndicClause,
                });
            }
        }
    }
}

/// Scores every candidate and returns the best split, if any candidate
/// yields two non-empty lines.
fn score_candidates(
    trimmed: &str,
    candidates: &[Candidate],
    config: &BreakerConfig,
) -> Option<TitleSplit> {
    let mid_byte_f = trimmed.len() as f64 / 2.0;
    let mut best: Option<TitleSplit> = None;
    for cand in candidates {
        let l1 = trimmed[..cand.cut_start].trim();
        let l2 = clean_line2_leading_delims(&trimmed[cand.cut_end..]);
        let len1 = l1.chars().count();
        let len2 = l2.chars().count();
        if len1 == 0 || len2 == 0 {
            continue;
        }
        let total_len = (len1 + len2) as f64;
        let diff = (len1 as f64 - len2 as f64).abs();
        let balance_penalty = (diff / total_len).powf(1.3) * 35.0;
        let cut_mid = (cand.cut_start + cand.cut_end) as f64 / 2.0;
        let mid_penalty = if mid_byte_f > 0.0 {
            ((cut_mid - mid_byte_f).abs() / mid_byte_f) * 8.0
        } else {
            0.0
        };
        let w1: Vec<&str> = l1.split_whitespace().collect();
        let w2: Vec<&str> = l2.split_whitespace().collect();
        let orphan_penalty = orphan_penalty(cand.reason, &w1, &w2, len2);
        let mut end_penalty = 0.0;
        if cand.reason != BreakReason::AfterIndicClause
            && let Some(&last_w) = w1.last()
        {
            let clean_last = last_w
                .trim_end_matches(|c: char| {
                    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'')
                })
                .to_lowercase();
            if is_bad_line1_ending(&clean_last) {
                end_penalty += 40.0;
            }
        }
        let mut overflow_penalty = 0.0;
        if let Some(max_c) = config.max_line1_chars
            && len1 > max_c
        {
            overflow_penalty += 200.0 + ((len1 - max_c) as f64 * 10.0);
        }
        let total_score = cand.base_score
            - balance_penalty
            - mid_penalty
            - orphan_penalty
            - end_penalty
            - overflow_penalty;
        if best.as_ref().is_none_or(|b| total_score > b.score) {
            best = Some(TitleSplit {
                line1: l1.to_string(),
                line2: l2.to_string(),
                reason: cand.reason,
                score: total_score,
            });
        }
    }
    best
}

/// Asymmetric orphan/runt penalty for a candidate split.
fn orphan_penalty(reason: BreakReason, w1: &[&str], w2: &[&str], len2: usize) -> f64 {
    let mut penalty = 0.0;
    if w2.len() == 1 {
        penalty += 55.0;
        if w2[0].chars().count() <= 4 {
            penalty += 45.0;
        }
    } else if w2.len() == 2 && len2 < 8 {
        penalty += 20.0;
    }
    if w1.len() == 1
        && !matches!(
            reason,
            BreakReason::BeforeBracket
                | BreakReason::Pipe
                | BreakReason::DoubleSlash
                | BreakReason::EmOrEnDash
                | BreakReason::SpacedHyphen
        )
    {
        penalty += 35.0;
        if w1[0].chars().count() <= 4 {
            penalty += 25.0;
        }
    }
    penalty
}

/// Whether `word` is a prohibited dangling line-1 ending (article,
/// preposition, or Indic postposition/particle).
fn is_bad_line1_ending(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "to"
            | "of"
            | "in"
            | "on"
            | "at"
            | "by"
            | "for"
            | "from"
            | "with"
            | "and"
            | "or"
            | "but"
            | "feat"
            | "feat."
            | "&"
            | "ft."
            | "vs"
            | "vs."
            | "ka"
            | "ke"
            | "ki"
            | "ko"
            | "se"
            | "me"
            | "mein"
            | "par"
            | "pe"
            | "ne"
            | "aur"
            | "ya"
            | "ye"
            | "yeh"
            | "wo"
            | "woh"
            | "ek"
            | "jo"
            | "bin"
    )
}
