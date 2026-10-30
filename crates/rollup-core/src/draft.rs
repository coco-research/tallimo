//! The model seam and the validator for AI-drafted leadership updates. No
//! network: the only model here is a deterministic fake.

use crate::ai::{self, AiError};
use rusqlite::Connection;

/// A detected change offered to the model, for example id "ev-12".
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFact {
    pub id: String,
    pub text: String,
}

/// One item the model may rewrite: the facts that belong to it, and the prose
/// lines on the open draft as read.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRequest {
    pub item_id: String,
    pub facts: Vec<SourceFact>,
    pub completed: Vec<String>,
    pub next: Vec<String>,
    pub future: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftRequest {
    pub items: Vec<ItemRequest>,
}

/// Replaces one prose field of one item.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchLine {
    pub item_id: String,
    pub field: String,
    pub lines: Vec<String>,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Accepted(PatchLine),
    Rejected {
        item_id: String,
        field: String,
        reason: String,
    },
}

/// The seam a real gateway adapter plugs into. The fake here is deterministic.
pub trait Model {
    fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String>;
}

pub struct FakeModel;

impl Model for FakeModel {
    fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
        Ok(req
            .items
            .iter()
            .map(|item| {
                let mut lines = item.completed.clone();
                lines.extend(item.facts.iter().map(|fact| fact.text.clone()));
                PatchLine {
                    item_id: item.item_id.clone(),
                    field: "completed".to_string(),
                    lines,
                    source_ids: item.facts.iter().map(|fact| fact.id.clone()).collect(),
                }
            })
            .collect())
    }
}

/// Every number in a line: a run of digits, at most one decimal point. A comma
/// joins digits only as a thousands separator: there is a digit before it and
/// exactly three digits after it, followed by a non-digit or the end of the
/// text, so "12,500" reads as "12500" and "1,000,000" as "1000000". Any other
/// comma is a boundary, so "1,2" reads as "1" and "2", "Q3,Q4" as "3" and "4",
/// and "12,13" as "12" and "13".
fn numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut num = String::new();
        while i < chars.len() {
            if chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
            } else if chars[i] == ','
                && chars[i - 1].is_ascii_digit()
                && i + 3 < chars.len()
                && chars[i + 1].is_ascii_digit()
                && chars[i + 2].is_ascii_digit()
                && chars[i + 3].is_ascii_digit()
                && (i + 4 == chars.len() || !chars[i + 4].is_ascii_digit())
            {
                i += 1;
            } else {
                break;
            }
        }
        if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
            num.push('.');
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
            }
        }
        out.push(num);
    }
    out
}

fn kept_lines<'a>(current: &'a [ItemRequest], item_id: &str, field: &str) -> &'a [String] {
    let Some(item) = current.iter().find(|item| item.item_id == item_id) else {
        return &[];
    };
    match field {
        "completed" => &item.completed,
        "next" => &item.next,
        "future" => &item.future,
        _ => &[],
    }
}

fn first_rejection(
    patch: &PatchLine,
    facts: &[SourceFact],
    rules: &[ai::Note],
    current: &[ItemRequest],
) -> Option<String> {
    const PROSE: [&str; 3] = ["completed", "next", "future"];
    if !PROSE.contains(&patch.field.as_str()) {
        return Some("the model may not set typed fields".into());
    }
    if patch.lines.iter().all(|l| l.trim().is_empty()) {
        return Some("an empty field would erase the owner's text".into());
    }
    // Every current line stays, in order, and new lines only follow them.
    let kept = kept_lines(current, &patch.item_id, &patch.field);
    if !patch.lines.starts_with(kept) {
        return Some("would drop the owner's text".into());
    }
    for id in &patch.source_ids {
        if !facts.iter().any(|f| &f.id == id) {
            return Some("cites an unknown source".into());
        }
    }
    for note in rules {
        if let Some(target) = note.body.strip_prefix("drop:") {
            if target.trim() == patch.item_id.as_str() {
                return Some("dropped by the owner".into());
            }
        }
    }
    for note in rules {
        if let Some(phrase) = note.body.strip_prefix("never_say:") {
            let phrase = phrase.trim().to_lowercase();
            if !phrase.is_empty()
                && patch.lines.iter().any(|l| l.to_lowercase().contains(&phrase))
            {
                return Some("uses a phrase the owner banned".into());
            }
        }
    }
    for text in &patch.lines {
        if text.contains('\u{2013}') || text.contains('\u{2014}') {
            return Some("dash style".into());
        }
    }
    let allowed: Vec<String> = facts
        .iter()
        .filter(|f| patch.source_ids.contains(&f.id))
        .flat_map(|f| numbers(&f.text))
        .collect();
    // Owner lines are already on the draft. Only lines the model added can invent a number.
    for text in patch.lines.iter().skip(kept.len()) {
        if numbers(text).iter().any(|n| !allowed.contains(n)) {
            return Some("a number not in its sources".into());
        }
    }
    None
}

/// Accept a patch only when every line clears the owner's rules. A rejected
/// line raises one flag; an accepted line gets the open glossary applied.
pub fn validate(
    conn: &Connection,
    patch: &[PatchLine],
    facts: &[SourceFact],
    current: &[ItemRequest],
    now: &str,
) -> Result<Vec<Verdict>, AiError> {
    let rules = ai::open_notes(conn, Some("rule"))?;
    let mut out = Vec::with_capacity(patch.len());
    for line in patch {
        // Unknown ids never reach the field checks, and raise no flag: no item shows it.
        let known = current.iter().any(|item| item.item_id == line.item_id);
        let reason = if known {
            first_rejection(line, facts, &rules, current)
        } else {
            Some("item not in this request".into())
        };
        match reason {
            Some(reason) => {
                if known {
                    ai::raise_flag(conn, &line.item_id, &line.field, &reason, now)?;
                }
                out.push(Verdict::Rejected {
                    item_id: line.item_id.clone(),
                    field: line.field.clone(),
                    reason,
                });
            }
            None => {
                let mut accepted = line.clone();
                accepted.lines = line
                    .lines
                    .iter()
                    .map(|l| ai::apply_glossary(conn, l))
                    .collect::<Result<Vec<_>, _>>()?;
                out.push(Verdict::Accepted(accepted));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{add_note, open_flags};

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn
    }

    fn fact(id: &str, text: &str) -> SourceFact {
        SourceFact {
            id: id.into(),
            text: text.into(),
        }
    }

    fn line(item_id: &str, field: &str, lines: &[&str], sources: &[&str]) -> PatchLine {
        PatchLine {
            item_id: item_id.into(),
            field: field.into(),
            lines: lines.iter().map(|l| l.to_string()).collect(),
            source_ids: sources.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn present(item_id: &str) -> ItemRequest {
        ItemRequest {
            item_id: item_id.into(),
            facts: vec![],
            completed: vec![],
            next: vec![],
            future: vec![],
        }
    }

    struct StubModel;

    impl Model for StubModel {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            let item = &req.items[0];
            let mut lines = item.completed.clone();
            lines.extend(item.facts.iter().map(|fact| fact.text.clone()));
            Ok(vec![
                line("ghost-item", "completed", &["not this item"], &[]),
                PatchLine {
                    item_id: item.item_id.clone(),
                    field: "completed".into(),
                    lines,
                    source_ids: item.facts.iter().map(|fact| fact.id.clone()).collect(),
                },
            ])
        }
    }

    #[test]
    fn fake_model_drafts_and_validates() {
        let conn = db();
        let req = DraftRequest {
            items: vec![
                ItemRequest {
                    item_id: "pay-1".into(),
                    facts: vec![fact("ev-12", "PAY-104 moved from In progress to Done")],
                    completed: vec!["kept".into()],
                    next: vec![],
                    future: vec![],
                },
                ItemRequest {
                    item_id: "pay-2".into(),
                    facts: vec![fact("ev-13", "Login page shipped")],
                    completed: vec![],
                    next: vec![],
                    future: vec![],
                },
            ],
        };
        let patch = FakeModel.draft(&req).unwrap();
        assert_eq!(
            patch,
            vec![
                PatchLine {
                    item_id: "pay-1".into(),
                    field: "completed".into(),
                    lines: vec![
                        "kept".into(),
                        "PAY-104 moved from In progress to Done".into(),
                    ],
                    source_ids: vec!["ev-12".into()],
                },
                PatchLine {
                    item_id: "pay-2".into(),
                    field: "completed".into(),
                    lines: vec!["Login page shipped".into()],
                    source_ids: vec!["ev-13".into()],
                },
            ]
        );
        let facts: Vec<SourceFact> = req.items.iter().flat_map(|item| item.facts.clone()).collect();
        let verdicts = validate(&conn, &patch, &facts, &req.items, "t1").unwrap();
        assert_eq!(verdicts.len(), 2);
        assert!(verdicts.iter().all(|v| matches!(v, Verdict::Accepted(_))));
    }

    #[test]
    fn invented_number_is_rejected_and_flags_once() {
        let conn = db();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![line("pay-1", "completed", &["Done in 3 days"], &["ev-1"])];
        let first = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            first,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "a number not in its sources".into(),
            }]
        );
        let second = validate(&conn, &patch, &facts, &[present("pay-1")], "t2").unwrap();
        assert_eq!(second, first);
        assert_eq!(open_flags(&conn).unwrap().len(), 1);
    }

    #[test]
    fn thousands_commas_only_join_digits() {
        assert_eq!(
            numbers("12,500 and 1,2 and Q3,Q4 and 3.5"),
            vec!["12500", "1", "2", "3", "4", "3.5"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>()
        );
    }

    #[test]
    fn comma_boundary_number_must_be_in_sources() {
        let conn = db();
        let facts = vec![fact("ev-1", "12 shipped")];
        let patch = vec![line("pay-1", "completed", &["1,2 shipped"], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "a number not in its sources".into(),
            }]
        );
    }

    #[test]
    fn empty_field_is_rejected_and_flags() {
        let conn = db();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![line("pay-1", "completed", &[], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "an empty field would erase the owner's text".into(),
            }]
        );
        assert_eq!(open_flags(&conn).unwrap().len(), 1);
    }

    #[test]
    fn typed_field_is_rejected() {
        let conn = db();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![line("pay-1", "pct", &["Login page shipped"], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "pct".into(),
                reason: "the model may not set typed fields".into(),
            }]
        );
    }

    #[test]
    fn drop_rule_skips_only_its_item() {
        let conn = db();
        add_note(&conn, "rule", "drop:pay-1", "t0").unwrap();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![
            line("pay-1", "completed", &["Login page shipped"], &["ev-1"]),
            line("pay-2", "completed", &["Login page shipped"], &["ev-1"]),
        ];
        let verdicts = validate(
            &conn,
            &patch,
            &facts,
            &[present("pay-1"), present("pay-2")],
            "t1",
        )
        .unwrap();
        assert_eq!(
            verdicts[0],
            Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "dropped by the owner".into(),
            }
        );
        assert!(matches!(verdicts[1], Verdict::Accepted(_)));
    }

    #[test]
    fn never_say_rejects_banned_phrase() {
        let conn = db();
        add_note(&conn, "rule", "never_say:on track", "t0").unwrap();
        let facts = vec![fact("ev-1", "Still On Track")];
        let patch = vec![line("pay-1", "next", &["Still On Track"], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "next".into(),
                reason: "uses a phrase the owner banned".into(),
            }]
        );
    }

    #[test]
    fn plain_hyphen_passes_but_dash_style_is_rejected() {
        let conn = db();
        let facts = vec![fact("ev-1", "go-live moved")];
        let good = vec![line("pay-1", "next", &["go-live moved"], &["ev-1"])];
        assert!(matches!(
            validate(&conn, &good, &facts, &[present("pay-1")], "t1").unwrap()[0],
            Verdict::Accepted(_)
        ));
        let bad = vec![line("pay-1", "next", &["Shipping\u{2013}soon"], &["ev-1"])];
        assert_eq!(
            validate(&conn, &bad, &facts, &[present("pay-1")], "t2").unwrap(),
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "next".into(),
                reason: "dash style".into(),
            }]
        );
    }

    #[test]
    fn unknown_source_is_rejected() {
        let conn = db();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![line("pay-1", "completed", &["Login page shipped"], &["ev-9"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "cites an unknown source".into(),
            }]
        );
    }

    #[test]
    fn accepted_lines_get_the_glossary() {
        let conn = db();
        add_note(&conn, "glossary", "Pelix => Felix", "t0").unwrap();
        let facts = vec![fact("ev-1", "Pelix shipped")];
        let patch = vec![line("pay-1", "completed", &["Pelix shipped"], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &[present("pay-1")], "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Accepted(PatchLine {
                item_id: "pay-1".into(),
                field: "completed".into(),
                lines: vec!["Felix shipped".into()],
                source_ids: vec!["ev-1".into()],
            })]
        );
    }

    #[test]
    fn dropping_a_current_line_is_rejected() {
        let conn = db();
        let facts = vec![fact("ev-1", "Login page shipped")];
        let current = vec![ItemRequest {
            item_id: "pay-1".into(),
            facts: facts.clone(),
            completed: vec!["Owner wrote this".into()],
            next: vec![],
            future: vec![],
        }];
        let patch = vec![line("pay-1", "completed", &["Login page shipped"], &["ev-1"])];
        let verdicts = validate(&conn, &patch, &facts, &current, "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "pay-1".into(),
                field: "completed".into(),
                reason: "would drop the owner's text".into(),
            }]
        );
    }

    #[test]
    fn ghost_item_is_rejected_and_real_item_is_accepted() {
        let conn = db();
        let req = DraftRequest {
            items: vec![ItemRequest {
                item_id: "pay-1".into(),
                facts: vec![fact("ev-1", "Login page shipped")],
                completed: vec!["kept".into()],
                next: vec![],
                future: vec![],
            }],
        };
        let patch = StubModel.draft(&req).unwrap();
        let facts: Vec<SourceFact> = req.items.iter().flat_map(|item| item.facts.clone()).collect();
        let verdicts = validate(&conn, &patch, &facts, &req.items, "t1").unwrap();
        assert_eq!(
            verdicts[0],
            Verdict::Rejected {
                item_id: "ghost-item".into(),
                field: "completed".into(),
                reason: "item not in this request".into(),
            }
        );
        assert_eq!(
            verdicts[1],
            Verdict::Accepted(PatchLine {
                item_id: "pay-1".into(),
                field: "completed".into(),
                lines: vec!["kept".into(), "Login page shipped".into()],
                source_ids: vec!["ev-1".into()],
            })
        );
        assert_eq!(open_flags(&conn).unwrap().len(), 0);
        let again = validate(&conn, &patch, &facts, &req.items, "t2").unwrap();
        assert_eq!(again, verdicts);
        assert_eq!(open_flags(&conn).unwrap().len(), 0);
    }

    #[test]
    fn patch_only_for_an_unknown_id_accepts_nothing() {
        let conn = db();
        let current = vec![present("pay-1")];
        let facts = vec![fact("ev-1", "Login page shipped")];
        let patch = vec![line(
            "missing",
            "completed",
            &["Login page shipped"],
            &["ev-1"],
        )];
        let verdicts = validate(&conn, &patch, &facts, &current, "t1").unwrap();
        assert_eq!(
            verdicts,
            vec![Verdict::Rejected {
                item_id: "missing".into(),
                field: "completed".into(),
                reason: "item not in this request".into(),
            }]
        );
    }
}
