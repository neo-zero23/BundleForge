//! BundleForge core — tipos compartidos + JSON manual (sin serde, cero deps).

pub mod builders;
pub mod cache;
pub mod config;
pub mod container;
pub mod detectors;
pub mod install;
pub mod remote;
pub mod tester;

/// Escapa un string para JSON (entre comillas).
pub fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub path: String,
    pub stack: String,
    pub name: String,
    pub version: String,
}

impl ProjectInfo {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"path\":{},\"stack\":{},\"name\":{},\"version\":{}}}",
            jstr(&self.path),
            jstr(&self.stack),
            jstr(&self.name),
            jstr(&self.version)
        )
    }
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub format: String,
    pub state: String, // SAFE | MEH | BLOCKED
    pub reason: String,
    pub test_type: String, // quick | normal | package
    pub success: Option<bool>,
    pub error_log: Option<String>,
    pub command_output: Option<String>,
    pub artifact_path: Option<String>, // final kept file (package mode only)
}

impl TestResult {
    pub fn to_json(&self) -> String {
        let opt = |v: &Option<String>| match v {
            Some(s) => jstr(s),
            None => "null".to_string(),
        };
        let ok = |v: Option<bool>| match v {
            Some(true) => "true".to_string(),
            Some(false) => "false".to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"format\":{},\"state\":{},\"reason\":{},\"test_type\":{},\"success\":{},\"error_log\":{},\"command_output\":{},\"artifact_path\":{}}}",
            jstr(&self.format),
            jstr(&self.state),
            jstr(&self.reason),
            jstr(&self.test_type),
            ok(self.success),
            opt(&self.error_log),
            opt(&self.command_output),
            opt(&self.artifact_path)
        )
    }
}

#[derive(Debug, Clone)]
pub struct PackageResult {
    pub format: String,
    pub path: String,
    pub size: u64,
    pub success: bool,
}

impl PackageResult {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"format\":{},\"path\":{},\"size\":{},\"success\":{}}}",
            jstr(&self.format),
            jstr(&self.path),
            self.size,
            self.success
        )
    }
}

/// Une una lista de JSONs en array.
pub fn json_array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}
