use serde_json::{json, Value};

use crate::types::{Config, Event, Finding, Pr};

pub fn run(cfg: Config, pr: Pr, tx: std::sync::mpsc::Sender<Event>) {
    let defs = tools();

    let mut user = format!("PR #{}: {}\n\n", pr.number, pr.title);
    if !pr.body.trim().is_empty() {
        user.push_str(&pr.body);
        user.push_str("\n\n");
    }
    for f in &pr.files {
        user.push_str(&format!("### {}\n{}\n", f.path, f.patch));
    }

    let system = json!({
        "role": "system",
        "content": "You are a code reviewer. You receive a pull request diff. Find real bugs, \
security issues, and missing tests. Do not comment on style. Use read_file when the diff lacks \
context. Call add_comment once per issue with the exact line number in the new file. Call finish \
when done."
    });
    let mut messages: Vec<Value> = vec![
        system,
        json!({"role": "user", "content": user}),
    ];

    for _step in 0..10 {
        let msg = match crate::llm::chat(&cfg, &messages, &defs) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(Event::Error(e));
                return;
            }
        };
        messages.push(msg.clone());

        let calls = match msg["tool_calls"].as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => {
                let text = msg["content"].as_str().unwrap_or("review finished").to_string();
                let _ = tx.send(Event::Done(text));
                return;
            }
        };

        for call in &calls {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            // `id` is a sibling of `function` in the OpenAI tool-call envelope.
            let id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            let args: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("bad arguments: {e}");
                    let _ = tx.send(Event::Log(format!("  {name} {msg}")));
                    messages.push(json!({"role":"tool","tool_call_id": id, "content": msg}));
                    continue;
                }
            };

            let _ = tx.send(Event::Log(format!("→ {} {}", name, summarize(&name, &args))));

            let result: String = match name.as_str() {
                "read_file" => crate::github::get_file(&cfg, &pr, &arg_str(&args, "path"))
                    .unwrap_or_else(|e| format!("error: {e}")),
                "list_files" => crate::github::list_dir(&cfg, &pr, &arg_str(&args, "dir"))
                    .map(|v| v.join("\n"))
                    .unwrap_or_else(|e| format!("error: {e}")),
                "add_comment" => add_comment(&pr, &tx, &args),
                "finish" => {
                    let summary = arg_str(&args, "summary");
                    let _ = tx.send(Event::Done(summary));
                    return;
                }
                _ => format!("unknown tool {name}"),
            };

            messages.push(json!({"role":"tool","tool_call_id": id, "content": result}));
        }
    }

    let _ = tx.send(Event::Done("step limit reached".into()));
}

/// Handle one `add_comment` call. Always returns the tool result string; never fails.
fn add_comment(pr: &Pr, tx: &std::sync::mpsc::Sender<Event>, a: &Value) -> String {
    let path = a["path"].as_str();
    let body = a["body"].as_str();
    let line = a["line"].as_u64().or_else(|| {
        a["line"].as_str().and_then(|s| s.trim().parse::<u64>().ok())
    });
    let sev = match a["severity"].as_str() {
        Some(s @ ("high" | "medium" | "low")) => s.to_string(),
        _ => "medium".to_string(),
    };

    let Some(path) = path else {
        return "bad arguments: path must be a string".into();
    };
    let Some(body) = body else {
        return "bad arguments: body must be a string".into();
    };
    let Some(line) = line else {
        return "bad arguments: line must be an integer".into();
    };
    let line = line.min(u64::from(u32::MAX)) as u32;

    if let Err(e) = validate(pr, path, line) {
        return e;
    }
    let _ = tx.send(Event::Log(format!("  finding {path}:{line} [{sev}]")));
    let _ = tx.send(Event::Finding(Finding {
        path: path.to_string(),
        line,
        severity: sev,
        body: body.to_string(),
        accepted: false,
    }));
    "ok".into()
}

/// Reject paths that are not part of the diff and line numbers that are not
/// present in the new side of that file's patch.
fn validate(pr: &Pr, path: &str, line: u32) -> Result<(), String> {
    let Some(f) = pr.files.iter().find(|f| f.path == path) else {
        return Err(format!("path {path} is not in the diff"));
    };
    if new_lines(&f.patch).contains(&line) {
        Ok(())
    } else {
        Err(format!("line {line} is not in the diff for {path}"))
    }
}

/// Line numbers that exist on the *new* side of a unified diff.
fn new_lines(patch: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut cur: Option<u32> = None;
    for line in patch.lines() {
        if line.starts_with("@@") {
            cur = hunk_start(line);
            continue;
        }
        let Some(n) = cur else { continue };
        // `+` and context lines both exist on the new side, so each consumes a
        // line number. `-` is old-side only; anything else is skipped.
        if line.starts_with('+') || line.starts_with(' ') || line.is_empty() {
            out.push(n);
            cur = Some(n + 1);
        }
    }
    out
}

/// Parse the `+c,d` start of a `@@ -a,b +c,d @@` header.
fn hunk_start(header: &str) -> Option<u32> {
    let after_plus = header.split_once('+')?.1;
    let digits: String = after_plus.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn arg_str(a: &Value, key: &str) -> String {
    a[key].as_str().unwrap_or("").to_string()
}

/// One-line, ~70 char description of a tool call for the event log.
fn summarize(name: &str, a: &Value) -> String {
    let s = match name {
        "read_file" => arg_str(a, "path"),
        "list_files" => arg_str(a, "dir"),
        "add_comment" => format!(
            "{}:{} [{}] {}",
            arg_str(a, "path"),
            a["line"].as_u64().map(|n| n.to_string()).unwrap_or_default(),
            arg_str(a, "severity"),
            arg_str(a, "body")
        ),
        "finish" => arg_str(a, "summary"),
        _ => String::new(),
    };
    let s = s.replace(['\r', '\n'], " ");
    if s.chars().count() > 70 {
        let head: String = s.chars().take(67).collect();
        format!("{head}...")
    } else {
        s
    }
}

/// Builds the OpenAI tool-calling schema for the four review tools.
fn tools() -> Value {
    let f = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {"type": "object", "properties": props, "required": req}
            }
        })
    };
    json!([
        f("read_file", "Read the full contents of a file at the PR head SHA.",
          json!({"path": {"type": "string", "description": "Path relative to the repo root."}}),
          &["path"]),
        f("list_files", "List entries in a directory at the PR head SHA.",
          json!({"dir": {"type": "string", "description": "Directory; \"\" is the root."}}),
          &["dir"]),
        f("add_comment",
          "Report one issue. Use an exact line number from the new file. Call once per issue.",
          json!({
              "path": {"type": "string", "description": "Path, exactly as it appears in the diff."},
              "line": {"type": "integer", "description": "Line number in the new file."},
              "severity": {"type": "string", "enum": ["high", "medium", "low"],
                           "description": "How serious the issue is."},
              "body": {"type": "string", "description": "Short explanation of the problem."}
          }),
          &["path", "line", "body"]),
        f("finish", "Finish the review and return a short summary.",
          json!({"summary": {"type": "string", "description": "Short summary of the review."}}),
          &["summary"]),
    ])
}

#[cfg(test)]
mod tests {
    use super::{json, new_lines};

    #[test]
    fn parses_hunks_and_skips_removed_lines() {
        let patch = concat!(
            "diff --git a/x.rs b/x.rs\n",
            "index 111..222 100644\n",
            "--- a/x.rs\n",
            "+++ b/x.rs\n",
            "@@ -10,6 +12,7 @@ fn f() {\n",
            " ctx\n",
            "+added\n",
            "-deleted\n",
            "\n",
            "@@ -100,2 +103,3 @@\n",
            " a\n",
            " b\n",
            " c\n",
            "\\ No newline at end of file\n",
        );
        assert_eq!(new_lines(patch), vec![12, 13, 14, 103, 104, 105]);
    }

    #[test]
    fn handles_single_line_and_creation_hunks() {
        assert_eq!(new_lines("@@ -1 +5 @@\n a\n"), vec![5]);
        assert_eq!(new_lines("@@ -0,0 +1 @@\n+a\n"), vec![1]);
        assert_eq!(new_lines("no hunks here"), Vec::<u32>::new());
    }

    /// The tool schema is built programmatically, so pin its wire shape: the
    /// API rejects a malformed `parameters` block outright.
    #[test]
    fn tool_schema_is_well_formed() {
        let v = super::tools();
        let names: Vec<&str> = v
            .as_array()
            .expect("tools must be an array")
            .iter()
            .map(|t| t["function"]["name"].as_str().expect("name must be a string"))
            .collect();
        assert_eq!(names, ["read_file", "list_files", "add_comment", "finish"]);

        for t in v.as_array().expect("array") {
            assert_eq!(t["type"], "function");
            assert!(t["function"]["description"].is_string());
            let params = &t["function"]["parameters"];
            assert_eq!(params["type"], "object");
            assert!(params["properties"].is_object());
            // Every declared property needs a JSON Schema type and a description.
            for (key, spec) in params["properties"].as_object().expect("object") {
                assert!(spec["type"].is_string(), "{key} has no type");
                assert!(spec["description"].is_string(), "{key} has no description");
            }
            // `required` may only name properties that actually exist. The reverse
            // need not hold: `severity` is deliberately optional and falls back
            // to "medium" when the model omits it.
            for r in params["required"].as_array().expect("required array") {
                let r = r.as_str().expect("required entry must be a string");
                assert!(params["properties"].get(r).is_some(), "{r} is required but undeclared");
            }
        }

        let add = &v[2]["function"]["parameters"];
        assert_eq!(add["properties"]["line"]["type"], "integer");
        assert_eq!(add["properties"]["severity"]["enum"], json!(["high", "medium", "low"]));
    }
}
