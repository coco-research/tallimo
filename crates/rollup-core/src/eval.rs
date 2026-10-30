//! Offline score for drafted leadership lines against an owner's final text.
//!
//! Each case asks the model for a draft, keeps only the lines `validate`
//! accepts (the same call `airun` makes, on a rule-free database), and
//! compares those lines with `owner_final` for the same item and field.

use crate::draft::{self, DraftRequest, ItemRequest, Model, SourceFact, Verdict};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// One eval case: the draft the model sees, and the text a careful owner sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub id: String,
    pub family: String,
    pub request: DraftRequest,
    pub owner_final: BTreeMap<String, FinalFields>,
}

/// The three prose fields on one item after the owner edited the draft.
#[derive(Debug, Clone, PartialEq)]
pub struct FinalFields {
    pub completed: Vec<String>,
    pub next: Vec<String>,
    pub future: Vec<String>,
}

/// Counts for one family. `mean_edit_distance` averages the normalised
/// distances of accepted lines that were not kept verbatim. No such line
/// means 0. A kept line is an exact match and is not part of that mean.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyScore {
    pub cases: usize,
    pub drafted: usize,
    pub kept: usize,
    pub keep_rate: f64,
    pub mean_edit_distance: f64,
    pub rejected: usize,
    pub failed: usize,
}

/// Totals across every case, plus the same numbers per family.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub cases: usize,
    pub drafted: usize,
    pub kept: usize,
    pub keep_rate: f64,
    pub mean_edit_distance: f64,
    pub rejected: usize,
    pub failed: usize,
    pub per_family: BTreeMap<String, FamilyScore>,
}

#[derive(Default)]
struct Bucket {
    cases: usize,
    drafted: usize,
    kept: usize,
    rejected: usize,
    failed: usize,
    edit_sum: f64,
    edit_n: usize,
}

impl Bucket {
    fn add(&mut self, other: &Bucket) {
        self.cases += other.cases;
        self.drafted += other.drafted;
        self.kept += other.kept;
        self.rejected += other.rejected;
        self.failed += other.failed;
        self.edit_sum += other.edit_sum;
        self.edit_n += other.edit_n;
    }

    fn finish(&self) -> FamilyScore {
        let keep_rate = if self.drafted == 0 {
            0.0
        } else {
            self.kept as f64 / self.drafted as f64
        };
        // Kept lines are exact, so they are counted in keep_rate and left
        // out of the mean. An empty owner field is distance 1.
        let mean_edit_distance = if self.edit_n == 0 {
            0.0
        } else {
            self.edit_sum / self.edit_n as f64
        };
        FamilyScore {
            cases: self.cases,
            drafted: self.drafted,
            kept: self.kept,
            keep_rate,
            mean_edit_distance,
            rejected: self.rejected,
            failed: self.failed,
        }
    }
}

impl Score {
    pub fn to_value(&self) -> Value {
        let mut value = family_value(&FamilyScore {
            cases: self.cases,
            drafted: self.drafted,
            kept: self.kept,
            keep_rate: self.keep_rate,
            mean_edit_distance: self.mean_edit_distance,
            rejected: self.rejected,
            failed: self.failed,
        });
        let families: serde_json::Map<String, Value> = self
            .per_family
            .iter()
            .map(|(name, score)| (name.clone(), family_value(score)))
            .collect();
        value["per_family"] = Value::Object(families);
        value
    }
}

fn family_value(score: &FamilyScore) -> Value {
    serde_json::json!({
        "cases": score.cases,
        "drafted": score.drafted,
        "kept": score.kept,
        "keep_rate": score.keep_rate,
        "mean_edit_distance": score.mean_edit_distance,
        "rejected": score.rejected,
        "failed": score.failed,
    })
}

/// Normalised Levenshtein distance over Unicode scalar values, in 0..=1.
/// Equal strings are 0. The distance is divided by the longer length.
fn normalised_levenshtein(left: &str, right: &str) -> f64 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let width = left.len().max(right.len());
    if width == 0 {
        return 0.0;
    }
    let mut prev: Vec<usize> = (0..=right.len()).collect();
    let mut curr = vec![0; right.len() + 1];
    for (i, ca) in left.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in right.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[right.len()] as f64 / width as f64
}

fn eval_conn() -> Result<Connection, String> {
    let conn = Connection::open_in_memory().map_err(|err| format!("eval db: {err}"))?;
    conn.execute_batch(crate::store::SCHEMA)
        .map_err(|err| format!("eval schema: {err}"))?;
    Ok(conn)
}

fn owner_lines<'a>(case: &'a Case, item_id: &str, field: &str) -> &'a [String] {
    let Some(item) = case.owner_final.get(item_id) else {
        return &[];
    };
    match field {
        "completed" => &item.completed,
        "next" => &item.next,
        "future" => &item.future,
        _ => &[],
    }
}

fn request_lines<'a>(case: &'a Case, item_id: &str, field: &str) -> &'a [String] {
    let Some(item) = case.request.items.iter().find(|item| item.item_id == item_id) else {
        return &[];
    };
    match field {
        "completed" => &item.completed,
        "next" => &item.next,
        "future" => &item.future,
        _ => &[],
    }
}

fn score_case(
    conn: &Connection,
    case: &Case,
    model: &dyn Model,
    bucket: &mut Bucket,
) -> Result<(), String> {
    let patch = match model.draft(&case.request) {
        Ok(patch) => patch,
        Err(_) => {
            bucket.failed += 1;
            return Ok(());
        }
    };
    let facts = crate::airun::flat_facts(&case.request.items);
    let verdicts = match draft::validate(conn, &patch, &facts, &case.request.items, "eval") {
        Ok(verdicts) => verdicts,
        Err(crate::ai::AiError::Invalid(_)) => {
            bucket.failed += 1;
            return Ok(());
        }
        Err(crate::ai::AiError::Db(msg)) => return Err(msg),
    };
    for verdict in verdicts {
        let line = match verdict {
            Verdict::Accepted(line) => line,
            Verdict::Rejected { .. } => {
                bucket.rejected += 1;
                continue;
            }
        };
        let owner = owner_lines(case, &line.item_id, &line.field);
        // The patch repeats the item's current lines. A line that still equals
        // the current line at the same index is not text the model added.
        let current = request_lines(case, &line.item_id, &line.field);
        for (index, drafted) in line.lines.iter().enumerate() {
            if current.get(index).is_some_and(|have| have == drafted) {
                continue;
            }
            bucket.drafted += 1;
            if owner.iter().any(|have| have == drafted) {
                bucket.kept += 1;
                continue;
            }
            let distance = if owner.is_empty() {
                1.0
            } else {
                owner
                    .iter()
                    .map(|have| normalised_levenshtein(drafted, have))
                    .fold(f64::INFINITY, f64::min)
            };
            bucket.edit_sum += distance;
            bucket.edit_n += 1;
        }
    }
    Ok(())
}

/// Draft every case, keep accepted lines only, and score them against the owner.
/// A model `Err`, or `AiError::Invalid` from `validate`, increments `failed` and
/// skips that case. `AiError::Db` stops the score. Current lines repeated on an
/// accepted patch are not drafted.
pub fn score(cases: &[Case], model: &dyn Model) -> Result<Score, String> {
    let conn = eval_conn()?;
    let mut total = Bucket::default();
    let mut families: BTreeMap<String, Bucket> = BTreeMap::new();
    for case in cases {
        let mut bucket = Bucket {
            cases: 1,
            ..Bucket::default()
        };
        score_case(&conn, case, model, &mut bucket).map_err(|err| format!("{}: {err}", case.id))?;
        total.add(&bucket);
        families.entry(case.family.clone()).or_default().add(&bucket);
    }
    let totals = total.finish();
    Ok(Score {
        cases: totals.cases,
        drafted: totals.drafted,
        kept: totals.kept,
        keep_rate: totals.keep_rate,
        mean_edit_distance: totals.mean_edit_distance,
        rejected: totals.rejected,
        failed: totals.failed,
        per_family: families
            .into_iter()
            .map(|(name, bucket)| (name, bucket.finish()))
            .collect(),
    })
}

/// Read `case-*.json` from `dir`, sorted by file name.
pub fn load_cases(dir: &Path) -> Result<Vec<Case>, String> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))? {
        let path = entry
            .map_err(|err| format!("{}: {err}", dir.display()))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    let mut cases = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        cases.push(parse_case(&value).map_err(|err| format!("{}: {err}", path.display()))?);
    }
    Ok(cases)
}

fn need_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn need_strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    let list = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{key} must be an array"))?;
    list.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key} must be strings"))
        })
        .collect()
}

fn parse_item(value: &Value) -> Result<ItemRequest, String> {
    let facts = value
        .get("facts")
        .and_then(Value::as_array)
        .ok_or_else(|| "facts must be an array".to_string())?;
    let mut parsed = Vec::with_capacity(facts.len());
    for fact in facts {
        parsed.push(SourceFact {
            id: need_str(fact, "id")?.to_string(),
            text: need_str(fact, "text")?.to_string(),
        });
    }
    Ok(ItemRequest {
        item_id: need_str(value, "item_id")?.to_string(),
        facts: parsed,
        completed: need_strings(value, "completed")?,
        next: need_strings(value, "next")?,
        future: need_strings(value, "future")?,
    })
}

fn parse_final(value: &Value) -> Result<FinalFields, String> {
    Ok(FinalFields {
        completed: need_strings(value, "completed")?,
        next: need_strings(value, "next")?,
        future: need_strings(value, "future")?,
    })
}

fn parse_case(value: &Value) -> Result<Case, String> {
    let id = need_str(value, "id")?.to_string();
    let items = value
        .get("request")
        .and_then(|request| request.get("items"))
        .and_then(Value::as_array)
        .ok_or_else(|| "request.items must be an array".to_string())?;
    let items = items.iter().map(parse_item).collect::<Result<Vec<_>, _>>()?;
    let owners = value
        .get("owner_final")
        .and_then(Value::as_object)
        .ok_or_else(|| "owner_final must be an object".to_string())?;
    let mut owner_final = BTreeMap::new();
    for (item_id, body) in owners {
        if !items.iter().any(|item| item.item_id == *item_id) {
            return Err(format!(
                "{id}: owner_final item {item_id} is not in request.items"
            ));
        }
        owner_final.insert(item_id.clone(), parse_final(body)?);
    }
    Ok(Case {
        id,
        family: need_str(value, "family")?.to_string(),
        request: DraftRequest { items },
        owner_final,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::PatchLine;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(normalised_levenshtein("", ""), 0.0);
        assert_eq!(normalised_levenshtein("cat", "cat"), 0.0);
        assert_eq!(normalised_levenshtein("", "cat"), 1.0);
        assert_eq!(normalised_levenshtein("cat", ""), 1.0);
        assert!((normalised_levenshtein("cat", "bat") - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(normalised_levenshtein("kit", "kite"), 0.25);
        assert!((normalised_levenshtein("kitten", "sitting") - 3.0 / 7.0).abs() < 1e-12);
    }

    struct TwoLine;

    impl Model for TwoLine {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            let item = &req.items[0];
            let text = if item.item_id == "kept-item" {
                "Ship it."
            } else {
                "Ship it!"
            };
            Ok(vec![PatchLine {
                item_id: item.item_id.clone(),
                field: "completed".to_string(),
                lines: vec![text.to_string()],
                source_ids: vec![item.facts[0].id.clone()],
            }])
        }
    }

    fn hand_case(id: &str, family: &str, item_id: &str) -> Case {
        let mut owner_final = BTreeMap::new();
        owner_final.insert(
            item_id.to_string(),
            FinalFields {
                completed: vec!["Ship it.".to_string()],
                next: vec![],
                future: vec![],
            },
        );
        Case {
            id: id.to_string(),
            family: family.to_string(),
            request: DraftRequest {
                items: vec![ItemRequest {
                    item_id: item_id.to_string(),
                    facts: vec![SourceFact {
                        id: "ev-1".to_string(),
                        text: "Ship it.".to_string(),
                    }],
                    completed: vec![],
                    next: vec![],
                    future: vec![],
                }],
            },
            owner_final,
        }
    }

    #[test]
    fn one_kept_and_one_edited_line() {
        let cases = vec![
            hand_case("case-kept", "status", "kept-item"),
            hand_case("case-edited", "tone", "edited-item"),
        ];
        let scored = score(&cases, &TwoLine).expect("score");
        // "Ship it!" vs "Ship it." is one substitution over 8 chars.
        assert_eq!(scored.cases, 2);
        assert_eq!(scored.drafted, 2);
        assert_eq!(scored.kept, 1);
        assert_eq!(scored.keep_rate, 0.5);
        assert_eq!(scored.mean_edit_distance, 0.125);
        assert_eq!(scored.rejected, 0);
        assert_eq!(scored.failed, 0);
        assert_eq!(scored.per_family["status"].keep_rate, 1.0);
        assert_eq!(scored.per_family["status"].mean_edit_distance, 0.0);
        assert_eq!(scored.per_family["tone"].kept, 0);
        assert_eq!(scored.per_family["tone"].mean_edit_distance, 0.125);
    }

    struct Failing;

    impl Model for Failing {
        fn draft(&self, _: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            Err("model down".into())
        }
    }

    #[test]
    fn failing_model_counts_as_failed_not_drafted() {
        let scored = score(
            &[hand_case("case-fail", "status", "kept-item")],
            &Failing,
        )
        .expect("score");
        assert_eq!(scored.failed, 1);
        assert_eq!(scored.drafted, 0);
        assert_eq!(scored.kept, 0);
        assert_eq!(scored.per_family["status"].failed, 1);
        assert_eq!(scored.per_family["status"].drafted, 0);
    }

    struct AppendOne;

    impl Model for AppendOne {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            let item = &req.items[0];
            let mut lines = item.completed.clone();
            lines.push("Ship it.".to_string());
            Ok(vec![PatchLine {
                item_id: item.item_id.clone(),
                field: "completed".to_string(),
                lines,
                source_ids: vec![item.facts[0].id.clone()],
            }])
        }
    }

    #[test]
    fn current_lines_are_not_counted_as_drafted() {
        let mut owner_final = BTreeMap::new();
        owner_final.insert(
            "item".to_string(),
            FinalFields {
                completed: vec![
                    "Already there.".to_string(),
                    "Still there.".to_string(),
                    "Ship it.".to_string(),
                ],
                next: vec![],
                future: vec![],
            },
        );
        let case = Case {
            id: "case-prefix".to_string(),
            family: "status".to_string(),
            request: DraftRequest {
                items: vec![ItemRequest {
                    item_id: "item".to_string(),
                    facts: vec![SourceFact {
                        id: "ev-1".to_string(),
                        text: "Ship it.".to_string(),
                    }],
                    completed: vec!["Already there.".to_string(), "Still there.".to_string()],
                    next: vec![],
                    future: vec![],
                }],
            },
            owner_final,
        };
        let scored = score(&[case], &AppendOne).expect("score");
        assert_eq!(scored.drafted, 1);
        assert_eq!(scored.kept, 1);
        assert_eq!(scored.failed, 0);
    }

    struct InvalidThenKept;

    impl Model for InvalidThenKept {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            let item = &req.items[0];
            if item.item_id == "bad-item" {
                return Ok(vec![PatchLine {
                    item_id: item.item_id.clone(),
                    field: String::new(),
                    lines: vec!["nope".to_string()],
                    source_ids: vec![],
                }]);
            }
            Ok(vec![PatchLine {
                item_id: item.item_id.clone(),
                field: "completed".to_string(),
                lines: vec!["Ship it.".to_string()],
                source_ids: vec![item.facts[0].id.clone()],
            }])
        }
    }

    #[test]
    fn invalid_patch_fails_one_case_and_scoring_continues() {
        let cases = vec![
            hand_case("case-bad", "status", "bad-item"),
            hand_case("case-ok", "tone", "kept-item"),
        ];
        let scored = score(&cases, &InvalidThenKept).expect("score");
        assert_eq!(scored.failed, 1);
        assert_eq!(scored.cases, 2);
        assert_eq!(scored.drafted, 1);
        assert_eq!(scored.kept, 1);
        assert_eq!(scored.per_family["status"].failed, 1);
        assert_eq!(scored.per_family["status"].drafted, 0);
        assert_eq!(scored.per_family["tone"].drafted, 1);
        assert_eq!(scored.per_family["tone"].failed, 0);
    }

    #[test]
    fn owner_final_item_must_be_on_the_request() {
        let value = serde_json::json!({
            "id": "case-007",
            "family": "status",
            "request": {
                "items": [{
                    "item_id": "release-schedule",
                    "facts": [{"id": "ev-7", "text": "Release schedule moved."}],
                    "completed": [],
                    "next": [],
                    "future": []
                }]
            },
            "owner_final": {
                "not-on-the-request": {
                    "completed": ["The release schedule is in progress."],
                    "next": [],
                    "future": []
                }
            }
        });
        let err = parse_case(&value).expect_err("owner item");
        assert_eq!(
            err,
            "case-007: owner_final item not-on-the-request is not in request.items"
        );
    }

    #[test]
    fn load_cases_reads_dev_dir() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/eval/dev");
        let cases = load_cases(&dir).expect("dev cases");
        assert_eq!(cases.len(), 40);
        assert_eq!(cases[0].id, "case-001");
        assert_eq!(cases[39].id, "case-040");
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for case in &cases {
            assert!(!case.request.items.is_empty(), "{}", case.id);
            assert!(!case.owner_final.is_empty(), "{}", case.id);
            *counts.entry(case.family.as_str()).or_insert(0) += 1;
        }
        for family in [
            "status",
            "number",
            "dropped-owner-text",
            "tone",
            "glossary",
            "wrong-item",
        ] {
            assert!(counts.get(family).copied().unwrap_or(0) >= 5, "{family}");
        }
    }
}
