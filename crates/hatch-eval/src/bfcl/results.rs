//! 写出 BFCL 格式的结果文件，复用官方 checker 评分。
//!
//! 每类别一个文件：`<out>/non_live/BFCL_v4_<类别>_result.json`，
//! 每行一个 JSON 对象（与模型级 smoke 的结果文件同格式）：
//!
//! ```json
//! {"id": "simple_python_0",
//!  "result": [{"calculate_triangle_area": "{\"base\":10,\"height\":5}"}],
//!  "input_token_count": 174, "output_token_count": 41, "latency": 1.53}
//! ```
//!
//! 没有工具调用时 `result` 是回答文本（字符串），与 irrelevance 类别的
//! 官方格式一致；评分器会按类别口径判定（对 simple/multiple 计失败）。

use hatch_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 一道题的跑分行。
#[derive(Clone, Debug)]
pub struct BfclRow {
    pub id: String,
    /// 工具调用列表 `[{点名: "{JSON 参数}"}]`，或无调用时的回答文本。
    pub result: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_secs: f64,
}

#[derive(Serialize)]
struct ResultLine<'a> {
    id: &'a str,
    result: &'a Value,
    input_token_count: u64,
    output_token_count: u64,
    latency: f64,
}

pub fn write_category_results(out_dir: &Path, category: &str, rows: &[BfclRow]) -> Result<PathBuf> {
    let dir = out_dir.join("non_live");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Invalid(format!("创建结果目录失败 {}：{e}", dir.display())))?;
    let path = dir.join(format!("BFCL_v4_{category}_result.json"));
    let mut text = String::new();
    for row in rows {
        let line = ResultLine {
            id: &row.id,
            result: &row.result,
            input_token_count: row.input_tokens,
            output_token_count: row.output_tokens,
            latency: row.latency_secs,
        };
        text.push_str(&serde_json::to_string(&line)?);
        text.push('\n');
    }
    std::fs::write(&path, text)
        .map_err(|e| Error::Invalid(format!("写结果文件失败 {}：{e}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_bfcl_shaped_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            BfclRow {
                id: "simple_python_0".into(),
                result: json!([{"calculate_triangle_area": "{\"base\":10,\"height\":5}"}]),
                input_tokens: 174,
                output_tokens: 41,
                latency_secs: 1.5,
            },
            BfclRow {
                id: "simple_python_1".into(),
                result: json!("The area is 25."),
                input_tokens: 100,
                output_tokens: 10,
                latency_secs: 0.5,
            },
        ];
        let path = write_category_results(dir.path(), "simple_python", &rows).unwrap();
        assert_eq!(
            path,
            dir.path()
                .join("non_live/BFCL_v4_simple_python_result.json")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], "simple_python_0");
        assert_eq!(
            lines[0]["result"][0]["calculate_triangle_area"],
            "{\"base\":10,\"height\":5}"
        );
        assert_eq!(lines[0]["input_token_count"], 174);
        assert_eq!(lines[0]["output_token_count"], 41);
        assert_eq!(lines[0]["latency"], 1.5);
        // 无调用时 result 是字符串（irrelevance 格式）。
        assert_eq!(lines[1]["result"], "The area is 25.");
    }
}
