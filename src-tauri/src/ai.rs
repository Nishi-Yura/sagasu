//! Claude API calls (raw HTTPS; Rust has no official SDK).
//! Only file names and metadata are sent — never file contents.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::organize::{FileInfo, Plan, PlannedMove};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Files sent to the model in one organize request.
pub const AI_MAX_FILES: usize = 1500;

/// Models that accept server-side refusal fallbacks (`fallbacks: "default"`).
fn supports_fallbacks(model: &str) -> bool {
    matches!(model, "claude-opus-5-5" | "claude-opus-5" | "claude-fable-5-1" | "claude-sonnet-5-5")
}

fn call(key: &str, model: &str, effort: &str, system: &str, user: &str, schema: Value, max_tokens: u32) -> Result<Value> {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{"role": "user", "content": user}],
        "output_config": {"effort": effort, "format": {"type": "json_schema", "schema": schema}},
    });
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
    let mut req = client
        .post(API_URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");
    if supports_fallbacks(model) {
        body["fallbacks"] = json!("default");
        req = req.header("anthropic-beta", FALLBACK_BETA);
    }
    let resp = req.json(&body).send().context("Claude API に接続できませんでした。ネットワークを確認してください。")?;
    let status = resp.status();
    let v: Value = resp.json().context("Claude API の応答を読めませんでした")?;
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or("不明なエラー");
        bail!(match status.as_u16() {
            401 => "APIキーが正しくありません。設定で確認してください。".to_string(),
            403 => format!("このAPIキーでは利用できません: {msg}"),
            404 => format!("モデルが見つかりません（{model}）。設定のモデル名を確認してください。"),
            429 => "利用上限に達しました。少し待ってから再度お試しください。".to_string(),
            500..=599 => "Claude API が混み合っています。少し待ってから再度お試しください。".to_string(),
            _ => format!("Claude API エラー ({status}): {msg}"),
        });
    }
    match v["stop_reason"].as_str() {
        Some("refusal") => bail!("AI がこの依頼に応じませんでした。指示の言い方を変えてお試しください。"),
        Some("max_tokens") => bail!("AI の回答が長すぎて途中で切れました。対象のファイル数を減らしてお試しください。"),
        _ => {}
    }
    let text = v["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .ok_or_else(|| anyhow!("AI の応答にテキストがありません"))?;
    serde_json::from_str(text).context("AI の応答を解析できませんでした")
}

pub fn test_key(key: &str, model: &str) -> Result<()> {
    let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"], "additionalProperties": false});
    call(key, model, "low", "Reply with {\"ok\": true}.", "ping", schema, 2000)?;
    Ok(())
}

// ---------------- natural-language search ----------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiSearch {
    /// Words likely to appear in the file or folder name.
    pub name_keywords: Vec<String>,
    /// Words likely to appear inside the document.
    pub content_keywords: Vec<String>,
    pub extensions: Vec<String>,
    /// YYYY-MM-DD or "".
    pub modified_after: String,
    pub modified_before: String,
    pub folder_hints: Vec<String>,
    /// "any" | "file" | "folder"
    pub kind: String,
    /// One short Japanese sentence describing how the request was interpreted.
    pub explanation: String,
}

pub fn parse_search(key: &str, model: &str, text: &str) -> Result<AiSearch> {
    let today = Local::now().format("%Y-%m-%d (%a)").to_string();
    let system = format!(
        "You turn a Japanese (or English) description of a file someone is looking for into search filters for a desktop file search tool on Windows.\n\
         Today is {today}. Resolve relative dates (先月, 去年の夏, 3日前) into an inclusive-exclusive date range; use \"\" when no date is implied.\n\
         name_keywords: short words probably in the file name (both Japanese and common English/romaji variants are fine, at most 4).\n\
         content_keywords: distinctive words probably written inside the document (company names, topics), at most 4.\n\
         extensions: lowercase without dots, inferred from words like PDF, エクセル, 写真, パワポ; empty when unknown.\n\
         folder_hints: folder names the user mentions. kind: any, file or folder.\n\
         Prefer fewer, more distinctive keywords; never include generic words such as ファイル, データ, 資料."
    );
    let schema = json!({
        "type": "object",
        "properties": {
            "name_keywords": {"type": "array", "items": {"type": "string"}},
            "content_keywords": {"type": "array", "items": {"type": "string"}},
            "extensions": {"type": "array", "items": {"type": "string"}},
            "modified_after": {"type": "string"},
            "modified_before": {"type": "string"},
            "folder_hints": {"type": "array", "items": {"type": "string"}},
            "kind": {"type": "string", "enum": ["any", "file", "folder"]},
            "explanation": {"type": "string"}
        },
        "required": ["name_keywords", "content_keywords", "extensions", "modified_after", "modified_before", "folder_hints", "kind", "explanation"],
        "additionalProperties": false
    });
    let v = call(key, model, "low", &system, text, schema, 4000)?;
    Ok(serde_json::from_value(v)?)
}

// ---------------- organize ----------------

#[derive(Debug, Deserialize)]
struct AiGroup {
    folder: String,
    ids: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct AiRename {
    id: u32,
    new_name: String,
}

#[derive(Debug, Deserialize)]
struct AiPlan {
    summary: String,
    groups: Vec<AiGroup>,
    renames: Vec<AiRename>,
}

fn date(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n}B"),
        1024..1_048_576 => format!("{}KB", n / 1024),
        _ => format!("{:.1}MB", n as f64 / 1_048_576.0),
    }
}

pub fn plan_organize(key: &str, model: &str, root: &str, instruction: &str, files: &[FileInfo]) -> Result<Plan> {
    let shown = &files[..files.len().min(AI_MAX_FILES)];
    let mut listing = String::with_capacity(shown.len() * 48);
    for (i, f) in shown.iter().enumerate() {
        listing.push_str(&format!("{i}\t{}\t{}\t{}\n", f.rel, human_size(f.size), date(f.mtime)));
    }
    let system = "You organize files inside ONE folder on a Windows PC according to the user's instruction (usually Japanese).\n\
        You receive a numbered list: id<TAB>relative path<TAB>size<TAB>modified date.\n\
        Return groups: each group has a destination folder path RELATIVE to the root (use \\ between levels, no drive letters, no ..) and the ids of files that go there.\n\
        Put a file in at most one group. Leave files that are already well placed, or that the instruction does not cover, out of every group.\n\
        Use renames only when the instruction asks to rename files; keep the extension unchanged.\n\
        Folder names should be short and in the user's language. Never invent ids.\n\
        summary: 1-3 short Japanese sentences describing the new structure.";
    let user = format!(
        "整理する指示: {instruction}\nルートフォルダ: {root}\nファイル数: {}{}\n\n{listing}",
        files.len(),
        if files.len() > shown.len() { format!("（先頭{}件のみ表示）", shown.len()) } else { String::new() }
    );
    let schema = json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "groups": {"type": "array", "items": {
                "type": "object",
                "properties": {"folder": {"type": "string"}, "ids": {"type": "array", "items": {"type": "integer"}}},
                "required": ["folder", "ids"], "additionalProperties": false
            }},
            "renames": {"type": "array", "items": {
                "type": "object",
                "properties": {"id": {"type": "integer"}, "new_name": {"type": "string"}},
                "required": ["id", "new_name"], "additionalProperties": false
            }}
        },
        "required": ["summary", "groups", "renames"],
        "additionalProperties": false
    });
    let v = call(key, model, "medium", system, &user, schema, 16000)?;
    let ai: AiPlan = serde_json::from_value(v)?;
    Ok(to_plan(root, instruction, files.len(), shown, ai))
}

fn to_plan(root: &str, instruction: &str, total: usize, files: &[FileInfo], ai: AiPlan) -> Plan {
    let renames: HashMap<u32, String> = ai.renames.into_iter().map(|r| (r.id, r.new_name)).collect();
    let mut dest: HashMap<u32, String> = HashMap::new();
    for g in ai.groups {
        let folder = g.folder.trim().trim_matches(['\\', '/']).to_string();
        for id in g.ids {
            dest.entry(id).or_insert_with(|| folder.clone());
        }
    }
    let mut moves = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let id = i as u32;
        let name = renames.get(&id).cloned().unwrap_or_else(|| f.name.clone());
        let folder = match dest.get(&id) {
            Some(d) => d.clone(),
            None if renames.contains_key(&id) => f.rel.rsplit_once('\\').map(|(d, _)| d.to_string()).unwrap_or_default(),
            None => continue,
        };
        let to = if folder.is_empty() { name } else { format!("{folder}\\{name}") };
        moves.push(PlannedMove { from: f.rel.clone(), to });
    }
    let plan = Plan { root: root.into(), instruction: instruction.into(), summary: ai.summary, moves, notes: Vec::new(), total_files: total };
    crate::organize::validate(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_conversion() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.pdf"), "x").unwrap();
        std::fs::write(d.path().join("b.jpg"), "x").unwrap();
        std::fs::write(d.path().join("c.txt"), "x").unwrap();
        let files: Vec<FileInfo> = ["a.pdf", "b.jpg", "c.txt"]
            .iter()
            .map(|n| FileInfo { rel: n.to_string(), name: n.to_string(), ext: String::new(), size: 1, mtime: 0 })
            .collect();
        let ai = AiPlan {
            summary: "s".into(),
            groups: vec![AiGroup { folder: "書類\\".into(), ids: vec![0, 99] }, AiGroup { folder: "写真".into(), ids: vec![1, 0] }],
            renames: vec![AiRename { id: 2, new_name: "メモ.txt".into() }],
        };
        let p = to_plan(&d.path().to_string_lossy(), "x", 3, &files, ai);
        let m: HashMap<_, _> = p.moves.iter().map(|m| (m.from.as_str(), m.to.as_str())).collect();
        assert_eq!(m["a.pdf"], "書類\\a.pdf");
        assert_eq!(m["b.jpg"], "写真\\b.jpg");
        assert_eq!(m["c.txt"], "メモ.txt");
        assert_eq!(p.moves.len(), 3);
    }
}
