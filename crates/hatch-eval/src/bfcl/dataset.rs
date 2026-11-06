//! 加载 BFCL v4 数据集（非 live 单轮类别）。
//!
//! 数据文件是 JSONL 内容（每行一个题目），文件名为 `BFCL_v4_<类别>.json`，
//! 位于传入的数据目录下。题目结构：
//!
//! ```json
//! {"id": "simple_python_0",
//!  "question": [[{"role": "user", "content": "..."}]],
//!  "function": [{"name": "calculate_triangle_area", "description": "...",
//!                "parameters": {"type": "object", "properties": {...}}}],
//!  "ground_truth": null}
//! ```
//!
//! 注意：数据文件里的 `ground_truth` 为空，评分用的期望调用不在这里，
//! 由官方 `bfcl evaluate` 从它自己的数据里取。本模块只负责读题和函数定义。

use hatch_core::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Phase A 支持的三个类别。
pub const SUPPORTED_CATEGORIES: [&str; 3] = ["simple_python", "multiple", "irrelevance"];

pub fn is_supported_category(category: &str) -> bool {
    SUPPORTED_CATEGORIES.contains(&category)
}

/// 一个 BFCL 函数定义（原样照搬进 stub Tool 的 spec）。
#[derive(Clone, Debug, PartialEq)]
pub struct BfclFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// 一道单轮题：第一轮的第一条 user 消息 + 该题的全部候选函数。
#[derive(Clone, Debug, PartialEq)]
pub struct BfclEntry {
    pub id: String,
    pub question: String,
    pub functions: Vec<BfclFunction>,
}

#[derive(Deserialize)]
struct RawEntry {
    id: String,
    question: Vec<Vec<RawMessage>>,
    function: Vec<RawFunction>,
}

#[derive(Deserialize)]
struct RawMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct RawFunction {
    name: String,
    description: String,
    parameters: Value,
}

/// OpenAI 兼容的函数调用不允许名字里带点；模型看到的是下划线名。
pub fn to_underscored(name: &str) -> String {
    name.replace('.', "_")
}

/// 下划线名 → 原始点名的映射表（每题一张；导出结果时还原）。
#[derive(Clone, Debug, Default)]
pub struct NameMap {
    map: HashMap<String, String>,
}

impl NameMap {
    /// 两道同题函数下划线化后撞名时返回 Err，调用方应跳过该题。
    pub fn build(functions: &[BfclFunction]) -> Result<Self> {
        let mut map = HashMap::new();
        for f in functions {
            let under = to_underscored(&f.name);
            if let Some(prev) = map.get(&under) {
                if prev != &f.name {
                    return Err(Error::Invalid(format!(
                        "函数名下划线化后冲突：`{prev}` 与 `{}` 都映射到 `{under}`",
                        f.name
                    )));
                }
            }
            map.insert(under, f.name.clone());
        }
        Ok(Self { map })
    }

    /// 不在表里的名字（模型幻觉出的工具名）原样返回，评分时会判错。
    pub fn restore<'a>(&'a self, underscored: &'a str) -> &'a str {
        self.map
            .get(underscored)
            .map(String::as_str)
            .unwrap_or(underscored)
    }
}

fn parse_entry(line: &str) -> Result<BfclEntry> {
    let raw: RawEntry = serde_json::from_str(line)
        .map_err(|e| Error::Invalid(format!("BFCL 题目解析失败：{e}（行：{line:.200}）")))?;
    let question = raw
        .question
        .first()
        .and_then(|turn| turn.iter().find(|m| m.role == "user"))
        .map(|m| m.content.clone())
        .ok_or_else(|| Error::Invalid(format!("题目 {} 没有 user 消息", raw.id)))?;
    Ok(BfclEntry {
        id: raw.id,
        question,
        functions: raw
            .function
            .into_iter()
            .map(|f| BfclFunction {
                name: f.name,
                description: f.description,
                parameters: f.parameters,
            })
            .collect(),
    })
}

/// 解析一串 JSONL 文本（纯函数，便于测试）。
pub fn parse_entries(text: &str) -> Result<Vec<BfclEntry>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse_entry)
        .collect()
}

/// 加载一个类别。`wanted` 为 Some 时只保留这些 id（保持文件顺序）；
/// `limit` 为 Some 时每类别最多取这么多题（先过滤 id 再截断）。
pub fn load_category(
    data_dir: &Path,
    category: &str,
    wanted: Option<&HashSet<String>>,
    limit: Option<usize>,
) -> Result<Vec<BfclEntry>> {
    if !is_supported_category(category) {
        return Err(Error::Invalid(format!(
            "不支持的 BFCL 类别：{category}（Phase A 只支持 {}）",
            SUPPORTED_CATEGORIES.join("、")
        )));
    }
    let path = data_dir.join(format!("BFCL_v4_{category}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::Invalid(format!("读 BFCL 数据失败 {}：{e}", path.display())))?;
    let mut entries = parse_entries(&text)?;
    if let Some(wanted) = wanted {
        entries.retain(|e| wanted.contains(&e.id));
    }
    if let Some(n) = limit {
        entries.truncate(n);
    }
    Ok(entries)
}

/// 解析 BFCL 的 test_case_ids_to_generate.json：`{类别: [id, ...]}`。
pub fn load_ids_file(path: &Path) -> Result<HashMap<String, Vec<String>>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("读 ids 文件失败 {}：{e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Invalid(format!("ids 文件解析失败 {}：{e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{"id": "simple_python_0", "question": [[{"role": "user", "content": "Find the area."}]], "function": [{"name": "calculate_triangle_area", "description": "Area.", "parameters": {"type": "object", "properties": {"base": {"type": "number"}}}}], "ground_truth": null}
{"id": "multiple_0", "question": [[{"role": "user", "content": "Triangle props?"}]], "function": [{"name": "triangle_properties.get", "description": "Props.", "parameters": {"type": "object", "properties": {}}}, {"name": "triangle_properties.calculate", "description": "Calc.", "parameters": {"type": "object", "properties": {}}}], "ground_truth": null}
{"id": "irrelevance_0", "question": [[{"role": "user", "content": "Area with base 10."}]], "function": [{"name": "determine_body_mass_index", "description": "BMI.", "parameters": {"type": "object", "properties": {}}}], "ground_truth": null}"#;

    #[test]
    fn parses_three_entries() {
        let entries = parse_entries(SAMPLE).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "simple_python_0");
        assert_eq!(entries[0].question, "Find the area.");
        assert_eq!(entries[0].functions.len(), 1);
        assert_eq!(entries[0].functions[0].name, "calculate_triangle_area");
        assert_eq!(entries[1].functions.len(), 2);
        assert_eq!(entries[1].functions[0].name, "triangle_properties.get");
        assert_eq!(entries[2].functions[0].name, "determine_body_mass_index");
    }

    #[test]
    fn missing_user_message_is_an_error() {
        let bad = r#"{"id": "x", "question": [[{"role": "assistant", "content": "hi"}]], "function": [], "ground_truth": null}"#;
        assert!(parse_entries(bad).is_err());
    }

    #[test]
    fn underscore_mapping_round_trips() {
        assert_eq!(
            to_underscored("triangle_properties.get"),
            "triangle_properties_get"
        );
        assert_eq!(
            to_underscored("calculate_triangle_area"),
            "calculate_triangle_area"
        );
        let funcs = vec![
            BfclFunction {
                name: "triangle_properties.get".into(),
                description: String::new(),
                parameters: json!({}),
            },
            BfclFunction {
                name: "math_factorial".into(),
                description: String::new(),
                parameters: json!({}),
            },
        ];
        let map = NameMap::build(&funcs).unwrap();
        assert_eq!(
            map.restore("triangle_properties_get"),
            "triangle_properties.get"
        );
        assert_eq!(map.restore("math_factorial"), "math_factorial");
        // 模型幻觉出的名字原样返回。
        assert_eq!(map.restore("not_a_real_tool"), "not_a_real_tool");
    }

    #[test]
    fn underscore_collision_is_an_error() {
        let funcs = vec![
            BfclFunction {
                name: "a.b_c".into(),
                description: String::new(),
                parameters: json!({}),
            },
            BfclFunction {
                name: "a_b.c".into(),
                description: String::new(),
                parameters: json!({}),
            },
        ];
        assert!(NameMap::build(&funcs).is_err());
    }

    #[test]
    fn load_category_filters_ids_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("BFCL_v4_simple_python.json"), SAMPLE).unwrap();
        let wanted: HashSet<String> = ["simple_python_0".to_string()].into_iter().collect();
        let entries = load_category(dir.path(), "simple_python", Some(&wanted), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "simple_python_0");
        let limited = load_category(dir.path(), "simple_python", None, Some(1)).unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn unsupported_category_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_category(dir.path(), "multi_turn_base", None, None).is_err());
    }

    #[test]
    fn load_ids_file_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.json");
        std::fs::write(
            &path,
            r#"{"simple_python": ["simple_python_0", "simple_python_1"], "multiple": []}"#,
        )
        .unwrap();
        let ids = load_ids_file(&path).unwrap();
        assert_eq!(ids["simple_python"].len(), 2);
        assert!(ids["multiple"].is_empty());
    }
}
