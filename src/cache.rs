//! Small files claude++ keeps under the XDG cache folder: the model and command lists, and notes of what was said.

use super::*;

pub(crate) fn cache_path(name: &str) -> Option<std::path::PathBuf> {
    config::cache_dir().map(|d| d.join(name))
}

/// A cached JSON array, one `f` per element (empty when the file is missing or not an array).
pub(crate) fn read_list<T>(path: &std::path::Path, f: impl Fn(&serde_json::Value) -> T) -> Vec<T> {
    let list: Option<Vec<serde_json::Value>> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
    list.unwrap_or_default().iter().map(f).collect()
}

pub(crate) fn write_list(p: &std::path::Path, items: Vec<serde_json::Value>) {
    let _ = p.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(p, serde_json::Value::Array(items).to_string());
}

pub(crate) fn text(v: &serde_json::Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

pub(crate) fn read_models(path: &std::path::Path) -> Vec<ModelInfo> {
    read_list(path, |m| ModelInfo {
        value: text(m, "value"),
        name: text(m, "name"),
        desc: text(m, "desc"),
        resolved: text(m, "resolved"),
        efforts: m["efforts"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect(),
        fast: m["fast"] == true,
    })
}

pub(crate) fn write_models(p: &std::path::Path, models: &[ModelInfo]) {
    write_list(p, models.iter().map(|m| json!({"value": m.value, "name": m.name, "desc": m.desc, "resolved": m.resolved, "efforts": m.efforts, "fast": m.fast})).collect());
}

pub(crate) fn read_commands(path: &std::path::Path) -> Vec<Cmd> {
    read_list(path, |c| Cmd { name: text(c, "name"), desc: text(c, "desc"), hint: text(c, "hint") })
}

pub(crate) fn write_commands(p: &std::path::Path, cmds: &[Cmd]) {
    write_list(p, cmds.iter().map(|c| json!({"name": c.name, "desc": c.desc, "hint": c.hint})).collect());
}
