//! Embeds the built-in recipe catalog into the binary so the tool is a single
//! self-contained executable with no data-directory assumption.

use std::fs;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml") | Some("yaml")
        ) {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();

    let mut code = String::from("pub static BUILTIN_RECIPES: &[(&str, &str)] = &[\n");
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .expect("recipe under catalog root")
            .to_string_lossy()
            .replace('\\', "/");
        let abs = file.to_string_lossy().replace('\\', "/");
        code.push_str(&format!("    ({:?}, include_str!({:?})),\n", rel, abs));
    }
    code.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("builtin_recipes.rs");
    fs::write(&dest, code).expect("write builtin recipe table");
    println!("cargo:rerun-if-changed=recipes");
}
