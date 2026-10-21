//! Decision-tree executor.
//!
//! A tree is the JSON a `CREATE DECISION TREE` body parses into.
//! [`eval_batch`] walks many rows together: every row sitting on the same
//! node is one ask or one choose, with duplicate inputs at that node
//! served from a per-node memo. The next level does not start until this
//! level has branched. Leaves are text. A boolean leaf is `true` or
//! `false`.
//!
//! The planner lifts a scan that projects `rvbbit.decide` onto a Result
//! so this walk runs once per batch. A MEANS qual on that same Result
//! keeps the semantic batcher, and `decide` then runs per row inside it.
//!
//! Stacking is the node's `using` list plus `combine` (`mean` or `vote`).
//! The walker forwards both to [`Decisions`] and branches on the bool that
//! implementation returns. It does not call a model itself.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct AskReq {
    pub node: String,
    pub question: String,
    pub inputs: Vec<String>,
    pub models: Vec<String>,
    /// `mean` or `vote`. `mean` compares the score with `threshold`.
    /// `vote` is the backend's majority bool.
    pub combine: String,
    pub threshold: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AskAns {
    pub yes: bool,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceReq {
    pub node: String,
    pub question: String,
    pub inputs: Vec<String>,
    pub labels: Vec<String>,
    pub meanings: Vec<String>,
    pub models: Vec<String>,
    pub combine: String,
}

pub trait Decisions {
    fn ask(&mut self, req: &AskReq) -> Result<Vec<AskAns>, String>;
    fn choose(&mut self, req: &ChoiceReq) -> Result<Vec<String>, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalReport {
    pub values: Vec<String>,
    /// Ask or choose invocations. A memo that covers the whole group
    /// is not one of these.
    pub node_calls: u32,
    /// Inputs at a node that were already answered in this batch.
    pub memo_hits: u32,
    /// Wavefront rounds. A leaf-only tree is 0.
    pub levels: u32,
}

#[derive(Clone)]
enum Node {
    Ask {
        id: String,
        question: String,
        threshold: f64,
        using: Vec<String>,
        combine: String,
        yes: usize,
        no: usize,
    },
    Choose {
        id: String,
        question: String,
        using: Vec<String>,
        combine: String,
        arms: Vec<(String, String, usize)>,
    },
    Leaf {
        value: String,
    },
}

struct Built {
    nodes: Vec<Node>,
    root: usize,
}

pub fn eval_batch(
    root: &Value,
    inputs: &[String],
    decisions: &mut impl Decisions,
) -> Result<EvalReport, String> {
    let built = build(root)?;
    let mut at = vec![built.root; inputs.len()];
    let mut memo: std::collections::HashMap<(usize, String), Step> = std::collections::HashMap::new();
    let mut node_calls = 0u32;
    let mut memo_hits = 0u32;
    let mut levels = 0u32;
    for _ in 0..32 {
        let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
        for (row, node) in at.iter().enumerate() {
            if !matches!(built.nodes[*node], Node::Leaf { .. }) {
                groups.entry(*node).or_default().push(row);
            }
        }
        if groups.is_empty() {
            break;
        }
        levels += 1;
        for (node_idx, rows) in groups {
            let mut order: Vec<String> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for row in &rows {
                let input = &inputs[*row];
                if memo.contains_key(&(node_idx, input.clone())) {
                    memo_hits += 1;
                    continue;
                }
                if seen.insert(input.clone()) {
                    order.push(input.clone());
                } else {
                    memo_hits += 1;
                }
            }
            if !order.is_empty() {
                node_calls += 1;
                let answers = ask_node(&built.nodes[node_idx], &order, decisions)?;
                if answers.len() != order.len() {
                    return Err(format!(
                        "node {} returned {} answers for {} inputs",
                        node_name(&built.nodes[node_idx]),
                        answers.len(),
                        order.len()
                    ));
                }
                for (input, step) in order.into_iter().zip(answers) {
                    memo.insert((node_idx, input), step);
                }
            }
            for row in rows {
                let step = memo
                    .get(&(node_idx, inputs[row].clone()))
                    .cloned()
                    .ok_or_else(|| format!("node {} has no answer", node_name(&built.nodes[node_idx])))?;
                at[row] = match step {
                    Step::Go(next) => next,
                    Step::Done(value) => {
                        let _ = value;
                        node_idx
                    }
                };
            }
        }
    }
    let mut values = Vec::with_capacity(inputs.len());
    for (row, node) in at.iter().enumerate() {
        match &built.nodes[*node] {
            Node::Leaf { value } => values.push(value.clone()),
            _ => {
                return Err(format!(
                    "row {row} stopped on {} before a leaf",
                    node_name(&built.nodes[*node])
                ))
            }
        }
    }
    Ok(EvalReport {
        values,
        node_calls,
        memo_hits,
        levels,
    })
}

#[derive(Clone)]
enum Step {
    Go(usize),
    Done(String),
}

fn ask_node(node: &Node, inputs: &[String], decisions: &mut impl Decisions) -> Result<Vec<Step>, String> {
    match node {
        Node::Ask {
            id,
            question,
            threshold,
            using,
            combine,
            yes,
            no,
        } => {
            let ans = decisions.ask(&AskReq {
                node: id.clone(),
                question: question.clone(),
                inputs: inputs.to_vec(),
                models: using.clone(),
                combine: combine.clone(),
                threshold: *threshold,
            })?;
            ans.into_iter()
                .map(|a| Ok(Step::Go(if a.yes { *yes } else { *no })))
                .collect()
        }
        Node::Choose {
            id,
            question,
            using,
            combine,
            arms,
        } => {
            let labels: Vec<String> = arms.iter().map(|(l, _, _)| l.clone()).collect();
            let meanings: Vec<String> = arms.iter().map(|(_, m, _)| m.clone()).collect();
            let chosen = decisions.choose(&ChoiceReq {
                node: id.clone(),
                question: question.clone(),
                inputs: inputs.to_vec(),
                labels,
                meanings,
                models: using.clone(),
                combine: combine.clone(),
            })?;
            chosen
                .into_iter()
                .map(|label| {
                    arms.iter()
                        .find(|(name, _, _)| name == &label)
                        .map(|(_, _, next)| Step::Go(*next))
                        .ok_or_else(|| format!("choose {id} has no branch for {label}"))
                })
                .collect()
        }
        Node::Leaf { value } => Ok(vec![Step::Done(value.clone()); inputs.len()]),
    }
}

fn node_name(node: &Node) -> &str {
    match node {
        Node::Ask { id, .. } | Node::Choose { id, .. } => id,
        Node::Leaf { .. } => "leaf",
    }
}

fn build(root: &Value) -> Result<Built, String> {
    let mut nodes = Vec::new();
    let root = build_into(root, &mut nodes, "n0")?;
    Ok(Built { nodes, root })
}

fn build_into(v: &Value, nodes: &mut Vec<Node>, fallback_id: &str) -> Result<usize, String> {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let id = v
        .get("id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_id)
        .to_string();
    match kind {
        "leaf" => {
            nodes.push(Node::Leaf {
                value: leaf_text(v)?,
            });
            Ok(nodes.len() - 1)
        }
        "ask" => {
            let question = v
                .get("question")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("ask {id} has no question"))?
                .to_string();
            let threshold = v.get("threshold").and_then(|n| n.as_f64()).unwrap_or(0.5);
            if !(0.0..=1.0).contains(&threshold) {
                return Err(format!("ask {id} threshold {threshold} is outside [0, 1]"));
            }
            let idx = nodes.len();
            nodes.push(Node::Leaf {
                value: String::new(),
            });
            let yes = build_into(
                v.get("yes").ok_or_else(|| format!("ask {id} has no yes"))?,
                nodes,
                &format!("{id}.y"),
            )?;
            let no = build_into(
                v.get("no").ok_or_else(|| format!("ask {id} has no no"))?,
                nodes,
                &format!("{id}.n"),
            )?;
            nodes[idx] = Node::Ask {
                id,
                question,
                threshold,
                using: string_list(v.get("using")),
                combine: combine_of(v),
                yes,
                no,
            };
            Ok(idx)
        }
        "choose" => {
            let question = v
                .get("question")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("choose {id} has no question"))?
                .to_string();
            let when = v
                .get("when")
                .and_then(|w| w.as_array())
                .ok_or_else(|| format!("choose {id} has no when"))?;
            if when.len() < 2 {
                return Err(format!("choose {id} needs at least two branches"));
            }
            let idx = nodes.len();
            nodes.push(Node::Leaf {
                value: String::new(),
            });
            let mut arms = Vec::new();
            for (i, arm) in when.iter().enumerate() {
                let label = arm
                    .get("label")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("choose {id} branch {i} has no label"))?
                    .to_string();
                let meaning = arm.get("meaning").and_then(|s| s.as_str()).unwrap_or("").to_string();
                let then = build_into(
                    arm.get("then").ok_or_else(|| format!("choose {id} branch {label} has no then"))?,
                    nodes,
                    &format!("{id}.w{i}"),
                )?;
                arms.push((label, meaning, then));
            }
            nodes[idx] = Node::Choose {
                id,
                question,
                using: string_list(v.get("using")),
                combine: combine_of(v),
                arms,
            };
            Ok(idx)
        }
        other => Err(format!("unknown decision node {other}")),
    }
}

fn leaf_text(v: &Value) -> Result<String, String> {
    if v.get("type").and_then(|t| t.as_str()) == Some("bool") || v.get("value").map(|x| x.is_boolean()).unwrap_or(false)
    {
        let bit = v
            .get("value")
            .and_then(|b| b.as_bool())
            .ok_or_else(|| "boolean leaf has no value".to_string())?;
        return Ok(if bit { "true" } else { "false" }.into());
    }
    v.get("label")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "leaf has no label".into())
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn combine_of(v: &Value) -> String {
    match v.get("combine").and_then(|s| s.as_str()) {
        Some("vote") => "vote".into(),
        _ => "mean".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fake {
        ask_calls: u32,
        choose_calls: u32,
        last_ask: usize,
        last_models: Vec<String>,
        last_combine: String,
    }

    impl Decisions for Fake {
        fn ask(&mut self, req: &AskReq) -> Result<Vec<AskAns>, String> {
            self.ask_calls += 1;
            self.last_ask = req.inputs.len();
            if self.ask_calls == 1 {
                self.last_models = req.models.clone();
                self.last_combine = req.combine.clone();
            }
            Ok(req
                .inputs
                .iter()
                .map(|text| {
                    let yes = text.contains("saw")
                        || text.contains("hairy")
                        || text.contains("upright")
                        || text.contains("knock");
                    AskAns {
                        yes,
                        score: if yes { 0.9 } else { 0.2 },
                    }
                })
                .collect())
        }

        fn choose(&mut self, req: &ChoiceReq) -> Result<Vec<String>, String> {
            self.choose_calls += 1;
            let _ = req.models;
            Ok(req
                .inputs
                .iter()
                .map(|text| {
                    if text.contains("track") || text.contains("knock") {
                        "class b"
                    } else if text.contains("told me") {
                        "class c"
                    } else {
                        "class a"
                    }
                    .into()
                })
                .collect())
        }
    }

    fn bigfoot() -> Value {
        json!({
            "kind": "ask",
            "id": "n1",
            "question": "credible?",
            "threshold": 0.5,
            "using": ["typesafe/high", "typesafe/low"],
            "combine": "vote",
            "yes": {
                "kind": "choose",
                "id": "n2",
                "question": "class?",
                "when": [
                    {"label": "class a", "meaning": "saw it", "then": {
                        "kind": "ask",
                        "id": "n3",
                        "question": "follow up?",
                        "yes": {"kind": "leaf", "type": "bool", "value": true},
                        "no": {"kind": "leaf", "type": "bool", "value": false}
                    }},
                    {"label": "class b", "meaning": "tracks", "then": {"kind": "leaf", "label": "review"}},
                    {"label": "class c", "meaning": "secondhand", "then": {"kind": "leaf", "label": "archive"}}
                ]
            },
            "no": {"kind": "leaf", "label": "not a sighting"}
        })
    }

    #[test]
    fn one_call_per_node_level_and_a_per_node_memo() {
        let mut fake = Fake {
            ask_calls: 0,
            choose_calls: 0,
            last_ask: 0,
            last_models: Vec::new(),
            last_combine: String::new(),
        };
        let inputs = vec![
            "we saw a hairy upright figure".into(),
            "we saw a hairy upright figure".into(),
            "a deer in the ferns".into(),
            "wood knocks and a track".into(),
        ];
        let report = eval_batch(&bigfoot(), &inputs, &mut fake).unwrap();
        assert_eq!(
            report.values,
            vec![
                "true".to_string(),
                "true".to_string(),
                "not a sighting".to_string(),
                "review".to_string(),
            ]
        );
        // Root ask is one call for the unique inputs (3, not 4).
        // Follow-up ask is a second call, only for the class-a rows.
        assert_eq!(fake.ask_calls, 2);
        assert_eq!(fake.choose_calls, 1);
        assert_eq!(report.node_calls, 3);
        assert!(report.memo_hits >= 1, "duplicate root input is a memo hit");
        assert_eq!(report.levels, 3);
        assert_eq!(fake.last_models, vec!["typesafe/high".to_string(), "typesafe/low".to_string()]);
        assert_eq!(fake.last_combine, "vote");
    }
}
