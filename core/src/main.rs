//! bundleforge-core: JSON-speaking CLI. UIs are dumb clients.
//! Usage:
//!   bundleforge-core scan <path>
//!   bundleforge-core test [--quick] --formats deb,rpm
//!   bundleforge-core package --formats deb --out dist/

use bundleforge_core::{builders, cache, detectors, install, json_array, tester, ProjectInfo};
use bundleforge_core::jstr;

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  bundleforge-core scan <path>");
    eprintln!("  bundleforge-core test [--quick] --formats deb,rpm,appimage");
    eprintln!("  bundleforge-core package --formats deb --out dist/");
    std::process::exit(2);
}

fn get_flag(args: &[String], name: &str) -> Option<String> {
    // Acepta --formats a,b  y  --formats=a,b
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "scan" => {
            let path = args.get(1).cloned().unwrap_or_else(|| usage());
            let (stack, name, version) = detectors::detect_stack(std::path::Path::new(&path));
            let info = ProjectInfo {
                path,
                stack,
                name,
                version,
            };
            println!("{}", info.to_json());
        }
        "test" => {
            let path = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
            // Formas: test --formats X | test <path> --formats X
            let (path, rest) = if path.starts_with("--") {
                (".".to_string(), args[1..].to_vec())
            } else {
                (path, args[2..].to_vec())
            };
            let formats: Vec<String> = get_flag(&rest, "--formats")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if formats.is_empty() {
                usage();
            }
            let quick = rest.iter().any(|a| a == "--quick");
            let res = if quick {
                tester::quick_test(&path, &formats)
            } else {
                tester::normal_test(&path, &formats)
            };
            let items: Vec<String> = res.iter().map(|r| r.to_json()).collect();
            println!("{}", json_array(&items));
        }
        "package" => {
            // Real packaging: build + keep artifact under <out>/packages/.
            // Usage: package [<path>] --formats pacman [--out dist/]
            let first = args.get(1).cloned().unwrap_or_default();
            let (path, rest) = if first.is_empty() || first.starts_with("--") {
                (".".to_string(), args[1..].to_vec())
            } else {
                (first, args[2..].to_vec())
            };
            let formats: Vec<String> = get_flag(&rest, "--formats")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let out = get_flag(&rest, "--out").unwrap_or_else(|| "dist/".to_string());
            if formats.is_empty() {
                usage();
            }
            let (stack, name, version) =
                detectors::detect_stack(std::path::Path::new(&path));
            let _ = stack;
            let ctl = builders::BuildCtl::fresh();
            let out_base = std::path::Path::new(&out);
            let items: Vec<String> = formats
                .iter()
                .map(|f| builders::package_format(&path, &name, &version, f, out_base, &ctl).to_json())
                .collect();
            println!("{}", json_array(&items));
        }
        "cache-read" => {
            let path = args.get(1).cloned().unwrap_or_else(|| usage());
            match cache::read(&path) {
                Some(c) => println!("{c}"),
                None => println!("null"),
            }
        }
        "cache-write" => {
            let path = args.get(1).cloned().unwrap_or_else(|| usage());
            let content = args.get(2).cloned().unwrap_or_default();
            println!("{}", cache::write(&path, &content));
        }
        "install-hint" => {
            let format = args.get(1).cloned().unwrap_or_else(|| usage());
            match install::recipe_for(&format) {
                Some(r) => {
                    let pkgs = match &r.kind {
                        install::RecipeKind::Packages { manager, packages, .. } => {
                            format!("{manager}: {}", packages.join(" "))
                        }
                        install::RecipeKind::Download { url, .. } => format!("download {url}"),
                        install::RecipeKind::SourceBuild { manager, git_url, .. } => {
                            format!("source-build via {manager}: {git_url}")
                        }
                        install::RecipeKind::AurBuild { helper, package, .. } => {
                            format!("aur-build via {helper}: {package}")
                        }
                    };
                    println!(
                        "{{\"format\":{},\"via\":{},\"command\":{},\"note\":{}}}",
                        jstr(&format),
                        jstr(&pkgs),
                        jstr(&r.command),
                        match &r.note {
                            Some(n) => jstr(n),
                            None => "null".to_string(),
                        }
                    );
                }
                None => println!("{{\"format\":{},\"via\":null}}", jstr(&format)),
            }
        }
        "install" => {
            let format = args.get(1).cloned().unwrap_or_else(|| usage());
            let ctl = builders::BuildCtl::fresh();
            match install::install_format(&format, &ctl) {
                Ok(log) => println!("{{\"format\":{},\"ok\":true,\"log\":{}}}", jstr(&format), jstr(&log)),
                Err(e) => println!("{{\"format\":{},\"ok\":false,\"error\":{}}}", jstr(&format), jstr(&e)),
            }
        }
        _ => usage(),
    }
}
