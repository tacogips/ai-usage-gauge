use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StaticAssetResolver {
    root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl StaticAssetResolver {
    /// Resolve asset root:
    /// 1. Explicit `--assets` directory (if provided, must exist with `index.html`)
    /// 2. Adjacent Homebrew formula layout (`../share/ai-usage-gauge/web` relative to exe)
    /// 3. Adjacent macOS bundle layout (`../Resources/Web` relative to exe)
    pub fn resolve(explicit: Option<&Path>, exe_path: Option<&Path>) -> Self {
        if let Some(dir) = explicit {
            if dir.join("index.html").is_file() {
                return Self {
                    root: Some(fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())),
                };
            }
            return Self { root: None };
        }

        if let Some(exe) = exe_path {
            let resolved = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
            if let Some(bin_dir) = resolved.parent() {
                if let Some(prefix) = bin_dir.parent() {
                    let formula = prefix.join("share/ai-usage-gauge/web");
                    if formula.join("index.html").is_file() {
                        return Self {
                            root: Some(fs::canonicalize(&formula).unwrap_or(formula)),
                        };
                    }
                    let legacy_formula = prefix.join("share/ai-usage-gauge/web");
                    if legacy_formula.join("index.html").is_file() {
                        return Self {
                            root: Some(
                                fs::canonicalize(&legacy_formula).unwrap_or(legacy_formula),
                            ),
                        };
                    }
                    let cask = prefix.join("Resources/Web");
                    if cask.join("index.html").is_file() {
                        return Self {
                            root: Some(fs::canonicalize(&cask).unwrap_or(cask)),
                        };
                    }
                }
            }
        }

        Self { root: None }
    }

    pub fn from_root(root: Option<PathBuf>) -> Self {
        Self { root }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn serve(&self, request_path: &str) -> AssetResponse {
        let Some(root) = &self.root else {
            return AssetResponse {
                status: 503,
                content_type: "application/json; charset=utf-8",
                body:
                    br#"{"error":"assets_missing","message":"Dashboard web assets are missing."}"#
                        .to_vec(),
            };
        };

        let raw = request_path.split('?').next().unwrap_or("/");
        let trimmed = raw.trim_start_matches('/');
        let rel = Path::new(trimmed);
        for comp in rel.components() {
            if matches!(
                comp,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) {
                return AssetResponse {
                    status: 400,
                    content_type: "application/json; charset=utf-8",
                    body: br#"{"error":"invalid_path","message":"Invalid asset path."}"#.to_vec(),
                };
            }
        }

        let candidate = if trimmed.is_empty() {
            root.join("index.html")
        } else {
            root.join(rel)
        };

        if candidate.is_file() {
            if let Ok(canon) = fs::canonicalize(&candidate) {
                if canon.starts_with(root) {
                    if let Ok(bytes) = fs::read(&canon) {
                        return AssetResponse {
                            status: 200,
                            content_type: mime_type(&canon),
                            body: bytes,
                        };
                    }
                }
            }
        }

        // SPA fallback for navigation paths without a file extension (never list directories).
        if !trimmed.contains('.') || trimmed.is_empty() {
            let index = root.join("index.html");
            if let Ok(bytes) = fs::read(&index) {
                return AssetResponse {
                    status: 200,
                    content_type: "text/html; charset=utf-8",
                    body: bytes,
                };
            }
        }

        AssetResponse {
            status: 404,
            content_type: "application/json; charset=utf-8",
            body: br#"{"error":"not_found","message":"Asset not found."}"#.to_vec(),
        }
    }
}

fn mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
