//! Integration tests for the semantic song-title breaker.
//!
//! The benchmark corpus is taken from the source research paper's
//! comparative-evaluation table (Bollywood, pan-Indian film, Punjabi
//! collaboration, Western pop, and classical titles). One documented
//! divergence exists: for `Malang Sajna (Lo Flip) - Sachet-Param para` the
//! paper's table lists a `BeforeBracket` split, but the paper's own
//! extracted Rust scoring (spaced-hyphen base 86, balance penalty
//! `(|l1-l2|/total)^1.3 * 35`) scores the hyphen candidate at ~82.6 vs
//! ~73.6 for the bracket candidate, so the faithful implementation
//! returns the hyphen split. That case is asserted as-implemented below
//! with this note, rather than by silently retuning the weights.

use speaker_title_breaker::{
    BreakReason, BreakerConfig, split_song_title, split_song_title_with_config,
};

/// Table-driven benchmark corpus: `(input, expected line1, expected line2,
/// expected reason)`.
const BENCHMARK: &[(&str, &str, &str, BreakReason)] = &[
    (
        "Dheere Dheere Se Meri Zindagi Mein Aana",
        "Dheere Dheere Se",
        "Meri Zindagi Mein Aana",
        BreakReason::AfterIndicClause,
    ),
    (
        "Tujhe Dekha Toh Yeh Jaana Sanam",
        "Tujhe Dekha Toh",
        "Yeh Jaana Sanam",
        BreakReason::AfterIndicClause,
    ),
    (
        "Dil Deewana Bin Sajna Ke Maane Na",
        "Dil Deewana",
        "Bin Sajna Ke Maane Na",
        BreakReason::BeforeIndicConj,
    ),
    (
        "Aap Jaisa Koi Meri Zindagi Mein Aaye To Baat Ban Jaye",
        "Aap Jaisa Koi Meri Zindagi Mein",
        "Aaye To Baat Ban Jaye",
        BreakReason::AfterIndicClause,
    ),
    (
        "Zindagi Pyar Ka Geet Hai Ise Har Dil Ko Gaana Padega",
        "Zindagi Pyar Ka Geet Hai",
        "Ise Har Dil Ko Gaana Padega",
        BreakReason::AfterIndicClause,
    ),
    (
        "Tum Se Hi Din Hota Hai Surmaiye Shaam Aati Hai",
        "Tum Se Hi Din Hota Hai",
        "Surmaiye Shaam Aati Hai",
        BreakReason::AfterIndicClause,
    ),
    (
        "Humein Tumse Pyar Kitna Yeh Hum Nahin Jaante",
        "Humein Tumse Pyar Kitna",
        "Yeh Hum Nahin Jaante",
        BreakReason::TopLevelSpace,
    ),
    (
        "Kabhi Khushi Kabhie Gham (Title Track)",
        "Kabhi Khushi Kabhie Gham",
        "(Title Track)",
        BreakReason::BeforeBracket,
    ),
    (
        "Kesariya (From \"Brahmastra\")",
        "Kesariya",
        "(From \"Brahmastra\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Chaleya (From \"Jawan\") [Hindi]",
        "Chaleya",
        "(From \"Jawan\") [Hindi]",
        BreakReason::BeforeBracket,
    ),
    (
        "Kaavaalaa (From \"Jailer\") [Hindi Version]",
        "Kaavaalaa (From \"Jailer\")",
        "[Hindi Version]",
        BreakReason::BetweenBrackets,
    ),
    (
        "Arabic Kuthu - Halamithi Habibo (From \"Beast\")",
        "Arabic Kuthu - Halamithi Habibo",
        "(From \"Beast\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Hukum - Thalaivar Alappara (From \"Jailer\")",
        "Hukum - Thalaivar Alappara",
        "(From \"Jailer\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Oo Antava Mava..Oo Oo Antava (From \"Pushpa - The Rise\")",
        "Oo Antava Mava..Oo Oo Antava",
        "(From \"Pushpa - The Rise\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Tera Mera Rishta - New Version (From \"Awarapan 2\")",
        "Tera Mera Rishta - New Version",
        "(From \"Awarapan 2\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Tu Mil Jaaye (From \"KING\") - Sachin-Jigar, Ed Sheeran, Arijit Singh",
        "Tu Mil Jaaye (From \"KING\")",
        "Sachin-Jigar, Ed Sheeran, Arijit Singh",
        BreakReason::SpacedHyphen,
    ),
    (
        "Brown Munde - AP Dhillon x Gurinder Gill x Shinda Kahlon",
        "Brown Munde - AP Dhillon",
        "x Gurinder Gill x Shinda Kahlon",
        BreakReason::BeforeFeat,
    ),
    (
        "Taambdi Chaamdi - Kratex x Shreyas",
        "Taambdi Chaamdi",
        "Kratex x Shreyas",
        BreakReason::SpacedHyphen,
    ),
    (
        "Yeh Prem Mol Liya Title Track - From \"Yeh Prem Mol Liya\"",
        "Yeh Prem Mol Liya Title Track",
        "From \"Yeh Prem Mol Liya\"",
        BreakReason::SpacedHyphen,
    ),
    (
        "Samajavaragamana (From \"Ala Vaikunthapurramuloo\")",
        "Samajavaragamana",
        "(From \"Ala Vaikunthapurramuloo\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Jimikki Kammal (From \"Velipadinte Pusthakam\")",
        "Jimikki Kammal",
        "(From \"Velipadinte Pusthakam\")",
        BreakReason::BeforeBracket,
    ),
    (
        "Lying Is the Most Fun a Girl Can Have Without Taking Her Clothes Off",
        "Lying Is the Most Fun a Girl Can Have",
        "Without Taking Her Clothes Off",
        BreakReason::BeforeEngConj,
    ),
    (
        "The Only Difference Between Martyrdom And Suicide Is Press Coverage",
        "The Only Difference Between Martyrdom",
        "And Suicide Is Press Coverage",
        BreakReason::BeforeEngConj,
    ),
    (
        "Hope Is a Dangerous Thing for a Woman Like Me to Have - but I Have It",
        "Hope Is a Dangerous Thing for a Woman Like Me to Have",
        "but I Have It",
        BreakReason::SpacedHyphen,
    ),
    (
        "Star Wars: Episode IV - A New Hope: Main Title / Rebel Blockade Runner",
        "Star Wars: Episode IV - A New Hope",
        "Main Title / Rebel Blockade Runner",
        BreakReason::Colon,
    ),
    (
        "Symphony No. 9 in D Minor, Op. 125 \"Choral\": IV. Presto - Allegro assai",
        "Symphony No. 9 in D Minor, Op. 125 \"Choral\"",
        "IV. Presto - Allegro assai",
        BreakReason::Colon,
    ),
    (
        "All Too Well (10 Minute Version) (Taylor's Version) [From The Vault]",
        "All Too Well (10 Minute Version)",
        "(Taylor's Version) [From The Vault]",
        BreakReason::BetweenBrackets,
    ),
    (
        "It's Not Living (If It's Not With You)",
        "It's Not Living",
        "(If It's Not With You)",
        BreakReason::BeforeBracket,
    ),
    (
        "Don't Stop Believin' (2024 Remaster)",
        "Don't Stop Believin'",
        "(2024 Remaster)",
        BreakReason::BeforeBracket,
    ),
];

#[test]
fn benchmark_corpus_matches_human_editorial_breaks() {
    for (input, exp_l1, exp_l2, exp_reason) in BENCHMARK {
        let split = split_song_title(input);
        assert_eq!(split.line1, *exp_l1, "line1 for: {input}");
        assert_eq!(split.line2, *exp_l2, "line2 for: {input}");
        assert_eq!(split.reason, *exp_reason, "reason for: {input}");
        assert!(split.score.is_finite(), "score must be finite for: {input}");
    }
}

#[test]
fn malang_sajna_divergence_from_paper_table_is_documented() {
    // Paper table: BeforeBracket ("Malang Sajna" / "(Lo Flip) - ...").
    // Paper's own extracted Rust weights score the spaced hyphen higher
    // (base 86 vs bracket 88, but far better balance), so the faithful
    // implementation returns the hyphen split. See module doc note.
    let split = split_song_title("Malang Sajna (Lo Flip) - Sachet-Param para");
    assert_eq!(split.line1, "Malang Sajna (Lo Flip)");
    assert_eq!(split.line2, "Sachet-Param para");
    assert_eq!(split.reason, BreakReason::SpacedHyphen);
}

#[test]
fn short_empty_and_whitespace_titles_stay_single_line() {
    for input in [
        "",
        "   ",
        "Kesariya",
        "  Kesariya  ",
        "1234567890123456789012",
    ] {
        let split = split_song_title(input);
        assert_eq!(split.reason, BreakReason::SingleLine, "input: {input:?}");
        assert_eq!(split.line1, input.trim());
        assert!(split.line2.is_empty(), "input: {input:?}");
        assert_eq!(split.score, 100.0, "input: {input:?}");
    }
    // 23 chars exceeds the default min_wrap_chars of 22 and must split.
    let split = split_song_title("12345678901234567890123");
    assert_eq!(split.reason, BreakReason::HardWrap);
}

#[test]
fn explicit_delimiter_tiers_win_at_balance() {
    let cases: &[(&str, &str, &str, BreakReason)] = &[
        (
            "Alpha Beta Gamma Delta | Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::Pipe,
        ),
        (
            "Alpha Beta Gamma Delta // Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::DoubleSlash,
        ),
        (
            "Alpha Beta Gamma Delta — Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::EmOrEnDash,
        ),
        (
            "Alpha Beta Gamma Delta – Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::EmOrEnDash,
        ),
        (
            "Alpha Beta Gamma Delta ~ Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::Tilde,
        ),
        (
            "Alpha Beta Gamma Delta; Epsilon Zeta Eta Theta",
            "Alpha Beta Gamma Delta",
            "Epsilon Zeta Eta Theta",
            BreakReason::Semicolon,
        ),
        (
            "Alpha Beta Gamma Delta, Epsilon Zeta Eta Theta Iota",
            "Alpha Beta Gamma Delta,",
            "Epsilon Zeta Eta Theta Iota",
            BreakReason::Comma,
        ),
    ];
    for (input, exp_l1, exp_l2, exp_reason) in cases {
        let split = split_song_title(input);
        assert_eq!(split.line1, *exp_l1, "line1 for: {input}");
        assert_eq!(split.line2, *exp_l2, "line2 for: {input}");
        assert_eq!(split.reason, *exp_reason, "reason for: {input}");
    }
}

#[test]
fn collaboration_ampersand_and_feat_variants_group_credits() {
    let split = split_song_title("Alpha Beta Gamma Delta & Epsilon Zeta Eta Theta");
    assert_eq!(split.line1, "Alpha Beta Gamma Delta");
    assert_eq!(split.line2, "& Epsilon Zeta Eta Theta");
    assert_eq!(split.reason, BreakReason::BeforeFeat);

    let split = split_song_title("Alpha Beta Gamma Delta ft. Epsilon Zeta Eta Theta");
    assert_eq!(split.line1, "Alpha Beta Gamma Delta");
    assert_eq!(split.reason, BreakReason::BeforeFeat);
}

#[test]
fn apostrophe_disambiguation_prevents_false_quote_tracking() {
    // G-dropping elision: if "Believin'" toggled in_quote, the bracket
    // candidate would be masked and the split would land elsewhere.
    let split = split_song_title("Don't Stop Believin' (2024 Remaster)");
    assert_eq!(split.line1, "Don't Stop Believin'");
    assert_eq!(split.line2, "(2024 Remaster)");

    // Word-initial aphesis/decade: "'Cause" must not open a quote either,
    // or the following bracket candidate would be masked.
    let split = split_song_title("Alpha 'Cause Beta Gamma Delta (2024 Remaster)");
    assert_eq!(split.line1, "Alpha 'Cause Beta Gamma Delta");
    assert_eq!(split.line2, "(2024 Remaster)");
    assert_eq!(split.reason, BreakReason::BeforeBracket);
}

#[test]
fn standalone_single_quote_tokens_toggle_enclosure() {
    // A `'` token flanked by whitespace on both sides is neither a
    // contraction (rule 1), an elision (rule 2), nor an aphesis (rule 3),
    // so it toggles quote state: spaces inside the span are not top-level
    // and no candidate is generated inside it.
    let split = split_song_title("Alpha Beta ' Gamma Delta Epsilon ' Zeta Eta Theta Iota");
    assert_eq!(split.line1, "Alpha Beta ' Gamma Delta Epsilon '");
    assert_eq!(split.line2, "Zeta Eta Theta Iota");
    assert_eq!(split.reason, BreakReason::TopLevelSpace);

    // The in-quote hyphen of a double-quoted film title must never be a
    // candidate: breaking inside would leave a line ending in "(From".
    let split = split_song_title("Oo Antava Mava..Oo Oo Antava (From \"Pushpa - The Rise\")");
    assert!(!split.line1.ends_with("(From"));
    assert!(!split.line1.ends_with("(From \"Pushpa"));
}

#[test]
fn word_adjacent_single_quotes_are_apostrophes_by_spec_heuristic() {
    // Documented tradeoff of the spec's apostrophe rules (see PR #49 review):
    // a `'` preceded by whitespace and followed by an alphanumeric is always
    // classified as aphesis (rule 3), so a word-adjacent single-quoted span
    // such as `'Beta - Gamma'` does NOT toggle enclosure and the hyphen
    // inside it remains a candidate. Narrowing the heuristic here would
    // break the benchmark cases it exists for ("Taylor's", "Believin'",
    // "Don't"), so the behavior is pinned, not silently changed.
    let split = split_song_title("Alpha 'Beta - Gamma' Delta Epsilon Zeta");
    assert_eq!(split.line1, "Alpha 'Beta");
    assert_eq!(split.line2, "Gamma' Delta Epsilon Zeta");
    assert_eq!(split.reason, BreakReason::SpacedHyphen);
}

#[test]
fn unbalanced_bracket_still_breaks_before_it() {
    let split = split_song_title("Alpha Beta Gamma Delta (Epsilon Zeta Eta Theta");
    assert!(split.line2.starts_with('('), "line2: {}", split.line2);
    assert_eq!(split.reason, BreakReason::BeforeBracket);
}

#[test]
fn fully_enclosed_title_falls_back_to_inner_space() {
    let split = split_song_title("(Alpha Beta Gamma Delta Epsilon Zeta)");
    assert_eq!(split.reason, BreakReason::InnerSpace);
    assert_ne!(split.line1, "");
    assert_ne!(split.line2, "");
    assert_eq!(
        format!("{} {}", split.line1, split.line2),
        "(Alpha Beta Gamma Delta Epsilon Zeta)"
    );
}

#[test]
fn single_long_word_hard_wraps_at_mid_char_without_panic() {
    let title = "Supercalifragilisticexpialidocious";
    let split = split_song_title(title);
    assert_eq!(split.reason, BreakReason::HardWrap);
    assert_eq!(split.score, 0.0);
    assert_eq!(format!("{}{}", split.line1, split.line2), title);
    assert_eq!(split.line1.chars().count(), title.chars().count() / 2);
}

#[test]
fn unicode_titles_never_panic_and_respect_char_boundaries() {
    let titles = [
        "αβγ δευ ζην θικ λμν ξοπ ρστυ φχψ ωαβγ",
        "こんにちは 世界 テスト タイトル です これは",
        "🎵🎶 Alpha Beta Gamma 🎧 Delta Epsilon 🎵 Zeta",
        "Café del Mar — Sunset Session Original Mix",
    ];
    for title in titles {
        let split = split_song_title(title);
        assert!(split.score.is_finite(), "title: {title}");
        if split.reason != BreakReason::SingleLine {
            assert_ne!(split.line1, "", "title: {title}");
            assert_ne!(split.line2, "", "title: {title}");
            assert!(split.line1.is_char_boundary(split.line1.len()));
            assert!(split.line2.is_char_boundary(split.line2.len()));
            // Line 2 must not start with a stripped delimiter remnant.
            assert!(
                !split.line2.starts_with(['-', '–', '—', '|', '~', ':', '/']),
                "title: {title}, line2: {}",
                split.line2
            );
        }
    }
}

#[test]
fn config_defaults_and_custom_thresholds() {
    assert_eq!(BreakerConfig::default().min_wrap_chars, 22);
    assert_eq!(BreakerConfig::default().max_line1_chars, None);

    // A 15-char title: single line by default, split with a lower threshold.
    let title = "Alpha Beta Gam";
    assert_eq!(split_song_title(title).reason, BreakReason::SingleLine);
    let config = BreakerConfig {
        min_wrap_chars: 10,
        max_line1_chars: None,
    };
    let split = split_song_title_with_config(title, &config);
    assert_ne!(split.reason, BreakReason::SingleLine);
    assert_ne!(split.line2, "");
}

#[test]
fn max_line1_chars_forces_earlier_syntactic_break() {
    let title = "I'm Like a Lawyer with the Way I'm Always Trying to Get You Off (Me & You)";
    // Unconstrained, the bracket break wins and line 1 is very long.
    let unconstrained = split_song_title(title);
    assert!(unconstrained.line1.chars().count() > 36);
    // With a 36-char budget the engine must shift left to fit.
    let config = BreakerConfig {
        min_wrap_chars: 22,
        max_line1_chars: Some(36),
    };
    let constrained = split_song_title_with_config(title, &config);
    assert!(
        constrained.line1.chars().count() <= 36,
        "line1 too long: {}",
        constrained.line1
    );
    assert_ne!(constrained.line2, "");
    assert!(constrained.score.is_finite());
}

#[test]
fn dangling_particle_is_avoided_as_line1_ending() {
    // ("the" is deliberately avoided here: it is an Indic clause-final verb
    // in the scoring model, so breaking after it is intentional, not dangling.)
    let split = split_song_title("Alpha Beta Gamma Delta of Epsilon Zeta Eta Theta Iota");
    assert!(
        !split.line1.to_lowercase().ends_with(" of"),
        "line1 ends with preposition: {}",
        split.line1
    );
}

#[test]
fn splits_are_deterministic_and_reconstruct_without_delimiters() {
    for (input, _, _, _) in BENCHMARK {
        let first = split_song_title(input);
        let second = split_song_title(input);
        assert_eq!(first, second, "non-deterministic split for: {input}");
        // Both lines are substrings of the input (no invented content).
        assert!(input.contains(&first.line1), "input: {input}");
        assert!(input.contains(&first.line2), "input: {input}");
        assert!(first.score.is_finite(), "input: {input}");
    }
}

#[test]
fn curly_quotes_and_unicode_dashes_are_handled() {
    let split = split_song_title("Alpha Beta Gamma Delta — “Epsilon Zeta” Eta Theta");
    assert!(split.score.is_finite());
    assert_ne!(split.line1, "");
    assert_ne!(split.line2, "");
    // The em-dash itself must not survive at either line edge.
    assert!(!split.line1.ends_with('—'));
    assert!(!split.line2.starts_with('—'));
}

#[test]
fn trailing_delimiter_only_line2_does_not_panic() {
    // Candidate whose line 2 would be only a delimiter exercises the
    // clean-line2 "stripped is empty" guard during candidate scoring.
    let split = split_song_title("Alpha Beta Gamma Delta Epsilon Zeta Eta —");
    assert!(split.score.is_finite());
    assert_ne!(split.line1, "");
}

#[test]
fn debug_and_clone_are_available_for_public_types() {
    let split = split_song_title("Kesariya (From \"Brahmastra\")");
    let cloned = split.clone();
    assert_eq!(split, cloned);
    assert!(format!("{split:?}").contains("BeforeBracket"));
    assert!(format!("{:?}", BreakerConfig::default()).contains("min_wrap_chars: 22"));
    assert_eq!(format!("{:?}", BreakReason::Colon), "Colon");
}
